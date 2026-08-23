use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use reqwest::header::HeaderMap;
use reqwest::Method;
use tokio::sync::Mutex;

use crate::direction::Direction;
use crate::http_breakpoint::{BreakpointDownload, BreakpointDownloadHttpConfig, BreakpointUpload};
use crate::inner::inner_task::InnerTask;
use crate::upload_file::UploadFileSnapshot;
use crate::upload_source::UploadSource;

fn arm_download_checkpoint_timer(
    progress: Arc<StdMutex<Option<crate::dflt::download_progress::DownloadProgress>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(crate::dflt::download_progress::DEFAULT_CHECKPOINT_INTERVAL).await;
        let result = tokio::task::spawn_blocking(move || {
            let mut guard = progress
                .lock()
                .map_err(|_| std::io::Error::other("download checkpoint timer lock poisoned"))?;
            if let Some(state) = guard.as_mut() {
                state.checkpoint_timer_fired()?;
            }
            Ok::<(), std::io::Error>(())
        })
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                crate::meow_warn_log!(
                    "download_checkpoint",
                    "timed .rcdl checkpoint failed: {}",
                    error
                );
            }
            Err(error) => {
                crate::meow_warn_log!(
                    "download_checkpoint",
                    "timed .rcdl checkpoint worker failed: {}",
                    error
                );
            }
        }
    })
}

/// Immutable task snapshot exposed to transfer executor implementations.
///
/// This type is constructed from the crate-internal scheduler task state and
/// intentionally exposes read-only accessors.
#[derive(Clone)]
pub struct TransferTask {
    /// Stable file signature.
    file_sign: Arc<str>,
    /// Display file name.
    file_name: Arc<str>,
    /// Local file path.
    file_path: PathBuf,
    /// Upload-only source descriptor.
    upload_source: Option<UploadSource>,
    upload_file_snapshot: Option<UploadFileSnapshot>,
    /// Transfer direction.
    direction: Direction,
    /// Total file size in bytes.
    total_size: u64,
    /// Chunk size in bytes.
    chunk_size: u64,
    /// Request URL.
    url: String,
    /// Request HTTP method.
    method: Method,
    /// Base request headers.
    headers: HeaderMap,
    /// HTTP config for breakpoint download behavior.
    breakpoint_download_http: BreakpointDownloadHttpConfig,
    /// Upload breakpoint protocol implementation.
    breakpoint_upload: Arc<dyn BreakpointUpload + Send + Sync>,
    /// Download breakpoint protocol implementation.
    breakpoint_download: Arc<dyn BreakpointDownload + Send + Sync>,
    /// Optional per-task custom HTTP client.
    http_client: Option<reqwest::Client>,
    /// Task-level download file handle slot to avoid reopening per chunk.
    download_file_slot: Arc<Mutex<Option<tokio::fs::File>>>,
    /// Makes provider abort idempotent when several parallel readers observe the
    /// same source-generation failure concurrently.
    upload_abort_started: Arc<std::sync::atomic::AtomicBool>,
    /// Cross-client/process ownership of the visible download target.
    target_lease: Arc<StdMutex<Option<crate::target_lease::TargetLease>>>,
    /// Max parts of this file transferred concurrently (intra-file parallel).
    /// `1` means the strict-serial legacy path.
    max_parts_in_flight: usize,
    /// Shared progress bitmap for the concurrent download path (None until the
    /// parallel `download_prepare` initializes it). Guarded so concurrent parts
    /// can flip their bit without racing.
    download_progress: Arc<StdMutex<Option<crate::dflt::download_progress::DownloadProgress>>>,
    /// Max retries after first failed upload prepare (`BreakpointUpload::prepare`).
    max_upload_prepare_retries: u32,
}

