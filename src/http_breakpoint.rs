//! 断点上传/下载协议插件：
//! - 执行器只做并发调度、分片读写、重试、进度、暂停恢复；
//! - 协议插件只负责业务语义（初始化、分片请求、完成/取消、响应解析）。

use async_trait::async_trait;
use crate::error::{InnerErrorCode, MeowError};
use crate::transfer_task::TransferTask;

pub use crate::download_trait::{BreakpointDownload, DownloadHeadCtx, DownloadRangeGetCtx};
pub use crate::upload_trait::{BreakpointUpload, UploadChunkCtx, UploadPrepareCtx};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::multipart;
use reqwest::Method;

#[derive(Debug, Clone, Default)]
pub struct UploadResumeInfo {
    /// 若已上传完成，服务端返回的文件 ID。
    pub completed_file_id: Option<String>,
    /// 建议下一字节偏移（`nextByte`）。
    pub next_byte: Option<u64>,
}

/// 一次上传 HTTP 请求体。
#[derive(Debug)]
pub enum UploadBody {
    Multipart(multipart::Form),
    Binary(Vec<u8>),
}

/// 协议插件返回给执行器的上传请求描述。
#[derive(Debug)]
pub struct UploadRequest {
    pub method: Method,
    pub url: String,
    pub headers: HeaderMap,
    pub body: UploadBody,
}

impl UploadRequest {
    pub fn from_task(task: &TransferTask, body: UploadBody) -> Self {
        Self {
            method: task.method(),
            url: task.url().to_string(),
            headers: task.headers().clone(),
            body,
        }
    }
}


fn parse_default_upload_response(body: &str) -> Result<UploadResumeInfo, MeowError> {
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

async fn send_upload_request(
    client: &reqwest::Client,
    req: UploadRequest,
) -> Result<String, MeowError> {
    let mut builder = client.request(req.method, req.url).headers(req.headers);
    builder = match req.body {
        UploadBody::Multipart(form) => builder.multipart(form),
        UploadBody::Binary(bytes) => builder.body(bytes),
    };
    let resp = builder.send().await.map_err(map_reqwest)?;
    let status = resp.status();
    let body = resp.text().await.map_err(map_reqwest)?;
    if !status.is_success() {
        return Err(MeowError::from_code(
            InnerErrorCode::ResponseStatusError,
            format!("upload HTTP {status}: {body}"),
        ));
    }
    Ok(body)
}

fn map_reqwest(e: reqwest::Error) -> MeowError {
    MeowError::from_source(InnerErrorCode::HttpError, e.to_string(), e)
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

#[async_trait]
impl BreakpointUpload for DefaultStyleUpload {
    async fn prepare(&self, ctx: UploadPrepareCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        let form = multipart::Form::new()
            .text(KEY_FILE_MD5, ctx.task.file_sign().to_string())
            .text(KEY_FILE_NAME, ctx.task.file_name().to_string())
            .text(KEY_CATEGORY, self.category.clone())
            .text(KEY_TOTAL_SIZE, ctx.task.total_size().to_string());
        let req = UploadRequest::from_task(ctx.task, UploadBody::Multipart(form));
        let body = send_upload_request(ctx.client, req).await?;
        parse_default_upload_response(&body)
    }

    async fn upload_chunk(&self, ctx: UploadChunkCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        let part = multipart::Part::bytes(ctx.chunk.to_vec())
            .file_name(KEY_UPLOAD_CHUNK_DATA)
            .mime_str("application/octet-stream")
            .map_err(|e| MeowError::from_code(InnerErrorCode::HttpError, e.to_string()))?;

        let form = multipart::Form::new()
            .part(KEY_FILE, part)
            .text(KEY_FILE_MD5, ctx.task.file_sign().to_string())
            .text(KEY_FILE_NAME, ctx.task.file_name().to_string())
            .text(KEY_CATEGORY, self.category.clone())
            .text(KEY_OFFSET, ctx.offset.to_string())
            .text(KEY_PART_SIZE, ctx.chunk.len().to_string())
            .text(KEY_TOTAL_SIZE, ctx.task.total_size().to_string());
        let req = UploadRequest::from_task(ctx.task, UploadBody::Multipart(form));
        let body = send_upload_request(ctx.client, req).await?;
        parse_default_upload_response(&body)
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

pub(crate) const DEFAULT_RANGE_ACCEPT: &str = "application/octet-stream";

pub(crate) fn insert_header(map: &mut HeaderMap, name: &str, value: &str) {
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
