use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

use crate::direction::Direction;
use crate::error::{InnerErrorCode, MeowError};
use crate::http_breakpoint::{BreakpointDownload, BreakpointDownloadHttpConfig, BreakpointUpload};
use crate::ids::TaskId;
use crate::inner::sign::calculate_sign_bytes;
use crate::inner::UniqueId;
use crate::pounce_task::PounceTask;
use crate::upload_file::UploadFileSnapshot;
use crate::upload_source::UploadSource;
use reqwest::header::HeaderMap;
use reqwest::Method;

const TRANSFER_OPEN: u8 = 0;
const TRANSFER_COMPLETING: u8 = 1;
const TRANSFER_CANCELING: u8 = 2;
const TRANSFER_PAUSING: u8 = 3;
const TRANSFER_COMPLETING_CANCEL_PENDING: u8 = 4;
const TRANSFER_COMPLETING_PAUSE_PENDING: u8 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopRequestDisposition {
    InterruptNow,
    DeferredUntilRequestReturns,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeferredStop {
    Pause,
    Cancel,
}

/// Shared terminal lifecycle arbitration for every view of one scheduler task.
///
/// The scheduler and worker use this as the linearization point between the
/// final transfer request and pause/cancel. Upload abort idempotence lives in
/// the same shared object so a control-plane task view and the running task
/// can never issue two aborts.
pub(crate) struct TransferLifecycle {
    terminal: AtomicU8,
    abort_started: AtomicBool,
    abort_required: AtomicBool,
}

#[cfg(test)]
mod lifecycle_tests {
    use super::{DeferredStop, StopRequestDisposition, TransferLifecycle};

    #[test]
    fn completion_excludes_pause_and_cancel() {
        let lifecycle = TransferLifecycle::new();
        assert!(lifecycle.begin_completion());
        assert!(!lifecycle.begin_completion(), "completion has one owner");
        assert_eq!(
            lifecycle.begin_pause(),
            StopRequestDisposition::DeferredUntilRequestReturns
        );
        assert_eq!(
            lifecycle.begin_cancel(),
            StopRequestDisposition::DeferredUntilRequestReturns
        );
        assert_eq!(
            lifecycle.finish_incomplete_completion(),
            Some(DeferredStop::Cancel),
            "cancel upgrades a pending pause when the request was not terminal"
        );
    }

    #[test]
    fn accepted_pause_blocks_completion_and_can_upgrade_to_cancel() {
        let lifecycle = TransferLifecycle::new();
        assert_eq!(
            lifecycle.begin_pause(),
            StopRequestDisposition::InterruptNow
        );
        assert!(!lifecycle.begin_completion());
        assert_eq!(
            lifecycle.begin_cancel(),
            StopRequestDisposition::InterruptNow
        );
        assert!(!lifecycle.begin_completion());
    }

    #[test]
    fn quiescent_pause_can_reset_for_resume() {
        let lifecycle = TransferLifecycle::new();
        assert_eq!(
            lifecycle.begin_pause(),
            StopRequestDisposition::InterruptNow
        );
        lifecycle.reset_pause();
        assert!(lifecycle.begin_completion());
    }

    #[test]
    fn provider_abort_is_shared_and_exactly_once() {
        let lifecycle = TransferLifecycle::new();
        assert!(lifecycle.begin_abort());
        assert!(!lifecycle.begin_abort());
    }

    #[test]
    fn observed_remote_completion_overrides_racing_stop_request() {
        for cancel in [false, true] {
            let lifecycle = TransferLifecycle::new();
            let disposition = if cancel {
                lifecycle.begin_cancel()
            } else {
                lifecycle.begin_pause()
            };
            assert_eq!(disposition, StopRequestDisposition::InterruptNow);

            // This models a request that passed its preflight cancellation
            // check, then reported terminal success while the control command
            // raced it. Remote truth wins and later stops become deferred.
            lifecycle.acknowledge_completion();
            assert_eq!(
                lifecycle.begin_cancel(),
                StopRequestDisposition::DeferredUntilRequestReturns
            );
            assert!(lifecycle.finish_incomplete_completion().is_some());
        }
    }
}

impl TransferLifecycle {
    pub(crate) fn new() -> Self {
        Self {
            terminal: AtomicU8::new(TRANSFER_OPEN),
            abort_started: AtomicBool::new(false),
            abort_required: AtomicBool::new(false),
        }
    }

    pub(crate) fn begin_completion(&self) -> bool {
        self.terminal
            .compare_exchange(
                TRANSFER_OPEN,
                TRANSFER_COMPLETING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Records an observed terminal success from an in-flight provider
    /// request. Unlike the predictive claim made before a known last request,
    /// this is a fact reported by the remote side and therefore wins over a
    /// pause/cancel that raced the request: publishing Canceled and then
    /// aborting an already-complete object would be both false and unsafe.
    pub(crate) fn acknowledge_completion(&self) {
        loop {
            let current = self.terminal.load(Ordering::Acquire);
            if current == TRANSFER_COMPLETING
                || self
                    .terminal
                    .compare_exchange(
                        current,
                        TRANSFER_COMPLETING,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
            {
                return;
            }
        }
    }

    pub(crate) fn begin_pause(&self) -> StopRequestDisposition {
        loop {
            match self.terminal.load(Ordering::Acquire) {
                TRANSFER_OPEN => {
                    if self
                        .terminal
                        .compare_exchange(
                            TRANSFER_OPEN,
                            TRANSFER_PAUSING,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return StopRequestDisposition::InterruptNow;
                    }
                }
                TRANSFER_COMPLETING => {
                    if self
                        .terminal
                        .compare_exchange(
                            TRANSFER_COMPLETING,
                            TRANSFER_COMPLETING_PAUSE_PENDING,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return StopRequestDisposition::DeferredUntilRequestReturns;
                    }
                }
                TRANSFER_COMPLETING_PAUSE_PENDING | TRANSFER_COMPLETING_CANCEL_PENDING => {
                    return StopRequestDisposition::DeferredUntilRequestReturns;
                }
                TRANSFER_PAUSING | TRANSFER_CANCELING => {
                    return StopRequestDisposition::InterruptNow;
                }
                _ => return StopRequestDisposition::DeferredUntilRequestReturns,
            }
        }
    }

    pub(crate) fn begin_cancel(&self) -> StopRequestDisposition {
        loop {
            match self.terminal.load(Ordering::Acquire) {
                TRANSFER_CANCELING => return StopRequestDisposition::InterruptNow,
                TRANSFER_COMPLETING => {
                    if self
                        .terminal
                        .compare_exchange(
                            TRANSFER_COMPLETING,
                            TRANSFER_COMPLETING_CANCEL_PENDING,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return StopRequestDisposition::DeferredUntilRequestReturns;
                    }
                }
                TRANSFER_COMPLETING_PAUSE_PENDING => {
                    if self
                        .terminal
                        .compare_exchange(
                            TRANSFER_COMPLETING_PAUSE_PENDING,
                            TRANSFER_COMPLETING_CANCEL_PENDING,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return StopRequestDisposition::DeferredUntilRequestReturns;
                    }
                }
                TRANSFER_COMPLETING_CANCEL_PENDING => {
                    return StopRequestDisposition::DeferredUntilRequestReturns;
                }
                current @ (TRANSFER_OPEN | TRANSFER_PAUSING) => {
                    if self
                        .terminal
                        .compare_exchange(
                            current,
                            TRANSFER_CANCELING,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return StopRequestDisposition::InterruptNow;
                    }
                }
                _ => return StopRequestDisposition::DeferredUntilRequestReturns,
            }
        }
    }

    pub(crate) fn reset_pause(&self) {
        let _ = self.terminal.compare_exchange(
            TRANSFER_PAUSING,
            TRANSFER_OPEN,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    pub(crate) fn finish_incomplete_completion(&self) -> Option<DeferredStop> {
        loop {
            let (current, next, stop) = match self.terminal.load(Ordering::Acquire) {
                TRANSFER_COMPLETING => (TRANSFER_COMPLETING, TRANSFER_OPEN, None),
                TRANSFER_COMPLETING_PAUSE_PENDING => (
                    TRANSFER_COMPLETING_PAUSE_PENDING,
                    TRANSFER_PAUSING,
                    Some(DeferredStop::Pause),
                ),
                TRANSFER_COMPLETING_CANCEL_PENDING => (
                    TRANSFER_COMPLETING_CANCEL_PENDING,
                    TRANSFER_CANCELING,
                    Some(DeferredStop::Cancel),
                ),
                _ => return None,
            };
            if self
                .terminal
                .compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return stop;
            }
        }
    }

    pub(crate) fn begin_abort(&self) -> bool {
        self.abort_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(crate) fn require_abort(&self) {
        self.abort_required.store(true, Ordering::Release);
    }

    pub(crate) fn abort_required(&self) -> bool {
        self.abort_required.load(Ordering::Acquire)
    }
}

/// 调度与 [`crate::transfer_executor_trait::TransferTrait`] 视图的内部任务；不对外暴露构造。
#[derive(Clone)]
pub(crate) struct InnerTask {
    task_id: TaskId,
    file_sign: Arc<str>,
    file_name: Arc<str>,
    file_path: PathBuf,
    upload_source: Option<UploadSource>,
    upload_file_snapshot: Option<UploadFileSnapshot>,
    transfer_lifecycle: Arc<TransferLifecycle>,
    direction: Direction,
    total_size: u64,
    chunk_size: u64,
    url: String,
    method: Method,
    headers: HeaderMap,
    breakpoint_upload: Arc<dyn BreakpointUpload + Send + Sync>,
    breakpoint_download: Arc<dyn BreakpointDownload + Send + Sync>,
    breakpoint_download_http: BreakpointDownloadHttpConfig,
    /// 每个分片的最大重试次数（仅作用于 chunk 传输失败）。
    max_chunk_retries: u32,
    /// 上传 prepare（`BreakpointUpload::prepare`）首次失败后的最大重试次数。
    max_upload_prepare_retries: u32,
    /// 单文件内并发上传的最大在飞分片数；默认 `1`（严格串行）。
    max_parts_in_flight: usize,
    http_client: Option<reqwest::Client>,
}

impl InnerTask {
    pub(crate) async fn from_pounce(
        pounce: PounceTask,
        default_download_http: BreakpointDownloadHttpConfig,
        http_client: Option<reqwest::Client>,
        default_upload: Arc<dyn BreakpointUpload + Send + Sync>,
        default_download: Arc<dyn BreakpointDownload + Send + Sync>,
    ) -> Result<Self, MeowError> {
        let task_id = TaskId::new();
        crate::meow_key_log!("inner_task", "from_pounce start: task_id={:?}", task_id);

        let PounceTask {
            direction,
            file_name,
            file_path,
            upload_source,
            total_size,
            chunk_size,
            url,
            method,
            headers,
            client_file_sign,
            breakpoint_upload,
            breakpoint_download,
            breakpoint_download_http,
            max_chunk_retries,
            max_upload_prepare_retries,
            max_parts_in_flight,
        } = pounce;

        let upload_source = upload_source.or_else(|| {
            if direction == Direction::Upload {
                Some(UploadSource::File(file_path.clone()))
            } else {
                None
            }
        });

        let mut upload_file_snapshot = None;
        let file_sign = match direction {
            Direction::Upload => {
                let source = upload_source.as_ref().ok_or_else(|| {
                    crate::log::emit_lazy(|| {
                        crate::log::Log::warn("inner_task", "upload task missing upload source")
                            .with_task_id(task_id.to_string())
                    });
                    MeowError::from_code_str(
                        InnerErrorCode::ParameterEmpty,
                        "upload task missing upload source",
                    )
                })?;
                match source {
                    UploadSource::File(path) => {
                        crate::meow_key_log!(
                            "inner_task",
                            "build upload task from file: task_id={:?} path={}",
                            task_id,
                            path.display()
                        );
                        // Validate only one actual part allocation. Multiplying
                        // by the requested scheduler window used to reject
                        // otherwise valid serial fallbacks. The parallel driver
                        // later applies the real part grid and shared client
                        // memory budget only when parallel execution is chosen.
                        UploadFileSnapshot::validate_chunk_size(chunk_size)?;
                        let snapshot =
                            UploadFileSnapshot::open_and_hash_with_verification_block_bytes(
                                path.clone(),
                                total_size,
                                chunk_size,
                            )
                            .await
                            .inspect_err(|e| {
                                crate::log::emit_lazy(|| {
                                    crate::log::Log::error(
                                        "inner_task",
                                        format!(
                                            "calculate_sign failed: path={} err={}",
                                            path.display(),
                                            crate::log::redact_secrets(&e.to_string())
                                        ),
                                    )
                                    .with_task_id(task_id.to_string())
                                });
                            })?;
                        let sign = snapshot.sign().to_owned();
                        upload_file_snapshot = Some(snapshot);
                        sign
                    }
                    UploadSource::Bytes(bytes) => {
                        crate::meow_key_log!(
                            "inner_task",
                            "build upload task from bytes: task_id={:?} len={}",
                            task_id,
                            bytes.len()
                        );
                        calculate_sign_bytes(&bytes[..])
                    }
                }
            }
            Direction::Download => {
                crate::meow_key_log!(
                    "inner_task",
                    "build download task: task_id={:?} path={}",
                    task_id,
                    file_path.display()
                );
                client_file_sign.unwrap_or_else(|| {
                    crate::log::emit_lazy(|| {
                        crate::log::Log::warn(
                            "inner_task",
                            "download task missing client_file_sign; defaulting to empty",
                        )
                        .with_task_id(task_id.to_string())
                    });
                    String::default()
                })
            }
        };

        let breakpoint_upload = breakpoint_upload.unwrap_or(default_upload);
        let breakpoint_download = breakpoint_download.unwrap_or(default_download);
        let breakpoint_download_http = breakpoint_download_http.unwrap_or(default_download_http);
        crate::meow_key_log!(
            "inner_task",
            "from_pounce resolved: task_id={:?} dir={:?} file={} chunk={} total={} max_chunk_retries={} max_upload_prepare_retries={}",
            task_id,
            direction,
            file_name,
            chunk_size,
            total_size,
            max_chunk_retries,
            max_upload_prepare_retries
        );

        crate::log::emit_lazy(|| {
            crate::log::Log::key("inner_task", format!("task created dir={:?}", direction))
                .with_task_id(task_id.to_string())
                .with_byte_len(total_size)
        });

        Ok(Self {
            task_id,
            file_sign: Arc::<str>::from(file_sign),
            file_name: Arc::<str>::from(file_name),
            file_path,
            upload_source,
            upload_file_snapshot,
            transfer_lifecycle: Arc::new(TransferLifecycle::new()),
            direction,
            total_size,
            chunk_size,
            url,
            method,
            headers,
            breakpoint_upload,
            breakpoint_download,
            breakpoint_download_http,
            max_chunk_retries,
            max_upload_prepare_retries,
            max_parts_in_flight,
            http_client,
        })
    }

    pub(crate) fn task_id(&self) -> TaskId {
        self.task_id
    }

    pub(crate) fn dedupe_key(&self) -> UniqueId {
        match self.direction {
            Direction::Upload => (Direction::Upload, self.file_sign.to_string()),
            Direction::Download => (Direction::Download, self.url.clone()),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn file_sign(&self) -> &str {
        &self.file_sign
    }

    #[allow(dead_code)]
    pub(crate) fn file_name(&self) -> &str {
        &self.file_name
    }

    /// Returns a cheaply-clonable handle to the file signature for hot-path DTO
    /// construction (progress/status emission).
    pub(crate) fn file_sign_arc(&self) -> Arc<str> {
        Arc::clone(&self.file_sign)
    }

    /// Returns a cheaply-clonable handle to the display file name for hot-path
    /// DTO construction (progress/status emission).
    pub(crate) fn file_name_arc(&self) -> Arc<str> {
        Arc::clone(&self.file_name)
    }

    pub(crate) fn file_path(&self) -> &Path {
        &self.file_path
    }

    pub(crate) fn upload_source(&self) -> Option<&UploadSource> {
        self.upload_source.as_ref()
    }

    pub(crate) fn upload_file_snapshot(&self) -> Option<&UploadFileSnapshot> {
        self.upload_file_snapshot.as_ref()
    }

    pub(crate) fn transfer_lifecycle(&self) -> Arc<TransferLifecycle> {
        Arc::clone(&self.transfer_lifecycle)
    }

    pub(crate) fn begin_terminal_pause(&self) -> StopRequestDisposition {
        self.transfer_lifecycle.begin_pause()
    }

    pub(crate) fn begin_terminal_cancel(&self) -> StopRequestDisposition {
        self.transfer_lifecycle.begin_cancel()
    }

    pub(crate) fn reset_terminal_pause(&self) {
        self.transfer_lifecycle.reset_pause();
    }

    pub(crate) fn direction(&self) -> Direction {
        self.direction
    }

    pub(crate) fn total_size(&self) -> u64 {
        self.total_size
    }

    pub(crate) fn chunk_size(&self) -> u64 {
        self.chunk_size
    }

    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    pub(crate) fn method(&self) -> Method {
        self.method.clone()
    }

    pub(crate) fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    pub(crate) fn breakpoint_download_http(&self) -> &BreakpointDownloadHttpConfig {
        &self.breakpoint_download_http
    }

    pub(crate) fn breakpoint_upload(&self) -> &Arc<dyn BreakpointUpload + Send + Sync> {
        &self.breakpoint_upload
    }

    pub(crate) fn breakpoint_download(&self) -> &Arc<dyn BreakpointDownload + Send + Sync> {
        &self.breakpoint_download
    }

    pub(crate) fn max_chunk_retries(&self) -> u32 {
        self.max_chunk_retries
    }

    pub(crate) fn max_upload_prepare_retries(&self) -> u32 {
        self.max_upload_prepare_retries
    }

    pub(crate) fn max_parts_in_flight(&self) -> usize {
        self.max_parts_in_flight
    }

    pub(crate) fn http_client_ref(&self) -> Option<&reqwest::Client> {
        self.http_client.as_ref()
    }
}
