use crate::direction::Direction;
use crate::http_breakpoint::BreakpointUpload;
use crate::pounce_task::PounceTask;
use crate::upload_source::UploadSource;
use reqwest::header::HeaderMap;
use reqwest::Method;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Builder for creating an upload [`PounceTask`].
pub struct UploadPounceBuilder {
    /// Display file name used in logs and callbacks.
    file_name: String,
    /// Upload byte source.
    upload_source: UploadSource,
    /// Chunk size in bytes for each upload request.
    ///
    /// Effective range: `>= 1`; zero is normalized to default (1 MiB).
    chunk_size: u64,
    /// Target upload URL.
    url: String,
    /// HTTP method used for upload requests.
    method: Method,
    /// Base request headers for upload requests.
    headers: HeaderMap,
    /// Optional per-task custom breakpoint upload implementation.
    breakpoint_upload: Option<Arc<dyn BreakpointUpload + Send + Sync>>,
    /// Maximum retry count per chunk transfer.
    ///
    /// Effective range: `>= 0`; `0` means "do not retry".
    max_chunk_retries: u32,
    /// Maximum retry count after the first failed upload prepare (`BreakpointUpload::prepare`).
    ///
    /// Effective range: `>= 0`; `0` means "do not retry prepare".
    max_upload_prepare_retries: u32,
}

impl UploadPounceBuilder {
    /// Creates a new upload builder.
    ///
    /// Defaults:
    /// - method: `POST`
    /// - URL: empty, must be set with [`Self::with_url`]
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::UploadPounceBuilder;
    ///
    /// let builder = UploadPounceBuilder::new("demo.bin", "./demo.bin", 1024 * 1024);
    /// let _ = builder;
    /// ```
    pub fn new(file_name: impl Into<String>, file_path: impl AsRef<Path>, chunk_size: u64) -> Self {
        Self {
            file_name: file_name.into(),
            upload_source: UploadSource::File(file_path.as_ref().to_path_buf()),
            chunk_size: PounceTask::normalized_chunk_size(chunk_size),
            url: String::new(),
            method: Method::POST,
            headers: HeaderMap::new(),
            breakpoint_upload: None,
            max_chunk_retries: PounceTask::DEFAULT_MAX_CHUNK_RETRIES,
            max_upload_prepare_retries: PounceTask::DEFAULT_MAX_UPLOAD_PREPARE_RETRIES,
        }
    }

    /// Creates a new upload builder from in-memory bytes.
    ///
    /// The payload is moved into `Arc<Vec<u8>>` to avoid extra copies when task
    /// values are cloned across runtime layers.
    pub fn from_bytes(file_name: impl Into<String>, bytes: Vec<u8>, chunk_size: u64) -> Self {
        Self {
            file_name: file_name.into(),
            upload_source: UploadSource::Bytes(Arc::new(bytes)),
            chunk_size: PounceTask::normalized_chunk_size(chunk_size),
            url: String::new(),
            method: Method::POST,
            headers: HeaderMap::new(),
            breakpoint_upload: None,
            max_chunk_retries: PounceTask::DEFAULT_MAX_CHUNK_RETRIES,
            max_upload_prepare_retries: PounceTask::DEFAULT_MAX_UPLOAD_PREPARE_RETRIES,
        }
    }