impl std::fmt::Debug for TransferTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransferTask")
            .field("file_sign", &self.file_sign)
            .field("file_name", &self.file_name)
            .field("file_path", &self.file_path)
            .field("upload_source", &self.upload_source)
            .field("direction", &self.direction)
            .field("total_size", &self.total_size)
            .field("chunk_size", &self.chunk_size)
            .field("url", &self.url)
            .field("method", &self.method)
            .field("headers", &self.headers)
            .field("breakpoint_upload", &"<dyn BreakpointUpload>")
            .field("breakpoint_download", &"<dyn BreakpointDownload>")
            .field("breakpoint_download_http", &self.breakpoint_download_http)
            .field(
                "max_upload_prepare_retries",
                &self.max_upload_prepare_retries,
            )
            .finish()
    }
}

impl TransferTask {
    /// Creates a transfer snapshot from an internal runtime task.
    pub(crate) fn from_inner(inner: &InnerTask) -> Self {
        Self {
            file_sign: inner.file_sign_arc(),
            file_name: inner.file_name_arc(),
            file_path: inner.file_path().to_path_buf(),
            upload_source: inner.upload_source().cloned(),
            upload_file_snapshot: inner.upload_file_snapshot().cloned(),
            direction: inner.direction(),
            total_size: inner.total_size(),
            chunk_size: inner.chunk_size(),
            url: inner.url().to_string(),
            method: inner.method(),
            headers: inner.headers().clone(),
            breakpoint_download_http: inner.breakpoint_download_http().clone(),
            breakpoint_upload: inner.breakpoint_upload().clone(),
            breakpoint_download: inner.breakpoint_download().clone(),
            http_client: inner.http_client_ref().cloned(),
            download_file_slot: Arc::new(Mutex::new(None)),
            upload_abort_started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            target_lease: Arc::new(StdMutex::new(None)),
            max_parts_in_flight: inner.max_parts_in_flight(),
            download_progress: Arc::new(StdMutex::new(None)),
            max_upload_prepare_retries: inner.max_upload_prepare_retries(),
        }
    }

    /// Returns transfer direction.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::TransferTask;
    ///
    /// fn inspect(task: &TransferTask) {
    ///     let _ = task.direction();
    /// }
    /// ```
    pub fn direction(&self) -> Direction {
        self.direction
    }

    /// Returns total file size in bytes.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::TransferTask;
    ///
    /// fn inspect(task: &TransferTask) {
    ///     let _ = task.total_size();
    /// }
    /// ```
    pub fn total_size(&self) -> u64 {
        self.total_size
    }

    /// Returns chunk size in bytes.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::TransferTask;
    ///
    /// fn inspect(task: &TransferTask) {
    ///     let _ = task.chunk_size();
    /// }
    /// ```
    pub fn chunk_size(&self) -> u64 {
        self.chunk_size
    }

    /// Returns file signature.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::TransferTask;
    ///
    /// fn inspect(task: &TransferTask) {
    ///     let _ = task.file_sign();
    /// }
    /// ```
    pub fn file_sign(&self) -> &str {
        &self.file_sign
    }

    /// Returns display file name.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::TransferTask;
    ///
    /// fn inspect(task: &TransferTask) {
    ///     let _ = task.file_name();
    /// }
    /// ```
    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    /// Returns local file path.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::TransferTask;
    ///
    /// fn inspect(task: &TransferTask) {
    ///     let _ = task.file_path();
    /// }
    /// ```
    pub fn file_path(&self) -> &Path {
        &self.file_path
    }

    /// Returns upload source for upload tasks.
    pub(crate) fn upload_source(&self) -> Option<&UploadSource> {
        self.upload_source.as_ref()
    }

    /// Returns request URL.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::TransferTask;
    ///
    /// fn inspect(task: &TransferTask) {
    ///     let _ = task.url();
    /// }
    /// ```
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Returns request HTTP method.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::TransferTask;
    ///
    /// fn inspect(task: &TransferTask) {
    ///     let _ = task.method();
    /// }
    /// ```
    pub fn method(&self) -> Method {
        self.method.clone()
    }

