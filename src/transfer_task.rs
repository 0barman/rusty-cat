use std::path::{Path, PathBuf};
use std::sync::Arc;

use reqwest::header::HeaderMap;
use reqwest::Method;
use tokio::fs::File;
use tokio::sync::Mutex;

use crate::direction::Direction;
use crate::http_breakpoint::{BreakpointDownload, BreakpointDownloadHttpConfig, BreakpointUpload};
use crate::inner::inner_task::InnerTask;
use crate::upload_source::UploadSource;

/// Immutable task snapshot exposed to transfer executor implementations.
///
/// This type is constructed from the crate-internal scheduler task state and
/// intentionally exposes read-only accessors.
#[derive(Clone)]
pub struct TransferTask {
    /// Stable file signature.
    file_sign: String,
    /// Display file name.
    file_name: String,
    /// Local file path.
    file_path: PathBuf,
    /// Upload-only source descriptor.
    upload_source: Option<UploadSource>,
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
    /// Task-level upload file handle slot to avoid reopening per chunk.
    upload_file_slot: Arc<Mutex<Option<File>>>,
    /// Reused read buffer for upload chunks (same task, sequential chunks).
    upload_chunk_buf: Arc<Mutex<Vec<u8>>>,
    /// Task-level download file handle slot to avoid reopening per chunk.
    download_file_slot: Arc<Mutex<Option<File>>>,
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
            file_sign: inner.file_sign().to_string(),
            file_name: inner.file_name().to_string(),
            file_path: inner.file_path().to_path_buf(),
            upload_source: inner.upload_source().cloned(),
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
            upload_file_slot: Arc::new(Mutex::new(None)),
            upload_chunk_buf: Arc::new(Mutex::new(Vec::new())),
            download_file_slot: Arc::new(Mutex::new(None)),
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

    /// Returns upload file handle slot used by executor.
    pub(crate) fn upload_file_slot(&self) -> &Arc<Mutex<Option<File>>> {
        &self.upload_file_slot
    }

    /// Returns per-task upload chunk read buffer (executor-internal reuse).
    pub(crate) fn upload_chunk_buf(&self) -> &Arc<Mutex<Vec<u8>>> {
        &self.upload_chunk_buf
    }

    /// Returns download file handle slot used by executor.
    pub(crate) fn download_file_slot(&self) -> &Arc<Mutex<Option<File>>> {
        &self.download_file_slot
    }
}