    /// Sets upload URL.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::UploadPounceBuilder;
    ///
    /// let _builder = UploadPounceBuilder::new("a.bin", "./a.bin", 1024)
    ///     .with_url("https://upload.example.com/api/file");
    /// ```
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = url.into();
        self
    }

    /// Sets local file path.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::UploadPounceBuilder;
    ///
    /// let _builder = UploadPounceBuilder::new("a.bin", "./a.bin", 1024)
    ///     .with_file_path("./new-path/a.bin");
    /// ```
    pub fn with_file_path(mut self, path: impl AsRef<Path>) -> Self {
        self.upload_source = UploadSource::File(path.as_ref().to_path_buf());
        self
    }

    /// Sets upload source as in-memory bytes.
    ///
    /// This replaces previously configured file path source, if any.
    pub fn with_bytes(mut self, bytes: Vec<u8>) -> Self {
        self.upload_source = UploadSource::Bytes(Arc::new(bytes));
        self
    }

    /// Sets HTTP method used for upload.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use reqwest::Method;
    /// use rusty_cat::api::UploadPounceBuilder;
    ///
    /// let _builder = UploadPounceBuilder::new("a.bin", "./a.bin", 1024)
    ///     .with_method(Method::PUT);
    /// ```
    pub fn with_method(mut self, method: Method) -> Self {
        self.method = method;
        self
    }

    /// Replaces request headers.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
    /// use rusty_cat::api::UploadPounceBuilder;
    ///
    /// let mut headers = HeaderMap::new();
    /// headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer token"));
    ///
    /// let _builder = UploadPounceBuilder::new("a.bin", "./a.bin", 1024)
    ///     .with_headers(headers);
    /// ```
    pub fn with_headers(mut self, headers: HeaderMap) -> Self {
        self.headers = headers;
        self
    }

    /// Sets per-task custom breakpoint upload implementation.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::sync::Arc;
    /// use rusty_cat::api::{DefaultStyleUpload, UploadPounceBuilder};
    ///
    /// let upload_protocol = Arc::new(DefaultStyleUpload::default());
    /// let _builder = UploadPounceBuilder::new("a.bin", "./a.bin", 1024)
    ///     .with_breakpoint_upload(upload_protocol);
    /// ```
    pub fn with_breakpoint_upload(
        mut self,
        upload: Arc<dyn BreakpointUpload + Send + Sync>,
    ) -> Self {
        self.breakpoint_upload = Some(upload);
        self
    }

    /// Configures max retry attempts per upload chunk (default: `3`).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::UploadPounceBuilder;
    ///
    /// let _builder = UploadPounceBuilder::new("a.bin", "./a.bin", 1024)
    ///     .with_max_chunk_retries(4);
    /// ```
    pub fn with_max_chunk_retries(mut self, retries: u32) -> Self {
        self.max_chunk_retries = PounceTask::normalized_max_chunk_retries(retries);
        self
    }

    /// Configures max retry attempts after the first failed upload prepare (default: `3`).
    ///
    /// Applies only to the upload prepare stage (`BreakpointUpload::prepare`), not chunk transfer.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::UploadPounceBuilder;
    ///
    /// let _builder = UploadPounceBuilder::new("a.bin", "./a.bin", 1024)
    ///     .with_max_upload_prepare_retries(5);
    /// ```
    pub fn with_max_upload_prepare_retries(mut self, retries: u32) -> Self {
        self.max_upload_prepare_retries =
            PounceTask::normalized_max_upload_prepare_retries(retries);
        self
    }

    /// Builds upload [`PounceTask`].
    ///
    /// # Errors
    ///
    /// Returns `io::Error` if metadata cannot be read from file-path source.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::UploadPounceBuilder;
    ///
    /// let task = UploadPounceBuilder::new("demo.bin", "./demo.bin", 1024 * 1024)
    ///     .with_url("https://upload.example.com/files")
    ///     .build()?;
    /// let _ = task;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn build(self) -> io::Result<PounceTask> {
        let (file_path, total_size) = match &self.upload_source {
            UploadSource::File(path) => (path.clone(), std::fs::metadata(path)?.len()),
            UploadSource::Bytes(bytes) => (PathBuf::from(&self.file_name), bytes.len() as u64),
        };
        Ok(PounceTask {
            direction: Direction::Upload,
            file_name: self.file_name,
            file_path,
            upload_source: Some(self.upload_source),
            total_size,
            chunk_size: self.chunk_size,
            url: self.url,
            method: self.method,
            headers: self.headers,
            client_file_sign: None,
            breakpoint_upload: self.breakpoint_upload,
            breakpoint_download: None,
            breakpoint_download_http: None,
            max_chunk_retries: self.max_chunk_retries,
            max_upload_prepare_retries: self.max_upload_prepare_retries,
        })
    }
}
