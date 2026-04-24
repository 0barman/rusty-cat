use crate::direction::Direction;
use crate::http_breakpoint::{BreakpointDownload, BreakpointDownloadHttpConfig};
use crate::pounce_task::PounceTask;
use reqwest::header::HeaderMap;
use reqwest::Method;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Builder for creating a download [`PounceTask`].
///
/// Use this builder when you need fine-grained control over URL, headers,
/// and breakpoint download behavior.
///
/// # HTTP method
///
/// Download HTTP methods are **not configurable**: the executor always issues
/// `HEAD` during the prepare stage and `GET` with `Range` headers during
/// chunk transfer. These are fixed by the HTTP Range protocol (RFC 7233) and
/// by the breakpoint-resume contract, so there is no builder knob for the
/// request method. If a gateway requires an unusual verb, implement a custom
/// [`BreakpointDownload`] instead.
pub struct DownloadPounceBuilder {
    /// Display file name used in callbacks and logs.
    file_name: String,
    /// Target local file path.
    file_path: PathBuf,
    /// Chunk size in bytes for each range request.
    ///
    /// Effective range: `>= 1`; zero is normalized to default (1 MiB).
    chunk_size: u64,
    /// Resource URL.
    url: String,
    /// Base headers copied into HEAD and range GET requests.
    headers: HeaderMap,
    /// Optional client-defined file signature shown in progress records.
    client_file_sign: Option<String>,
    /// Optional custom breakpoint download protocol implementation.
    breakpoint_download: Option<Arc<dyn BreakpointDownload + Send + Sync>>,
    /// Optional per-task HTTP behavior for range download.
    breakpoint_download_http: Option<BreakpointDownloadHttpConfig>,
    /// Maximum retry count per chunk transfer.
    ///
    /// Effective range: `>= 0`; `0` means "do not retry".
    max_chunk_retries: u32,
}

impl DownloadPounceBuilder {
    /// Creates a new download task builder.
    ///
    /// # Parameters
    ///
    /// - `file_name`: Non-empty display name for logs/callbacks.
    /// - `file_path`: Local output path.
    /// - `chunk_size`: Desired chunk size in bytes; `0` falls back to default.
    /// - `url`: Download URL.
    ///
    /// The HTTP method is fixed by protocol (`HEAD` for prepare, `GET` with
    /// `Range` for chunks) and is therefore not a parameter; see the
    /// [struct-level docs](DownloadPounceBuilder) for details.
    ///
    /// # Usage rules
    ///
    /// Duplicate task detection is based on direction + URL in scheduler logic.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::DownloadPounceBuilder;
    ///
    /// let task = DownloadPounceBuilder::new(
    ///     "demo.bin",
    ///     "./downloads/demo.bin",
    ///     1024 * 1024,
    ///     "https://example.com/demo.bin",
    /// )
    /// .build();
    /// let _ = task;
    /// ```
    pub fn new(
        file_name: impl Into<String>,
        file_path: impl AsRef<Path>,
        chunk_size: u64,
        url: impl Into<String>,
    ) -> Self {
        Self {
            file_name: file_name.into(),
            file_path: file_path.as_ref().to_path_buf(),
            chunk_size: PounceTask::normalized_chunk_size(chunk_size),
            url: url.into(),
            headers: HeaderMap::new(),
            client_file_sign: None,
            breakpoint_download: None,
            breakpoint_download_http: None,
            max_chunk_retries: PounceTask::DEFAULT_MAX_CHUNK_RETRIES,
        }
    }