    /// Returns base request headers.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::TransferTask;
    ///
    /// fn inspect(task: &TransferTask) {
    ///     let _ = task.headers();
    /// }
    /// ```
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// Returns task-level breakpoint download HTTP configuration.
    ///
    /// Custom [`crate::download_trait::BreakpointDownload`] implementations can
    /// read values such as `range_accept`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::TransferTask;
    ///
    /// fn inspect(task: &TransferTask) {
    ///     let _ = task.breakpoint_download_http();
    /// }
    /// ```
    pub fn breakpoint_download_http(&self) -> Option<&BreakpointDownloadHttpConfig> {
        Some(&self.breakpoint_download_http)
    }

    /// Returns task-level upload protocol implementation.
    pub(crate) fn breakpoint_upload(&self) -> Option<&Arc<dyn BreakpointUpload + Send + Sync>> {
        Some(&self.breakpoint_upload)
    }

    /// Returns task-level download protocol implementation.
    pub(crate) fn breakpoint_download(&self) -> Option<&Arc<dyn BreakpointDownload + Send + Sync>> {
        Some(&self.breakpoint_download)
    }

    /// Returns max retries after the first failed upload prepare.
    pub(crate) fn max_upload_prepare_retries(&self) -> u32 {
        self.max_upload_prepare_retries
    }

    /// Returns task-level custom HTTP client, if configured.
    pub(crate) fn http_client_ref(&self) -> Option<&reqwest::Client> {
        self.http_client.as_ref()
    }

    pub(crate) fn upload_file_snapshot(&self) -> Option<&UploadFileSnapshot> {
        self.upload_file_snapshot.as_ref()
    }

    pub(crate) fn begin_upload_abort(&self) -> bool {
        self.upload_abort_started
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
    }

