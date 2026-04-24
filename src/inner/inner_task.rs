use std::path::{Path, PathBuf};
use std::sync::Arc;

use reqwest::header::HeaderMap;
use reqwest::Method;
use tokio::fs::File;

use crate::direction::Direction;
use crate::error::{InnerErrorCode, MeowError};
use crate::http_breakpoint::{BreakpointDownload, BreakpointDownloadHttpConfig, BreakpointUpload};
use crate::ids::TaskId;
use crate::inner::sign::{calculate_sign, calculate_sign_bytes};
use crate::inner::UniqueId;
use crate::pounce_task::PounceTask;
use crate::upload_source::UploadSource;

/// 调度与 [`crate::transfer_executor_trait::TransferTrait`] 视图的内部任务；不对外暴露构造。
#[derive(Clone)]
pub(crate) struct InnerTask {
    task_id: TaskId,
    file_sign: Arc<str>,
    file_name: Arc<str>,
    file_path: PathBuf,
    upload_source: Option<UploadSource>,
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
        let task_id = TaskId::new_v4();
        crate::meow_flow_log!("inner_task", "from_pounce start: task_id={:?}", task_id);

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
        } = pounce;

        let upload_source = upload_source.or_else(|| {
            if direction == Direction::Upload {
                Some(UploadSource::File(file_path.clone()))
            } else {
                None
            }
        });

        let file_sign = match direction {
            Direction::Upload => {
                let source = upload_source.as_ref().ok_or_else(|| {
                    MeowError::from_code_str(
                        InnerErrorCode::ParameterEmpty,
                        "upload task missing upload source",
                    )
                })?;
                match source {
                    UploadSource::File(path) => {
                        crate::meow_flow_log!(
                            "inner_task",
                            "build upload task from file: task_id={:?} path={}",
                            task_id,
                            path.display()
                        );
                        let file = File::open(path).await.map_err(|e| {
                            if e.kind() == std::io::ErrorKind::NotFound {
                                crate::meow_flow_log!(
                                    "inner_task",
                                    "upload source missing: task_id={:?} path={} err={}",
                                    task_id,
                                    path.display(),
                                    e
                                );
                                MeowError::from_source(
                                    InnerErrorCode::FileNotFound,
                                    format!("upload source file not found: {}", path.display()),
                                    e,
                                )
                            } else {
                                crate::meow_flow_log!(
                                    "inner_task",
                                    "upload source open failed: task_id={:?} path={} err={}",
                                    task_id,
                                    path.display(),
                                    e
                                );
                                MeowError::from_source(
                                    InnerErrorCode::IoError,
                                    format!("open upload source failed: {}", path.display()),
                                    e,
                                )
                            }
                        })?;
                        calculate_sign(&file).await?
                    }
                    UploadSource::Bytes(bytes) => {
                        crate::meow_flow_log!(
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
                crate::meow_flow_log!(
                    "inner_task",
                    "build download task: task_id={:?} path={}",
                    task_id,
                    file_path.display()
                );
                client_file_sign.unwrap_or_default()
            }
        };

        let breakpoint_upload = breakpoint_upload.unwrap_or(default_upload);
        let breakpoint_download = breakpoint_download.unwrap_or(default_download);
        let breakpoint_download_http = breakpoint_download_http.unwrap_or(default_download_http);
        crate::meow_flow_log!(
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

        Ok(Self {
            task_id,
            file_sign: Arc::<str>::from(file_sign),
            file_name: Arc::<str>::from(file_name),
            file_path,
            upload_source,
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

    pub(crate) fn http_client_ref(&self) -> Option<&reqwest::Client> {
        self.http_client.as_ref()
    }
}