    /// Overrides the request URL.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::DownloadPounceBuilder;
    ///
    /// let _task = DownloadPounceBuilder::new("a.bin", "./a.bin", 1024, "https://old")
    ///     .with_url("https://new")
    ///     .build();
    /// ```
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = url.into();
        self
    }

    /// Overrides target local file path.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::DownloadPounceBuilder;
    ///
    /// let _task = DownloadPounceBuilder::new("a.bin", "./a.bin", 1024, "https://x")
    ///     .with_file_path("./downloads/new-a.bin")
    ///     .build();
    /// ```
    pub fn with_file_path(mut self, path: impl AsRef<Path>) -> Self {
        self.file_path = path.as_ref().to_path_buf();
        self
    }

    /// Replaces base request headers.
    ///
    /// Provide authorization and other business headers here.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
    /// use rusty_cat::api::DownloadPounceBuilder;
    ///
    /// let mut headers = HeaderMap::new();
    /// headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer token"));
    ///
    /// let _task = DownloadPounceBuilder::new("a.bin", "./a.bin", 1024, "https://x")
    ///     .with_headers(headers)
    ///     .build();
    /// ```
    pub fn with_headers(mut self, headers: HeaderMap) -> Self {
        self.headers = headers;
        self
    }

    /// Sets optional client-side file signature for progress reporting.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::DownloadPounceBuilder;
    ///
    /// let _task = DownloadPounceBuilder::new("a.bin", "./a.bin", 1024, "https://x")
    ///     .with_client_file_sign("client-sign-001")
    ///     .build();
    /// ```
    pub fn with_client_file_sign(mut self, sign: impl Into<String>) -> Self {
        self.client_file_sign = Some(sign.into());
        self
    }

    /// Sets a custom breakpoint download implementation for this task only.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::sync::Arc;
    /// use rusty_cat::api::{DownloadPounceBuilder, StandardRangeDownload};
    ///
    /// let protocol = Arc::new(StandardRangeDownload);
    /// let _task = DownloadPounceBuilder::new("a.bin", "./a.bin", 1024, "https://x")
    ///     .with_breakpoint_download(protocol)
    ///     .build();
    /// ```
    pub fn with_breakpoint_download(
        mut self,
        download: Arc<dyn BreakpointDownload + Send + Sync>,
    ) -> Self {
        self.breakpoint_download = Some(download);
        self
    }

    /// Sets HTTP behavior config for breakpoint range download.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::{BreakpointDownloadHttpConfig, DownloadPounceBuilder};
    ///
    /// let _task = DownloadPounceBuilder::new("a.bin", "./a.bin", 1024, "https://x")
    ///     .with_breakpoint_download_http(BreakpointDownloadHttpConfig {
    ///         range_accept: "application/octet-stream".to_string(),
    ///     })
    ///     .build();
    /// ```
    pub fn with_breakpoint_download_http(mut self, config: BreakpointDownloadHttpConfig) -> Self {
        self.breakpoint_download_http = Some(config);
        self
    }

    /// Configures max retry attempts per chunk (default: `3`).
    ///
    /// # Range guidance
    ///
    /// - `0`: no retry.
    /// - `1..=10`: common production range.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::DownloadPounceBuilder;
    ///
    /// let _task = DownloadPounceBuilder::new("a.bin", "./a.bin", 1024, "https://x")
    ///     .with_max_chunk_retries(5)
    ///     .build();
    /// ```
    pub fn with_max_chunk_retries(mut self, retries: u32) -> Self {
        self.max_chunk_retries = PounceTask::normalized_max_chunk_retries(retries);
        self
    }

    /// Builds the final download [`PounceTask`].
    ///
    /// This operation is infallible; validation occurs during enqueue/runtime.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::DownloadPounceBuilder;
    ///
    /// let task = DownloadPounceBuilder::new(
    ///     "file.iso",
    ///     "./downloads/file.iso",
    ///     2 * 1024 * 1024,
    ///     "https://example.com/file.iso",
    /// )
    /// .build();
    /// let _ = task;
    /// ```
    pub fn build(self) -> PounceTask {
        PounceTask {
            direction: Direction::Download,
            file_name: self.file_name,
            file_path: self.file_path,
            upload_source: None,
            total_size: 0,
            chunk_size: self.chunk_size,
            url: self.url,
            // Download uses HEAD (prepare) + GET (chunks) hard-coded by the
            // executor; this field is a placeholder and is never consulted on
            // the download path.
            method: Method::GET,
            headers: self.headers,
            client_file_sign: self.client_file_sign,
            breakpoint_upload: None,
            breakpoint_download: self.breakpoint_download,
            breakpoint_download_http: self.breakpoint_download_http,
            max_chunk_retries: self.max_chunk_retries,
            max_upload_prepare_retries: PounceTask::DEFAULT_MAX_UPLOAD_PREPARE_RETRIES,
        }
    }
}
