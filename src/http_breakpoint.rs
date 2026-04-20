//! Breakpoint upload/download protocol plugins.
//!
//! - The executor handles scheduling, chunk I/O, retries, progress, and state.
//! - Protocol plugins handle business-specific request/response semantics.

use crate::error::{InnerErrorCode, MeowError};
use crate::transfer_task::TransferTask;
use async_trait::async_trait;

pub use crate::download_trait::{BreakpointDownload, DownloadHeadCtx, DownloadRangeGetCtx};
pub use crate::upload_trait::{BreakpointUpload, UploadChunkCtx, UploadPrepareCtx};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::multipart;
use reqwest::Method;

#[derive(Debug, Clone, Default)]
pub struct UploadResumeInfo {
    /// File ID returned by server when upload is already complete.
    pub completed_file_id: Option<String>,
    /// Suggested next byte offset (`nextByte`) from server.
    ///
    /// Range: `>= 0`.
    pub next_byte: Option<u64>,
}

/// Upload HTTP request body payload.
#[derive(Debug)]
pub enum UploadBody {
    Multipart(multipart::Form),
    Binary(Vec<u8>),
}

/// Upload request description returned by protocol plugin.
#[derive(Debug)]
pub struct UploadRequest {
    /// HTTP method for upload call.
    pub method: Method,
    /// Target request URL.
    pub url: String,
    /// Request headers.
    pub headers: HeaderMap,
    /// Body payload.
    pub body: UploadBody,
}

impl UploadRequest {
    /// Creates an upload request using URL/method/headers from task.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::{TransferTask, UploadBody, UploadRequest};
    ///
    /// fn make_request(task: &TransferTask, bytes: Vec<u8>) {
    ///     let req = UploadRequest::from_task(task, UploadBody::Binary(bytes));
    ///     let _ = req;
    /// }
    /// ```
    pub fn from_task(task: &TransferTask, body: UploadBody) -> Self {
        Self {
            method: task.method(),
            url: task.url().to_string(),
            headers: task.headers().clone(),
            body,
        }
    }
}

/// Parses default upload response JSON payload into [`UploadResumeInfo`].
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

/// Sends upload request and returns response body as string.
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

/// Maps `reqwest` errors into SDK errors.
fn map_reqwest(e: reqwest::Error) -> MeowError {
    MeowError::from_source(InnerErrorCode::HttpError, e.to_string(), e)
}

#[derive(Debug, Clone)]
pub struct DefaultStyleUpload {
    /// Optional business category sent in multipart form.
    pub category: String,
}

impl Default for DefaultStyleUpload {
    fn default() -> Self {
        Self {
            category: String::new(),
        }
    }
}

/// Multipart field key: file md5/signature.
const KEY_FILE_MD5: &str = "fileMd5";
/// Multipart field key: file name.
const KEY_FILE_NAME: &str = "fileName";
/// Multipart field key: business category.
const KEY_CATEGORY: &str = "category";
/// Multipart field key: total file size.
const KEY_TOTAL_SIZE: &str = "totalSize";
/// Multipart field key: current chunk offset.
const KEY_OFFSET: &str = "offset";
/// Multipart field key: current chunk byte length.
const KEY_PART_SIZE: &str = "partSize";
/// Multipart field key: file binary part.
const KEY_FILE: &str = "file";
/// Multipart part file name used for chunk payload.
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
    /// Sends prepare request for default multipart upload protocol.
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

    /// Sends one upload chunk for default multipart upload protocol.
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

/// HTTP behavior config for breakpoint range download.
///
/// This is usually provided by caller in task config. If missing, enqueue logic
/// fills it from [`crate::meow_config::MeowConfig`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BreakpointDownloadHttpConfig {
    /// `Accept` header used by range GET chunk requests.
    ///
    /// Typical value: `application/octet-stream`.
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

/// Inserts header pair into map when both name/value are valid.
pub(crate) fn insert_header(map: &mut HeaderMap, name: &str, value: &str) {
    if let (Ok(n), Ok(v)) = (
        HeaderName::from_bytes(name.as_bytes()),
        HeaderValue::from_str(value),
    ) {
        map.insert(n, v);
    }
}

/// Default range download protocol.
///
/// It sets `Range` and `Accept` headers and reads total size from
/// `Content-Length`.
#[derive(Debug, Clone, Default)]
pub struct StandardRangeDownload;

impl BreakpointDownload for StandardRangeDownload {}
