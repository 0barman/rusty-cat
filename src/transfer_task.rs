use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, Weak};

use fs2::FileExt;
use reqwest::header::HeaderMap;
use reqwest::Method;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::direction::Direction;
use crate::http_breakpoint::{BreakpointDownload, BreakpointDownloadHttpConfig, BreakpointUpload};
use crate::inner::inner_task::InnerTask;
use crate::upload_file::UploadFileSnapshot;
use crate::upload_source::UploadSource;

fn map_download_validation_error(error: std::io::Error) -> crate::MeowError {
    match error.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::UnexpectedEof => {
            crate::MeowError::from_source(
                crate::InnerErrorCode::LocalFileRemoved,
                "completed download disappeared or was truncated".to_owned(),
                error,
            )
        }
        std::io::ErrorKind::InvalidData => crate::MeowError::from_source(
            crate::InnerErrorCode::ChecksumMismatch,
            "validate completed download content failed".to_owned(),
            error,
        ),
        _ => crate::MeowError::from_io(
            "read completed download for validation failed".to_owned(),
            error,
        ),
    }
}

/// Maps failures while acquiring either layer of download-target ownership.
///
/// `target_lease` normalizes every genuine advisory-lock conflict to
/// `WouldBlock`. Only that condition is a task-state conflict; failures while
/// creating/opening the lease or target are ordinary local I/O failures and
/// must retain `MeowError::from_io` classifications such as `DiskFull` and
/// `LocalFileRemoved`.
fn map_download_target_lock_error(
    message: impl Into<String>,
    error: std::io::Error,
) -> crate::MeowError {
    let message = message.into();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        crate::MeowError::from_source(crate::InnerErrorCode::InvalidTaskState, message, error)
    } else {
        crate::MeowError::from_io(message, error)
    }
}

