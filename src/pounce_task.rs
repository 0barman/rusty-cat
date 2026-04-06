use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use reqwest::header::HeaderMap;
use reqwest::Method;

use crate::direction::Direction;
use crate::http_breakpoint::{BreakpointDownload, BreakpointDownloadHttpConfig, BreakpointUpload};

/// 仅承载调用方入参，由 [`UploadPounceBuilder`] / [`DownloadPounceBuilder`] 构造
#[derive(Clone)]
pub struct PounceTask {
    pub(crate) direction: Direction,
    pub(crate) file_name: String,
    pub(crate) file_path: PathBuf,
    pub(crate) total_size: u64,
    pub(crate) chunk_size: u64,
    pub(crate) url: String,
    pub(crate) method: Method,
    pub(crate) headers: HeaderMap,
    /// 下载任务在进度回调里展示的标识；上传任务忽略，由内部 MD5 填充。
    pub(crate) client_file_sign: Option<String>,
    pub(crate) breakpoint_upload: Option<Arc<dyn BreakpointUpload + Send + Sync>>,
    pub(crate) breakpoint_download: Option<Arc<dyn BreakpointDownload + Send + Sync>>,
    pub(crate) breakpoint_download_http: Option<BreakpointDownloadHttpConfig>,
    /// 每个分片的最大重试次数（仅在分片传输失败时生效，不包含 prepare 阶段）。
    pub(crate) max_chunk_retries: u32,
}

impl fmt::Debug for PounceTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PounceTask")
            .field("direction", &self.direction)
            .field("file_name", &self.file_name)
            .field("file_path", &self.file_path)
            .field("total_size", &self.total_size)
            .field("chunk_size", &self.chunk_size)
            .field("url", &self.url)
            .field("method", &self.method)
            .field("headers", &self.headers)
            .field("client_file_sign", &self.client_file_sign)
            .field(
                "breakpoint_upload",
                &self
                    .breakpoint_upload
                    .as_ref()
                    .map(|_| "Arc<dyn BreakpointUpload + Send + Sync>"),
            )
            .field(
                "breakpoint_download",
                &self
                    .breakpoint_download
                    .as_ref()
                    .map(|_| "Arc<dyn BreakpointDownload + Send + Sync>"),
            )
            .field("breakpoint_download_http", &self.breakpoint_download_http)
            .field("max_chunk_retries", &self.max_chunk_retries)
            .finish()
    }
}

impl PounceTask {
    /// 分片重试的默认最大次数：失败后最多再尝试 3 次。
    pub const DEFAULT_MAX_CHUNK_RETRIES: u32 = 3;

    pub(crate) fn normalized_chunk_size(chunk_size: u64) -> u64 {
        if chunk_size == 0 {
            1024 * 1024
        } else {
            chunk_size
        }
    }

    /// 规范化重试次数：
    /// - 0 表示“禁用重试”（仅执行一次）；
    /// - 其余值按调用方配置使用。
    pub(crate) fn normalized_max_chunk_retries(max_chunk_retries: u32) -> u32 {
        max_chunk_retries
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.file_path.as_os_str().is_empty()
            || self.file_name.is_empty()
            || self.url.is_empty()
            || match self.direction {
                Direction::Upload => self.total_size == 0,
                Direction::Download => false,
            }
    }
}