    pub(crate) async fn ensure_download_target_lease(&self) -> Result<(), crate::MeowError> {
        let slot = Arc::clone(&self.target_lease);
        let path = self.file_path.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = slot.lock().map_err(|_| {
                crate::MeowError::from_code_str(
                    crate::InnerErrorCode::LockPoisoned,
                    "download target lease lock poisoned",
                )
            })?;
            if guard.is_none() {
                *guard = Some(
                    crate::target_lease::TargetLease::acquire(&path).map_err(|e| {
                        crate::MeowError::from_source(
                            crate::InnerErrorCode::InvalidTaskState,
                            format!("acquire download target lease failed: {}", path.display()),
                            e,
                        )
                    })?,
                );
            }
            Ok(())
        })
        .await
        .map_err(|e| {
            crate::MeowError::from_code(
                crate::InnerErrorCode::IoError,
                format!("download target lease worker failed: {e}"),
            )
        })?
    }

    /// Returns download file handle slot used by executor.
    pub(crate) fn download_file_slot(&self) -> &Arc<Mutex<Option<tokio::fs::File>>> {
        &self.download_file_slot
    }

    /// Returns the configured max concurrent parts for this task.
    pub(crate) fn max_parts_in_flight(&self) -> usize {
        self.max_parts_in_flight
    }

    /// Returns the shared concurrent-download progress slot.
    pub(crate) fn download_progress(
        &self,
    ) -> &Arc<StdMutex<Option<crate::dflt::download_progress::DownloadProgress>>> {
        &self.download_progress
    }

    pub(crate) async fn stage_download_part(
        &self,
        offset: u64,
        digest: [u8; 32],
    ) -> Result<(), crate::MeowError> {
        let progress = Arc::clone(&self.download_progress);
        let timer_progress = Arc::clone(&progress);
        let outcome = tokio::task::spawn_blocking(move || {
            let mut guard = progress.lock().map_err(|_| {
                crate::MeowError::from_code_str(
                    crate::InnerErrorCode::LockPoisoned,
                    "download checkpoint lock poisoned",
                )
            })?;
            let state = guard.as_mut().ok_or_else(|| {
                crate::MeowError::from_code_str(
                    crate::InnerErrorCode::InvalidTaskState,
                    "download checkpoint state missing",
                )
            })?;
            state.stage_done_with_digest(offset, digest).map_err(|e| {
                crate::MeowError::from_io("persist .rcdl checkpoint failed".to_owned(), e)
            })
        })
        .await
        .map_err(|e| {
            crate::MeowError::from_code(
                crate::InnerErrorCode::IoError,
                format!("download checkpoint worker failed: {e}"),
            )
        })??;
        if outcome.arm_timer {
            // The progress object itself suppresses duplicate timers. The task
            // may complete before this wake-up; then the shared slot is empty
            // and the timer exits without touching the completed file.
            drop(arm_download_checkpoint_timer(timer_progress));
        }
        Ok(())
    }

    pub(crate) async fn force_download_checkpoint(&self) -> Result<(), crate::MeowError> {
        let progress = Arc::clone(&self.download_progress);
        tokio::task::spawn_blocking(move || {
            let mut guard = progress.lock().map_err(|_| {
                crate::MeowError::from_code_str(
                    crate::InnerErrorCode::LockPoisoned,
                    "download checkpoint lock poisoned",
                )
            })?;
            if let Some(state) = guard.as_mut() {
                state.force_checkpoint().map_err(|e| {
                    crate::MeowError::from_io("force .rcdl checkpoint failed".to_owned(), e)
                })?;
            }
            Ok(())
        })
        .await
        .map_err(|e| {
            crate::MeowError::from_code(
                crate::InnerErrorCode::IoError,
                format!("download checkpoint worker failed: {e}"),
            )
        })?
    }

    pub(crate) async fn take_download_progress_after_checkpoint(
        &self,
    ) -> Result<Option<crate::dflt::download_progress::DownloadProgress>, crate::MeowError> {
        let progress = Arc::clone(&self.download_progress);
        tokio::task::spawn_blocking(move || {
            let mut guard = progress.lock().map_err(|_| {
                crate::MeowError::from_code_str(
                    crate::InnerErrorCode::LockPoisoned,
                    "download checkpoint lock poisoned",
                )
            })?;
            if let Some(state) = guard.as_mut() {
                state.force_checkpoint().map_err(|e| {
                    crate::MeowError::from_io("final .rcdl checkpoint failed".to_owned(), e)
                })?;
                state.validate_committed_content().map_err(|e| {
                    crate::MeowError::from_source(
                        crate::InnerErrorCode::ChecksumMismatch,
                        "validate completed download content failed".to_owned(),
                        e,
                    )
                })?;
            }
            Ok(guard.take())
        })
        .await
        .map_err(|e| {
            crate::MeowError::from_code(
                crate::InnerErrorCode::IoError,
                format!("download checkpoint worker failed: {e}"),
            )
        })?
    }
}

#[cfg(test)]
mod checkpoint_timer_tests {
    use super::arm_download_checkpoint_timer;
    use crate::dflt::download_progress::{sidecar_path, DownloadProgress};
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn lone_staged_part_is_checkpointed_by_wall_clock_timer() {
        let target = std::env::temp_dir().join(format!(
            "rusty_cat_checkpoint_timer_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::write(&target, vec![7_u8; 20]).expect("target");
        let mut progress =
            DownloadProgress::load_or_create(&target, 20, 10, 4, "timer-test").expect("progress");
        assert!(!progress.stage_done(0).expect("stage"));
        assert!(!progress.is_done(0), "the batch threshold was not reached");

        let slot = Arc::new(Mutex::new(Some(progress)));
        arm_download_checkpoint_timer(Arc::clone(&slot))
            .await
            .expect("timer task");
        assert!(
            slot.lock()
                .expect("progress lock")
                .as_ref()
                .expect("progress")
                .is_done(0),
            "the 250 ms wall-clock wake-up must publish the staged part"
        );

        let _ = std::fs::remove_file(sidecar_path(&target));
        let _ = std::fs::remove_file(target);
    }
}