fn arm_download_checkpoint_timer(
    progress: Weak<StdMutex<Option<crate::dflt::download_progress::DownloadProgress>>>,
    file_slot: Weak<Mutex<Option<tokio::fs::File>>>,
    barrier: Weak<Mutex<()>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(crate::dflt::download_progress::DEFAULT_CHECKPOINT_INTERVAL).await;
        let (Some(progress), Some(file_slot), Some(barrier)) =
            (progress.upgrade(), file_slot.upgrade(), barrier.upgrade())
        else {
            // The transfer ended before the timer fired. Weak references ensure
            // a stale timer never extends the actual target lock lifetime.
            return;
        };
        let result = async {
            let _barrier = barrier.lock().await;
            let begin_progress = Arc::clone(&progress);
            let checkpoint_due = tokio::task::spawn_blocking(move || {
                let mut guard = begin_progress.lock().map_err(|_| {
                    std::io::Error::other("download checkpoint timer lock poisoned")
                })?;
                match guard.as_mut() {
                    Some(state) => state.begin_timer_checkpoint(),
                    None => Ok(false),
                }
            })
            .await
            .map_err(|error| std::io::Error::other(format!("timer worker failed: {error}")))??;
            if !checkpoint_due {
                return Ok::<(), std::io::Error>(());
            }

            {
                let mut slot = file_slot.lock().await;
                let file = slot.as_mut().ok_or_else(|| {
                    std::io::Error::other("locked download target missing during timed checkpoint")
                })?;
                file.sync_data().await?;
            }
            let commit_progress = Arc::clone(&progress);
            tokio::task::spawn_blocking(move || {
                let mut guard = commit_progress.lock().map_err(|_| {
                    std::io::Error::other("download checkpoint timer lock poisoned")
                })?;
                if let Some(state) = guard.as_mut() {
                    state.commit_checkpoint_after_data_sync()?;
                }
                Ok::<(), std::io::Error>(())
            })
            .await
            .map_err(|error| std::io::Error::other(format!("timer worker failed: {error}")))??;
            Ok(())
        }
        .await;
        if let Err(error) = result {
            crate::meow_warn_log!(
                "download_checkpoint",
                "timed .rcdl checkpoint failed: {}",
                error
            );
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
    /// Serializes writes and checkpoint data barriers for the locked target.
    download_checkpoint_barrier: Arc<Mutex<()>>,
    /// Shared terminal arbitration and abort idempotence for every task view
    /// derived from the same scheduler entry.
    transfer_lifecycle: Arc<crate::inner::inner_task::TransferLifecycle>,
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
            download_checkpoint_barrier: Arc::new(Mutex::new(())),
            transfer_lifecycle: inner.transfer_lifecycle(),
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
        self.transfer_lifecycle.begin_abort()
    }

    pub(crate) fn require_upload_abort(&self) {
        if self.direction == Direction::Upload {
            self.transfer_lifecycle.require_abort();
        }
    }

    pub(crate) fn upload_abort_required(&self) -> bool {
        self.direction == Direction::Upload && self.transfer_lifecycle.abort_required()
    }

    pub(crate) fn begin_terminal_completion(&self) -> bool {
        self.transfer_lifecycle.begin_completion()
    }

    pub(crate) fn acknowledge_terminal_completion(&self) {
        self.transfer_lifecycle.acknowledge_completion();
    }

    pub(crate) fn finish_incomplete_completion(
        &self,
    ) -> Option<crate::inner::inner_task::DeferredStop> {
        self.transfer_lifecycle.finish_incomplete_completion()
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
                        map_download_target_lock_error(
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

    /// Opens the actual target and acquires its cross-platform file lock.
    /// Every transfer write must reuse the returned task slot: on Windows a
    /// second handle cannot access a range locked through the first handle.
    pub(crate) async fn ensure_download_target_file_locked(&self) -> Result<u64, crate::MeowError> {
        let mut slot = self.download_file_slot.lock().await;
        if let Some(file) = slot.as_ref() {
            return file
                .metadata()
                .await
                .map(|metadata| metadata.len())
                .map_err(|e| {
                    crate::MeowError::from_io(
                        format!(
                            "stat locked download target failed: {}",
                            self.file_path.display()
                        ),
                        e,
                    )
                });
        }

        let path = self.file_path.clone();
        let display = path.display().to_string();
        let file = tokio::task::spawn_blocking(move || {
            crate::target_lease::open_locked_target(&path, true)
        })
        .await
        .map_err(|e| {
            crate::MeowError::from_code(
                crate::InnerErrorCode::IoError,
                format!("download target lock worker failed: {e}"),
            )
        })?
        .map_err(|e| {
            map_download_target_lock_error(
                format!("lock actual download target failed: {display}"),
                e,
            )
        })?;
        let len = file
            .metadata()
            .map(|metadata| metadata.len())
            .map_err(|e| {
                crate::MeowError::from_io(
                    format!("stat newly locked download target failed: {display}"),
                    e,
                )
            })?;
        *slot = Some(tokio::fs::File::from_std(file));
        Ok(len)
    }

    /// Flushes, explicitly unlocks and closes the actual target handle. The
    /// caller releases the path lease next, after completion validation or
    /// failure checkpointing and before publishing a terminal event.
    pub(crate) async fn release_download_target_file_lock(&self) -> Result<(), crate::MeowError> {
        let file = {
            let mut slot = self.download_file_slot.lock().await;
            slot.take()
        };
        let Some(file) = file else {
            return Ok(());
        };
        file.sync_all().await.map_err(|e| {
            crate::MeowError::from_io(
                format!(
                    "sync locked download target failed: {}",
                    self.file_path.display()
                ),
                e,
            )
        })?;
        let file = file.into_std().await;
        tokio::task::spawn_blocking(move || FileExt::unlock(&file))
            .await
            .map_err(|e| {
                crate::MeowError::from_code(
                    crate::InnerErrorCode::IoError,
                    format!("download target unlock worker failed: {e}"),
                )
            })?
            .map_err(|e| crate::MeowError::from_io("unlock download target failed".to_owned(), e))
    }

    /// Releases the normalized path/inode lease after the actual target handle
    /// has been closed. Terminal events must not become observable before this
    /// succeeds, otherwise an immediate same-target enqueue can spuriously see
    /// the completed task as still owning the path.
    pub(crate) fn release_download_target_lease(&self) -> Result<(), crate::MeowError> {
        let lease = self
            .target_lease
            .lock()
            .map_err(|_| {
                crate::MeowError::from_code_str(
                    crate::InnerErrorCode::LockPoisoned,
                    "download target lease lock poisoned",
                )
            })?
            .take();
        drop(lease);
        Ok(())
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

    /// Writes a fully received parallel part through the unique locked target
    /// handle, then stages its digest. If the checkpoint batch is due, the same
    /// handle is synced before the sidecar publishes the part bit.
    pub(crate) async fn write_and_stage_download_part(
        &self,
        offset: u64,
        body: &[u8],
        digest: [u8; 32],
    ) -> Result<(), crate::MeowError> {
        let _barrier = self.download_checkpoint_barrier.lock().await;
        let body_len = u64::try_from(body.len()).map_err(|_| {
            crate::MeowError::from_code_str(
                crate::InnerErrorCode::InvalidRange,
                "parallel download part length does not fit u64",
            )
        })?;
        let part_end = offset.checked_add(body_len).ok_or_else(|| {
            crate::MeowError::from_code_str(
                crate::InnerErrorCode::InvalidRange,
                "parallel download part end overflow",
            )
        })?;
        {
            let mut slot = self.download_file_slot.lock().await;
            let file = slot.as_mut().ok_or_else(|| {
                crate::MeowError::from_code_str(
                    crate::InnerErrorCode::InvalidTaskState,
                    "locked download target missing during positioned write",
                )
            })?;
            let file_len = file
                .metadata()
                .await
                .map(|metadata| metadata.len())
                .map_err(|e| {
                    crate::MeowError::from_io(
                        format!(
                            "stat locked download target failed: {}",
                            self.file_path.display()
                        ),
                        e,
                    )
                })?;
            if file_len < part_end {
                return Err(crate::MeowError::from_code(
                    crate::InnerErrorCode::LocalFileRemoved,
                    format!(
                        "download target was truncated before positioned write: len={file_len} need>={part_end}"
                    ),
                ));
            }
            file.seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(|e| {
                    crate::MeowError::from_io(
                        format!("seek locked download target failed: offset={offset}"),
                        e,
                    )
                })?;
            file.write_all(body).await.map_err(|e| {
                crate::MeowError::from_io(
                    format!("write locked download target failed: offset={offset}"),
                    e,
                )
            })?;
            file.flush().await.map_err(|e| {
                crate::MeowError::from_io(
                    format!("flush locked download target failed: offset={offset}"),
                    e,
                )
            })?;
        }

        self.stage_download_part_digest_locked(offset, digest, true)
            .await
    }

    /// Stages a serial part after its streaming write released the file mutex.
    /// Dropping that mutex first keeps the global order `barrier -> file` and
    /// prevents the timer from deadlocking with a serial network response.
    pub(crate) async fn stage_serial_download_part(
        &self,
        offset: u64,
        digest: [u8; 32],
    ) -> Result<(), crate::MeowError> {
        let _barrier = self.download_checkpoint_barrier.lock().await;
        self.stage_download_part_digest_locked(offset, digest, true)
            .await
    }

    /// Establishes the serial layout invariant: the visible target contains
    /// exactly the longest committed prefix, and the sidecar contains no bits
    /// beyond it. Target truncation is synced before the compacted snapshot is
    /// published, so a crash can never expose bits for missing bytes.
    pub(crate) async fn retain_serial_download_contiguous_progress(
        &self,
    ) -> Result<u64, crate::MeowError> {
        let _barrier = self.download_checkpoint_barrier.lock().await;
        let watermark = self
            .download_progress
            .lock()
            .map_err(|_| {
                crate::MeowError::from_code_str(
                    crate::InnerErrorCode::LockPoisoned,
                    "download checkpoint lock poisoned",
                )
            })?
            .as_ref()
            .ok_or_else(|| {
                crate::MeowError::from_code_str(
                    crate::InnerErrorCode::InvalidTaskState,
                    "download checkpoint state missing during serial compaction",
                )
            })?
            .contiguous_watermark();

        {
            let mut slot = self.download_file_slot.lock().await;
            let file = slot.as_mut().ok_or_else(|| {
                crate::MeowError::from_code_str(
                    crate::InnerErrorCode::InvalidTaskState,
                    "locked download target missing during serial compaction",
                )
            })?;
            file.set_len(watermark).await.map_err(|e| {
                crate::MeowError::from_io(
                    format!(
                        "truncate serial download to verified prefix failed: {}",
                        self.file_path.display()
                    ),
                    e,
                )
            })?;
            file.sync_all().await.map_err(|e| {
                crate::MeowError::from_io("sync serial verified prefix failed".to_owned(), e)
            })?;
        }

        let progress = Arc::clone(&self.download_progress);
        let persisted = tokio::task::spawn_blocking(move || {
            let mut guard = progress.lock().map_err(|_| {
                crate::MeowError::from_code_str(
                    crate::InnerErrorCode::LockPoisoned,
                    "download checkpoint lock poisoned",
                )
            })?;
            let state = guard.as_mut().ok_or_else(|| {
                crate::MeowError::from_code_str(
                    crate::InnerErrorCode::InvalidTaskState,
                    "download checkpoint state missing during serial compaction",
                )
            })?;
            state
                .retain_contiguous_prefix_after_data_sync()
                .map_err(|e| {
                    crate::MeowError::from_io(
                        "persist compacted serial checkpoint failed".to_owned(),
                        e,
                    )
                })
        })
        .await
        .map_err(|e| {
            crate::MeowError::from_code(
                crate::InnerErrorCode::IoError,
                format!("serial compaction checkpoint worker failed: {e}"),
            )
        })??;
        if persisted != watermark {
            return Err(crate::MeowError::from_code(
                crate::InnerErrorCode::InvalidTaskState,
                format!(
                    "serial checkpoint watermark changed during compaction: expected={watermark} actual={persisted}"
                ),
            ));
        }
        Ok(watermark)
    }

    async fn stage_download_part_digest_locked(
        &self,
        offset: u64,
        digest: [u8; 32],
        arm_timer: bool,
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
            state
                .stage_done_with_digest_deferred(offset, digest)
                .map_err(|e| {
                    crate::MeowError::from_io("stage .rcdl checkpoint failed".to_owned(), e)
                })
        })
        .await
        .map_err(|e| {
            crate::MeowError::from_code(
                crate::InnerErrorCode::IoError,
                format!("download checkpoint worker failed: {e}"),
            )
        })??;
        if outcome.checkpoint_due {
            self.checkpoint_locked_download_target().await?;
        } else if outcome.arm_timer && arm_timer {
            // The progress object itself suppresses duplicate timers. The task
            // may complete before this wake-up; then the shared slot is empty
            // and the timer exits without touching the completed file.
            drop(arm_download_checkpoint_timer(
                Arc::downgrade(&timer_progress),
                Arc::downgrade(&self.download_file_slot),
                Arc::downgrade(&self.download_checkpoint_barrier),
            ));
        }
        Ok(())
    }

    /// Runs an externally coordinated checkpoint while the caller already owns
    /// `download_checkpoint_barrier`.
    async fn checkpoint_locked_download_target(&self) -> Result<(), crate::MeowError> {
        let progress = Arc::clone(&self.download_progress);
        let checkpoint_due = tokio::task::spawn_blocking(move || {
            let mut guard = progress.lock().map_err(|_| {
                crate::MeowError::from_code_str(
                    crate::InnerErrorCode::LockPoisoned,
                    "download checkpoint lock poisoned",
                )
            })?;
            let Some(state) = guard.as_mut() else {
                return Ok(false);
            };
            let due = state.begin_external_checkpoint().map_err(|e| {
                crate::MeowError::from_io("begin .rcdl checkpoint failed".to_owned(), e)
            })?;
            Ok(due)
        })
        .await
        .map_err(|e| {
            crate::MeowError::from_code(
                crate::InnerErrorCode::IoError,
                format!("download checkpoint worker failed: {e}"),
            )
        })??;
        if !checkpoint_due {
            return Ok(());
        }

        {
            let mut slot = self.download_file_slot.lock().await;
            let file = slot.as_mut().ok_or_else(|| {
                crate::MeowError::from_code_str(
                    crate::InnerErrorCode::InvalidTaskState,
                    "locked download target missing during checkpoint",
                )
            })?;
            file.sync_data().await.map_err(|e| {
                crate::MeowError::from_io("sync download checkpoint data failed".to_owned(), e)
            })?;
        }

        let progress = Arc::clone(&self.download_progress);
        tokio::task::spawn_blocking(move || {
            let mut guard = progress.lock().map_err(|_| {
                crate::MeowError::from_code_str(
                    crate::InnerErrorCode::LockPoisoned,
                    "download checkpoint lock poisoned",
                )
            })?;
            if let Some(state) = guard.as_mut() {
                state.commit_checkpoint_after_data_sync().map_err(|e| {
                    crate::MeowError::from_io("commit .rcdl checkpoint failed".to_owned(), e)
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

    pub(crate) async fn force_download_checkpoint(&self) -> Result<(), crate::MeowError> {
        let _barrier = self.download_checkpoint_barrier.lock().await;
        self.checkpoint_locked_download_target().await
    }

    pub(crate) async fn take_download_progress_after_checkpoint(
        &self,
    ) -> Result<Option<crate::dflt::download_progress::DownloadProgress>, crate::MeowError> {
        #[cfg(windows)]
        {
            // Windows whole-file locks can reject reads through a second handle
            // in the same process. Temporarily move the exact locked handle to
            // the blocking validator without closing or unlocking it. The
            // handle was opened without FILE_SHARE_DELETE, so the visible path
            // cannot be replaced while this content check runs.
            let file = {
                let mut slot = self.download_file_slot.lock().await;
                slot.take().ok_or_else(|| {
                    crate::MeowError::from_code_str(
                        crate::InnerErrorCode::InvalidTaskState,
                        "locked download target missing during final validation",
                    )
                })?
            };
            let file = file.into_std().await;
            let progress = Arc::clone(&self.download_progress);
            let worker = tokio::task::spawn_blocking(move || {
                let mut file = file;
                let result = (|| {
                    let mut guard = progress.lock().map_err(|_| {
                        crate::MeowError::from_code_str(
                            crate::InnerErrorCode::LockPoisoned,
                            "download checkpoint lock poisoned",
                        )
                    })?;
                    if let Some(state) = guard.as_ref() {
                        state
                            .validate_committed_content_on_locked_file(&mut file)
                            .map_err(map_download_validation_error)?;
                    }
                    Ok(guard.take())
                })();
                (result, file)
            })
            .await;
            return match worker {
                Ok((result, file)) => {
                    let mut slot = self.download_file_slot.lock().await;
                    if slot.is_some() {
                        return Err(crate::MeowError::from_code_str(
                            crate::InnerErrorCode::InvalidTaskState,
                            "download target slot changed during final validation",
                        ));
                    }
                    *slot = Some(tokio::fs::File::from_std(file));
                    result
                }
                Err(error) => Err(crate::MeowError::from_code(
                    crate::InnerErrorCode::IoError,
                    format!("download validation worker failed: {error}"),
                )),
            };
        }

        #[cfg(not(windows))]
        {
            let progress = Arc::clone(&self.download_progress);
            tokio::task::spawn_blocking(move || {
                let mut guard = progress.lock().map_err(|_| {
                    crate::MeowError::from_code_str(
                        crate::InnerErrorCode::LockPoisoned,
                        "download checkpoint lock poisoned",
                    )
                })?;
                if let Some(state) = guard.as_ref() {
                    state
                        .validate_committed_content()
                        .map_err(map_download_validation_error)?;
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

    /// Commits all pending digests, validates every committed range through the
    /// visible path, removes the sidecar, and only then releases the actual
    /// target lock. Both serial and parallel downloads use this exact sequence.
    pub(crate) async fn finalize_download_content(
        &self,
        expected_total: u64,
    ) -> Result<(), crate::MeowError> {
        let _barrier = self.download_checkpoint_barrier.lock().await;
        let finalization = async {
            self.checkpoint_locked_download_target().await?;
            let visible_len = tokio::fs::metadata(&self.file_path)
                .await
                .map(|metadata| metadata.len())
                .map_err(|e| {
                    crate::MeowError::from_io(
                        format!("stat completed download failed: {}", self.file_path.display()),
                        e,
                    )
                })?;
            if visible_len != expected_total {
                return Err(crate::MeowError::from_code(
                    crate::InnerErrorCode::LocalFileRemoved,
                    format!(
                        "download length changed before complete: expected={expected_total} actual={visible_len}"
                    ),
                ));
            }
            let progress = self
                .take_download_progress_after_checkpoint()
                .await?
                .ok_or_else(|| {
                    crate::MeowError::from_code_str(
                        crate::InnerErrorCode::InvalidTaskState,
                        "download progress missing during final validation",
                    )
                })?;
            if progress.total() != expected_total {
                return Err(crate::MeowError::from_code(
                    crate::InnerErrorCode::InvalidRange,
                    format!(
                        "download progress total mismatch: expected={expected_total} progress={}",
                        progress.total()
                    ),
                ));
            }
            if !progress.all_done() {
                return Err(crate::MeowError::from_code_str(
                    crate::InnerErrorCode::InvalidRange,
                    "download complete called before all parts recorded done",
                ));
            }
            if let Err(error) = progress.delete() {
                crate::meow_warn_log!("download_complete", "sidecar delete failed: {}", error);
            }
            Ok(())
        }
        .await;

        // Always release the OS lock, but never let an unlock error mask the
        // content/checkpoint failure that made completion unsafe.
        let file_release = self.release_download_target_file_lock().await;
        let lease_release = self.release_download_target_lease();
        match (finalization, file_release, lease_release) {
            (Err(error), _, _) => Err(error),
            (Ok(()), Err(error), _) => Err(error),
            (Ok(()), Ok(()), Err(error)) => Err(error),
            (Ok(()), Ok(()), Ok(())) => Ok(()),
        }
    }
}

#[cfg(test)]
mod target_lock_error_tests {
    use super::map_download_target_lock_error;

    #[test]
    fn contention_is_an_invalid_task_state() {
        let error = map_download_target_lock_error(
            "lock target",
            std::io::Error::new(std::io::ErrorKind::WouldBlock, "owned elsewhere"),
        );

        assert_eq!(error.code(), crate::InnerErrorCode::InvalidTaskState as i32);
    }

    #[test]
    fn non_contention_uses_normal_io_classification() {
        let permission = map_download_target_lock_error(
            "open target",
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
        );
        let missing_parent = map_download_target_lock_error(
            "open target",
            std::io::Error::new(std::io::ErrorKind::NotFound, "gone"),
        );

        assert_eq!(permission.code(), crate::InnerErrorCode::IoError as i32);
        assert_eq!(
            missing_parent.code(),
            crate::InnerErrorCode::LocalFileRemoved as i32
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn out_of_space_preserves_disk_full() {
        #[cfg(unix)]
        let raw_error = 28;
        #[cfg(windows)]
        let raw_error = 112;

        let error = map_download_target_lock_error(
            "create target ownership file",
            std::io::Error::from_raw_os_error(raw_error),
        );

        assert_eq!(error.code(), crate::InnerErrorCode::DiskFull as i32);
    }
}

#[cfg(test)]
mod checkpoint_timer_tests {
    use super::{arm_download_checkpoint_timer, TransferTask};
    use crate::dflt::download_progress::{sidecar_path, DownloadProgress};
    use crate::direction::Direction;
    use crate::http_breakpoint::{
        BreakpointDownloadHttpConfig, DefaultStyleUpload, StandardRangeDownload,
    };
    use reqwest::{header::HeaderMap, Method};
    use std::sync::{Arc, Mutex};

    fn download_task_for(target: &std::path::Path) -> TransferTask {
        TransferTask {
            file_sign: Arc::<str>::from("positioned-write-test"),
            file_name: Arc::<str>::from("positioned-write-test.bin"),
            file_path: target.to_path_buf(),
            upload_source: None,
            upload_file_snapshot: None,
            direction: Direction::Download,
            total_size: 16,
            chunk_size: 8,
            url: "http://127.0.0.1/positioned-write-test".to_owned(),
            method: Method::GET,
            headers: HeaderMap::new(),
            breakpoint_download_http: BreakpointDownloadHttpConfig::default(),
            breakpoint_upload: Arc::new(DefaultStyleUpload::default()),
            breakpoint_download: Arc::new(StandardRangeDownload),
            http_client: None,
            download_file_slot: Arc::new(tokio::sync::Mutex::new(None)),
            download_checkpoint_barrier: Arc::new(tokio::sync::Mutex::new(())),
            transfer_lifecycle: Arc::new(crate::inner::inner_task::TransferLifecycle::new()),
            target_lease: Arc::new(Mutex::new(None)),
            max_parts_in_flight: 2,
            download_progress: Arc::new(Mutex::new(None)),
            max_upload_prepare_retries: 0,
        }
    }

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
        let target_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&target)
            .expect("target handle");
        let file_slot = Arc::new(tokio::sync::Mutex::new(Some(tokio::fs::File::from_std(
            target_file,
        ))));
        let barrier = Arc::new(tokio::sync::Mutex::new(()));
        arm_download_checkpoint_timer(
            Arc::downgrade(&slot),
            Arc::downgrade(&file_slot),
            Arc::downgrade(&barrier),
        )
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

    #[tokio::test]
    async fn positioned_write_reports_local_file_removed_when_locked_target_was_truncated() {
        let target = std::env::temp_dir().join(format!(
            "rusty_cat_positioned_write_truncated_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::write(&target, vec![0_u8; 16]).expect("preallocate target");
        let task = download_task_for(&target);
        assert_eq!(
            task.ensure_download_target_file_locked()
                .await
                .expect("lock actual target"),
            16
        );
        {
            let mut slot = task.download_file_slot.lock().await;
            slot.as_mut()
                .expect("locked target handle")
                .set_len(4)
                .await
                .expect("truncate actual target before positioned write");
        }

        let error = task
            .write_and_stage_download_part(8, b"abcdefgh", [0_u8; 32])
            .await
            .expect_err("a part extending beyond the truncated target must fail");
        assert_eq!(error.code(), crate::InnerErrorCode::LocalFileRemoved as i32);

        task.release_download_target_file_lock()
            .await
            .expect("unlock actual target");
        let _ = std::fs::remove_file(target);
    }

    #[test]
    fn final_content_validation_uses_bytes_not_file_identity() {
        let target = std::env::temp_dir().join(format!(
            "rusty_cat_download_content_validation_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let original = b"same bytes across replacement";
        std::fs::write(&target, original).expect("target");
        let mut progress = DownloadProgress::load_or_create(
            &target,
            original.len() as u64,
            original.len() as u64,
            1,
            "content-identity-test",
        )
        .expect("progress");
        progress
            .mark_done_and_persist(0)
            .expect("persist content digest");

        let replacement = target.with_extension("replacement");
        std::fs::write(&replacement, original).expect("same-content replacement");
        std::fs::rename(&replacement, &target).expect("replace target");
        progress
            .validate_committed_content()
            .expect("identical bytes are the same content generation");

        std::fs::write(&target, vec![b'X'; original.len()]).expect("different bytes");
        let error = progress
            .validate_committed_content()
            .expect_err("same-length different content must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        let _ = std::fs::remove_file(sidecar_path(&target));
        let _ = std::fs::remove_file(target);
    }
}
