use crate::direction::Direction;
use crate::http_breakpoint::BreakpointUpload;
use crate::pounce_task::PounceTask;
use reqwest::header::HeaderMap;
use reqwest::Method;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct UploadPounceBuilder {
    file_name: String,
    file_path: PathBuf,
    chunk_size: u64,
    url: String,
    method: Method,
    headers: HeaderMap,
    breakpoint_upload: Option<Arc<dyn BreakpointUpload + Send + Sync>>,
    /// 每个分片的最大重试次数（仅对 chunk 传输生效）。
    max_chunk_retries: u32,
}

impl UploadPounceBuilder {
    /// 默认 `POST`，`url` 需再通过 [`Self::with_url`] 设置。
    pub fn new(file_name: impl Into<String>, file_path: impl AsRef<Path>, chunk_size: u64) -> Self {
        Self {
            file_name: file_name.into(),
            file_path: file_path.as_ref().to_path_buf(),
            chunk_size: PounceTask::normalized_chunk_size(chunk_size),
            url: String::new(),
            method: Method::POST,
            headers: HeaderMap::new(),
            breakpoint_upload: None,
            max_chunk_retries: PounceTask::DEFAULT_MAX_CHUNK_RETRIES,
        }
    }

    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = url.into();
        self
    }

    pub fn with_file_path(mut self, path: impl AsRef<Path>) -> Self {
        self.file_path = path.as_ref().to_path_buf();
        self
    }

    pub fn with_method(mut self, method: Method) -> Self {
        self.method = method;
        self
    }

    pub fn with_headers(mut self, headers: HeaderMap) -> Self {
        self.headers = headers;
        self
    }

    pub fn with_breakpoint_upload(
        mut self,
        upload: Arc<dyn BreakpointUpload + Send + Sync>,
    ) -> Self {
        self.breakpoint_upload = Some(upload);
        self
    }

    /// 配置每个分片的最大重试次数（默认 3）。
    pub fn with_max_chunk_retries(mut self, retries: u32) -> Self {
        self.max_chunk_retries = PounceTask::normalized_max_chunk_retries(retries);
        self
    }

    /// 根据当前 `file_path` 读取 [`std::fs::metadata`] 得到 `total_size`。
    pub fn build(self) -> io::Result<PounceTask> {
        let total_size = std::fs::metadata(&self.file_path)?.len();
        Ok(PounceTask {
            direction: Direction::Upload,
            file_name: self.file_name,
            file_path: self.file_path,
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
        })
    }
}
