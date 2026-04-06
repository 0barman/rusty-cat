use std::path::{Path, PathBuf};
use std::sync::Arc;

use reqwest::header::HeaderMap;
use reqwest::Method;
use tokio::fs::File;
use tokio::sync::Mutex;

use crate::direction::Direction;
use crate::http_breakpoint::{BreakpointDownload, BreakpointDownloadHttpConfig, BreakpointUpload};
use crate::inner::inner_task::InnerTask;

/// 对 [`crate::transfer_executor_trait::TransferTrait`] 暴露的任务快照：仅含查询接口，由内部从 [`InnerTask`] 填充。
#[derive(Clone)]
pub struct TransferTask {
    file_sign: String,
    file_name: String,
    file_path: PathBuf,
    direction: Direction,
    total_size: u64,
    chunk_size: u64,
    url: String,
    method: Method,
    headers: HeaderMap,
    breakpoint_download_http: BreakpointDownloadHttpConfig,
    breakpoint_upload: Arc<dyn BreakpointUpload + Send + Sync>,
    breakpoint_download: Arc<dyn BreakpointDownload + Send + Sync>,
    http_client: Option<reqwest::Client>,
    /// 任务级上传文件句柄槽位，避免每个 chunk 反复 open。
    upload_file_slot: Arc<Mutex<Option<File>>>,
    /// 任务级下载文件句柄槽位，避免每个 chunk 反复 open/create。
    download_file_slot: Arc<Mutex<Option<File>>>,
}

impl std::fmt::Debug for TransferTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransferTask")
            .field("file_sign", &self.file_sign)
            .field("file_name", &self.file_name)
            .field("file_path", &self.file_path)
            .field("direction", &self.direction)
            .field("total_size", &self.total_size)
            .field("chunk_size", &self.chunk_size)
            .field("url", &self.url)
            .field("method", &self.method)
            .field("headers", &self.headers)
            .field("breakpoint_upload", &"<dyn BreakpointUpload>")
            .field("breakpoint_download", &"<dyn BreakpointDownload>")
            .field("breakpoint_download_http", &self.breakpoint_download_http)
            .finish()
    }
}

impl TransferTask {
    pub(crate) fn from_inner(inner: &InnerTask) -> Self {
        Self {
            file_sign: inner.file_sign().to_string(),
            file_name: inner.file_name().to_string(),
            file_path: inner.file_path().to_path_buf(),
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
            download_file_slot: Arc::new(Mutex::new(None)),
        }
    }

    pub fn direction(&self) -> Direction {
        self.direction
    }

    pub fn total_size(&self) -> u64 {
        self.total_size
    }

    pub fn chunk_size(&self) -> u64 {
        self.chunk_size
    }

    pub fn file_sign(&self) -> &str {
        &self.file_sign
    }

    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    pub fn file_path(&self) -> &Path {
        &self.file_path
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn method(&self) -> Method {
        self.method.clone()
    }

    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    pub(crate) fn breakpoint_download_http(&self) -> Option<&BreakpointDownloadHttpConfig> {
        Some(&self.breakpoint_download_http)
    }

    pub(crate) fn breakpoint_upload(&self) -> Option<&Arc<dyn BreakpointUpload + Send + Sync>> {
        Some(&self.breakpoint_upload)
    }

    pub(crate) fn breakpoint_download(&self) -> Option<&Arc<dyn BreakpointDownload + Send + Sync>> {
        Some(&self.breakpoint_download)
    }

    pub(crate) fn http_client_ref(&self) -> Option<&reqwest::Client> {
        self.http_client.as_ref()
    }

    pub(crate) fn upload_file_slot(&self) -> &Arc<Mutex<Option<File>>> {
        &self.upload_file_slot
    }

    pub(crate) fn download_file_slot(&self) -> &Arc<Mutex<Option<File>>> {
        &self.download_file_slot
    }
}
