//! 断点上传/下载的 HTTP 形态由业务决定：通过 [`BreakpointUpload`] / [`BreakpointDownload`]
//! 从外部构造 `multipart::Form`、合并 Header，便于作为第三方库接入不同后端。

use crate::error::{InnerErrorCode, MeowError};
use crate::transfer_task::TransferTask;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::multipart;

#[derive(Debug, Clone, Default)]
pub struct UploadResumeInfo {
    /// 若已上传完成，服务端返回的文件 ID。
    pub completed_file_id: Option<String>,
    /// 建议下一字节偏移（`nextByte`）。
    pub next_byte: Option<u64>,
}

/// 自定义断点上传：准备阶段表单、分块表单及响应解析。
pub trait BreakpointUpload: Send + Sync {
    /// 断点探测（无文件分块）使用的 multipart，例如仅含 fileMd5、fileName、totalSize 等。
    fn prepare_multipart(&self, task: &TransferTask) -> multipart::Form;

    /// 实际上传分块使用的 multipart。
    fn chunk_multipart(
        &self,
        task: &TransferTask,
        chunk: &[u8],
        offset: u64,
    ) -> Result<multipart::Form, MeowError>;

    /// 解析上传接口响应体（通常为 JSON）。
    fn parse_upload_response(&self, body: &str) -> Result<UploadResumeInfo, MeowError>;
}

#[derive(Debug, Clone)]
pub struct DefaultStyleUpload {
    pub category: String,
}

impl Default for DefaultStyleUpload {
    fn default() -> Self {
        Self {
            category: String::new(),
        }
    }
}

const KEY_FILE_MD5: &str = "fileMd5";
const KEY_FILE_NAME: &str = "fileName";
const KEY_CATEGORY: &str = "category";
const KEY_TOTAL_SIZE: &str = "totalSize";
const KEY_OFFSET: &str = "offset";
const KEY_PART_SIZE: &str = "partSize";
const KEY_FILE: &str = "file";
const KEY_UPLOAD_CHUNK_DATA: &str = "upload_chunk_data";

#[derive(serde::Deserialize)]
struct DefaultUploadResp {
    #[serde(rename = "fileId")]
    file_id: Option<String>,
    #[serde(rename = "nextByte")]
    next_byte: Option<i64>,
}

impl BreakpointUpload for DefaultStyleUpload {
    fn prepare_multipart(&self, task: &TransferTask) -> multipart::Form {
        multipart::Form::new()
            .text(KEY_FILE_MD5, task.file_sign().to_string())
            .text(KEY_FILE_NAME, task.file_name().to_string())
            .text(KEY_CATEGORY, self.category.clone())
            .text(KEY_TOTAL_SIZE, task.total_size().to_string())
    }

    fn chunk_multipart(
        &self,
        task: &TransferTask,
        chunk: &[u8],
        offset: u64,
    ) -> Result<multipart::Form, MeowError> {
        let part = multipart::Part::bytes(chunk.to_vec())
            .file_name(KEY_UPLOAD_CHUNK_DATA)
            .mime_str("application/octet-stream")
            .map_err(|e| MeowError::from_code(InnerErrorCode::HttpError, e.to_string()))?;

        Ok(multipart::Form::new()
            .part(KEY_FILE, part)
            .text(KEY_FILE_MD5, task.file_sign().to_string())
            .text(KEY_FILE_NAME, task.file_name().to_string())
            .text(KEY_CATEGORY, self.category.clone())
            .text(KEY_OFFSET, offset.to_string())
            .text(KEY_PART_SIZE, chunk.len().to_string())
            .text(KEY_TOTAL_SIZE, task.total_size().to_string()))
    }

    fn parse_upload_response(&self, body: &str) -> Result<UploadResumeInfo, MeowError> {
        if body.trim().is_empty() {
            crate::meow_flow_log!(
                "upload_protocol",
                "empty upload response body, fallback default"
            );
            return Ok(UploadResumeInfo::default());
        }
        let v: DefaultUploadResp = serde_json::from_str(body).map_err(|e| {
            crate::meow_flow_log!(
                "upload_protocol",
                "upload response parse failed: body_len={} err={}",
                body.len(),
                e
            );
            MeowError::from_code(
                InnerErrorCode::ResponseParseError,
                format!("upload response json: {e}, body: {body}"),
            )
        })?;
        crate::meow_flow_log!(
            "upload_protocol",
            "upload response parsed: file_id_present={} next_byte={:?}",
            v.file_id.is_some(),
            v.next_byte
        );
        Ok(UploadResumeInfo {
            completed_file_id: v.file_id,
            next_byte: v.next_byte.map(|n| if n < 0 { 0u64 } else { n as u64 }),
        })
    }
}

/// 断点下载中与 HTTP 请求相关的可配置项（Range GET 的 Accept、HEAD 行为等），通常放在
/// [`TransferTask`] 上由调用方传入；未设置时由 [`crate::meow_config::MeowConfig`] 在入队时补齐。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BreakpointDownloadHttpConfig {
    /// 分片 GET（带 `Range`）使用的 `Accept` 头。
    pub range_accept: String,
}

impl Default for BreakpointDownloadHttpConfig {
    fn default() -> Self {
        Self {
            range_accept: DEFAULT_RANGE_ACCEPT.to_string(),
        }
    }
}

const DEFAULT_RANGE_ACCEPT: &str = "application/octet-stream";

/// 自定义断点下载：HEAD 与带 Range 的 GET 如何拼 URL / Header、如何从 HEAD 取总长度。
pub trait BreakpointDownload: Send + Sync {
    /// HEAD 请求 URL，默认与任务 URL 相同。
    fn head_url(&self, task: &TransferTask) -> String {
        task.url().to_string()
    }

    /// 将 HEAD 专用头并入 `base`（`base` 已含 `TransferTask` 上的头）。
    fn merge_head_headers(&self, _task: &TransferTask, _base: &mut HeaderMap) {}

    /// 将分片 GET 专用头并入 `base`；`range_value` 形如 `bytes=0-1048575`。
    /// `Accept` 取自 [`TransferTask`] 上的断点下载 HTTP 配置（未设置时用 [`BreakpointDownloadHttpConfig`] 默认值）。
    fn merge_range_get_headers(
        &self,
        task: &TransferTask,
        range_value: &str,
        base: &mut HeaderMap,
    ) {
        let _ = self;
        insert_header(base, "Range", range_value);
        let accept = task
            .breakpoint_download_http()
            .map(|c| c.range_accept.as_str())
            .unwrap_or(DEFAULT_RANGE_ACCEPT);
        insert_header(base, "Accept", accept);
    }

    /// 从 HEAD 响应头解析资源总字节数。
    fn total_size_from_head(&self, headers: &HeaderMap) -> Result<u64, MeowError> {
        headers
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|&n| n > 0)
            .ok_or_else(|| {
                MeowError::from_code_str(
                    InnerErrorCode::MissingOrInvalidContentLengthFromHead,
                    "missing or invalid content-length from HEAD",
                )
            })
    }
}

fn insert_header(map: &mut HeaderMap, name: &str, value: &str) {
    if let (Ok(n), Ok(v)) = (
        HeaderName::from_bytes(name.as_bytes()),
        HeaderValue::from_str(value),
    ) {
        map.insert(n, v);
    }
}

/// 默认 Range 下载：仅设置 Range / Accept，总长度取自 `Content-Length`。
#[derive(Debug, Clone, Default)]
pub struct StandardRangeDownload;

impl BreakpointDownload for StandardRangeDownload {}
