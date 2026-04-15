# rusty-cat

## AliOssDownload
```rust
use crate::{header_value, OSS_UNSIGNED_PAYLOAD};
use reqwest::header::{HeaderMap, ACCEPT, RANGE};
use reqwest::Url;
use rusty_cat::http_breakpoint::{BreakpointDownload, DownloadHeadCtx, DownloadRangeGetCtx};
use rusty_cat::{InnerErrorCode, MeowError, TransferTask};
use crate::utils::{oss_auth_v4, time_util};
use crate::utils::oss_auth_v4::{OssV4Credentials, OSS_UNSIGNED_PAYLOAD};

const DEFAULT_RANGE_ACCEPT: &str = "application/octet-stream";

#[derive(Clone)]
pub struct AliOssDownload {
    canonical_uri: String,
    access_key_id: String,
    access_key_secret: String,
    region: String,
}

impl AliOssDownload {
    pub fn new(
        canonical_uri: impl Into<String>,
        access_key_id: impl Into<String>,
        access_key_secret: impl Into<String>,
        region: impl Into<String>,
    ) -> Self {
        Self {
            canonical_uri: canonical_uri.into(),
            access_key_id: access_key_id.into(),
            access_key_secret: access_key_secret.into(),
            region: region.into(),
        }
    }

    fn parse_bucket_from_canonical_uri(&self) -> Result<String, MeowError> {
        let path = self.canonical_uri.trim_start_matches('/');
        let (bucket, _) = path.split_once('/').ok_or_else(|| {
            MeowError::from_code(
                InnerErrorCode::ParameterEmpty,
                format!(
                    "invalid canonical_uri for OSS object: {}",
                    self.canonical_uri
                ),
            )
        })?;
        if bucket.is_empty() {
            return Err(MeowError::from_code(
                InnerErrorCode::ParameterEmpty,
                format!(
                    "invalid canonical_uri for OSS object: {}",
                    self.canonical_uri
                ),
            ));
        }
        Ok(bucket.to_string())
    }

    fn object_canonical_uri_from_task_url(&self, task: &TransferTask) -> Result<String, MeowError> {
        let bucket = self.parse_bucket_from_canonical_uri()?;
        let url = Url::parse(task.url()).map_err(|e| {
            MeowError::from_code(
                InnerErrorCode::ParameterEmpty,
                format!("invalid download url: {} ({e})", task.url()),
            )
        })?;
        Ok(format!("/{bucket}{}", url.path()))
    }

    /// 按 OSS V4 规则生成鉴权头并写入 `base`（不替换 `base` 里已有业务头）。
    fn apply_signed_headers(
        &self,
        task: &TransferTask,
        method: &str,
        base: &mut HeaderMap,
        sign_pairs: &[(&str, &str)],
        additional_signed: Option<&str>,
    ) -> Result<(), MeowError> {
        let iso8601 = time_util::now_iso8601_basic_z();
        let cred = OssV4Credentials::new(
            &self.access_key_id,
            &self.access_key_secret,
            &self.region,
            iso8601.as_str(),
        );

        let mut headers_for_sign: Vec<(&str, &str)> = Vec::with_capacity(sign_pairs.len() + 2);
        headers_for_sign.extend_from_slice(sign_pairs);
        headers_for_sign.push(("x-oss-date", iso8601.as_str()));
        headers_for_sign.push(("x-oss-content-sha256", OSS_UNSIGNED_PAYLOAD));

        let canonical_uri = self.object_canonical_uri_from_task_url(task)?;
        let auth = oss_auth_v4::generate_authorization_v4(
            method,
            canonical_uri.as_str(),
            None,
            &headers_for_sign,
            Some(OSS_UNSIGNED_PAYLOAD),
            additional_signed,
            cred,
        )
        .map_err(|code| {
            MeowError::from_code(
                InnerErrorCode::ParameterEmpty,
                format!("OSS V4 sign failed with code: {code}"),
            )
        })?;

        base.insert("x-oss-date", header_value(iso8601.as_str())?);
        base.insert("x-oss-content-sha256", header_value(OSS_UNSIGNED_PAYLOAD)?);
        let normalized_auth =
            if let Some((prefix, _)) = auth.authorization.as_str().split_once(",SignedHeaders=") {
                prefix.to_string()
            } else {
                auth.authorization
            };
        base.insert("authorization", header_value(normalized_auth.as_str())?);
        Ok(())
    }
}

impl BreakpointDownload for AliOssDownload {
    fn merge_head_headers(&self, ctx: DownloadHeadCtx<'_>) -> Result<(), MeowError> {
        self.apply_signed_headers(ctx.task, "HEAD", ctx.base, &[], None)?;
        Ok(())
    }

    fn merge_range_get_headers(&self, ctx: DownloadRangeGetCtx<'_>) -> Result<(), MeowError> {
        ctx.base.insert(RANGE, header_value(ctx.range_value)?);
        let _accept_val = match ctx.base.get(ACCEPT) {
            Some(v) => v.to_str().unwrap_or(DEFAULT_RANGE_ACCEPT).to_string(),
            None => {
                ctx.base.insert(ACCEPT, header_value(DEFAULT_RANGE_ACCEPT)?);
                DEFAULT_RANGE_ACCEPT.to_string()
            }
        };
        self.apply_signed_headers(ctx.task, "GET", ctx.base, &[], None)?;
        Ok(())
    }
}
```

## AliOssUpload
```rust
use crate::{header_value, OSS_UNSIGNED_PAYLOAD};
use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, CONTENT_LENGTH, CONTENT_TYPE, ETAG};
use reqwest::{Method, Url};
use rusty_cat::http_breakpoint::UploadResumeInfo;
use rusty_cat::upload_trait::{UploadChunkCtx, UploadPrepareCtx};
use rusty_cat::{BreakpointUpload, InnerErrorCode, MeowError, TransferTask};
use std::sync::Arc;
use tokio::sync::Mutex;
use crate::utils::oss_auth_v4::{OssV4Credentials, OSS_UNSIGNED_PAYLOAD};
use crate::utils::{oss_auth_v4, time_util};

#[derive(Debug, Default)]
struct MultipartSession {
    target_url: Option<String>,
    upload_id: Option<String>,
}

#[derive(Clone)]
pub struct AliOssUpload {
    canonical_uri: String,
    access_key_id: String,
    access_key_secret: String,
    region: String,
    session: Arc<Mutex<MultipartSession>>,
}

impl AliOssUpload {
    pub fn new(
        canonical_uri: impl Into<String>,
        access_key_id: impl Into<String>,
        access_key_secret: impl Into<String>,
        region: impl Into<String>,
    ) -> Self {
        Self {
            canonical_uri: canonical_uri.into(),
            access_key_id: access_key_id.into(),
            access_key_secret: access_key_secret.into(),
            region: region.into(),
            session: Arc::new(Mutex::new(MultipartSession::default())),
        }
    }

    fn parse_bucket_and_object_key(&self) -> Result<(String, String), MeowError> {
        let path = self.canonical_uri.trim_start_matches('/');
        let (bucket, object_key) = path.split_once('/').ok_or_else(|| {
            MeowError::from_code(
                InnerErrorCode::ParameterEmpty,
                format!(
                    "invalid canonical_uri for OSS object: {}",
                    self.canonical_uri
                ),
            )
        })?;
        if bucket.is_empty() || object_key.is_empty() {
            return Err(MeowError::from_code(
                InnerErrorCode::ParameterEmpty,
                format!(
                    "invalid canonical_uri for OSS object: {}",
                    self.canonical_uri
                ),
            ));
        }
        Ok((bucket.to_string(), object_key.to_string()))
    }

    fn bucket_canonical_uri(&self) -> Result<String, MeowError> {
        let (bucket, _) = self.parse_bucket_and_object_key()?;
        Ok(format!("/{bucket}/"))
    }

    /// OSS V4 签名用 Canonical URI：
    /// - 前缀必须带 bucket：`/{bucket}`
    /// - object path 必须使用 URL 中的编码形式（例如中文会是 `%E7...`）
    fn object_canonical_uri_from_task_url(&self, task: &TransferTask) -> Result<String, MeowError> {
        let (bucket, _) = self.parse_bucket_and_object_key()?;
        let url = Url::parse(task.url()).map_err(|e| {
            MeowError::from_code(
                InnerErrorCode::ParameterEmpty,
                format!("invalid upload url: {} ({e})", task.url()),
            )
        })?;
        Ok(format!("/{bucket}{}", url.path()))
    }

    fn normalize_authorization(raw: String) -> String {
        if let Some((prefix, _)) = raw.as_str().split_once(",SignedHeaders=") {
            prefix.to_string()
        } else {
            raw
        }
    }

    fn build_signed_headers(
        &self,
        method: &str,
        canonical_uri: &str,
        raw_query: Option<&str>,
        sign_pairs: &[(&str, &str)],
        additional_headers: Option<&str>,
    ) -> Result<HeaderMap, MeowError> {
        let iso8601 = time_util::now_iso8601_basic_z();
        let mut headers_for_sign: Vec<(&str, &str)> = Vec::with_capacity(sign_pairs.len() + 2);
        headers_for_sign.extend_from_slice(sign_pairs);
        headers_for_sign.push(("x-oss-date", iso8601.as_str()));
        headers_for_sign.push(("x-oss-content-sha256", OSS_UNSIGNED_PAYLOAD));

        let cred = OssV4Credentials::new(
            &self.access_key_id,
            &self.access_key_secret,
            &self.region,
            iso8601.as_str(),
        );
        let auth = oss_auth_v4::generate_authorization_v4(
            method,
            canonical_uri,
            raw_query,
            &headers_for_sign,
            Some(OSS_UNSIGNED_PAYLOAD),
            additional_headers,
            cred,
        )
        .map_err(|code| {
            MeowError::from_code(
                InnerErrorCode::ParameterEmpty,
                format!("OSS V4 sign failed with code: {code}"),
            )
        })?;

        let mut headers = HeaderMap::new();
        headers.insert("x-oss-date", header_value(iso8601.as_str())?);
        headers.insert("x-oss-content-sha256", header_value(OSS_UNSIGNED_PAYLOAD)?);
        for (k, v) in sign_pairs {
            let name = HeaderName::from_bytes(k.as_bytes()).map_err(|e| {
                MeowError::from_code(
                    InnerErrorCode::ParameterEmpty,
                    format!("invalid header name '{k}': {e}"),
                )
            })?;
            headers.insert(name, header_value(v)?);
        }
        headers.insert(
            "authorization",
            header_value(Self::normalize_authorization(auth.authorization).as_str())?,
        );
        Ok(headers)
    }

    async fn initiate_multipart_upload(
        &self,
        client: &reqwest::Client,
        task: &TransferTask,
    ) -> Result<String, MeowError> {
        let canonical_uri = self.object_canonical_uri_from_task_url(task)?;
        let mut url = Url::parse(task.url()).map_err(|e| {
            MeowError::from_code(
                InnerErrorCode::ParameterEmpty,
                format!("invalid upload url: {} ({e})", task.url()),
            )
        })?;
        url.set_query(Some("uploads"));
        let raw_query = url.query().unwrap_or("uploads");

        let headers =
            self.build_signed_headers("POST", &canonical_uri, Some(raw_query), &[], None)?;
        let resp = client
            .request(Method::POST, url)
            .headers(headers)
            .send()
            .await
            .map_err(|e| {
                MeowError::from_source(
                    InnerErrorCode::HttpError,
                    "oss initiate multipart failed",
                    e,
                )
            })?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(MeowError::from_code(
                InnerErrorCode::ResponseStatusError,
                format!("oss initiate multipart failed: {status}, body: {body}"),
            ));
        }
        let upload_id = extract_xml_tag(body.as_str(), "UploadId").ok_or_else(|| {
            MeowError::from_code(
                InnerErrorCode::ResponseParseError,
                format!("oss initiate multipart missing UploadId: {body}"),
            )
        })?;
        Ok(upload_id)
    }

    async fn try_adopt_upload_id_from_list(
        &self,
        client: &reqwest::Client,
        task: &TransferTask,
    ) -> Result<Option<String>, MeowError> {
        let (_, object_key) = self.parse_bucket_and_object_key()?;
        let bucket_uri = self.bucket_canonical_uri()?;
        let mut url = Url::parse(task.url()).map_err(|e| {
            MeowError::from_code(
                InnerErrorCode::ParameterEmpty,
                format!("invalid upload url: {} ({e})", task.url()),
            )
        })?;
        url.set_path("/");
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("uploads", "");
            pairs.append_pair("prefix", object_key.as_str());
            pairs.append_pair("max-uploads", "1000");
        }
        let raw_query = url.query().ok_or_else(|| {
            MeowError::from_code(
                InnerErrorCode::ParameterEmpty,
                "build list multipart query failed".to_string(),
            )
        })?;

        let headers =
            self.build_signed_headers("GET", bucket_uri.as_str(), Some(raw_query), &[], None)?;
        let resp = client
            .request(Method::GET, url)
            .headers(headers)
            .send()
            .await
            .map_err(|e| {
                MeowError::from_source(
                    InnerErrorCode::HttpError,
                    "oss list multipart uploads failed",
                    e,
                )
            })?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(MeowError::from_code(
                InnerErrorCode::ResponseStatusError,
                format!("oss list multipart uploads failed: {status}, body: {body}"),
            ));
        }
        let ids = extract_upload_ids_for_key(body.as_str(), object_key.as_str());
        if ids.len() > 1 {
            return Err(MeowError::from_code(
                InnerErrorCode::InvalidTaskState,
                format!(
                    "found multiple multipart sessions for object '{}', cannot choose one automatically",
                    object_key
                ),
            ));
        }
        Ok(ids.into_iter().next())
    }

    fn build_query_url(
        &self,
        task: &TransferTask,
        query_pairs: &[(&str, String)],
    ) -> Result<(Url, String), MeowError> {
        let mut url = Url::parse(task.url()).map_err(|e| {
            MeowError::from_code(
                InnerErrorCode::ParameterEmpty,
                format!("invalid upload url: {} ({e})", task.url()),
            )
        })?;
        {
            let mut pairs = url.query_pairs_mut();
            for (k, v) in query_pairs {
                pairs.append_pair(k, v.as_str());
            }
        }
        let query = url.query().map(|q| q.to_string()).ok_or_else(|| {
            MeowError::from_code(
                InnerErrorCode::ParameterEmpty,
                "build query url failed".to_string(),
            )
        })?;
        Ok((url, query))
    }
}

fn extract_xml_tag(body: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let (_, tail) = body.split_once(open.as_str())?;
    let (value, _) = tail.split_once(close.as_str())?;
    Some(value.trim().to_string())
}

fn extract_upload_ids_for_key(xml: &str, key: &str) -> Vec<String> {
    let mut ids = Vec::new();
    for seg in xml.split("<Upload>").skip(1) {
        if let Some((upload_block, _)) = seg.split_once("</Upload>") {
            let item_key = extract_xml_tag(upload_block, "Key");
            let upload_id = extract_xml_tag(upload_block, "UploadId");
            if item_key.as_deref() == Some(key) {
                if let Some(id) = upload_id {
                    ids.push(id);
                }
            }
        }
    }
    ids
}

#[async_trait]
impl BreakpointUpload for AliOssUpload {
    async fn prepare(&self, ctx: UploadPrepareCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        let client = ctx.client;
        let task = ctx.task;
        let local_offset = ctx.local_offset;
        {
            let mut state = self.session.lock().await;
            if state.target_url.as_deref() != Some(task.url()) {
                *state = MultipartSession {
                    target_url: Some(task.url().to_string()),
                    upload_id: None,
                };
            }
            if state.upload_id.is_some() {
                return Ok(UploadResumeInfo {
                    completed_file_id: None,
                    next_byte: Some(local_offset),
                });
            }
        }

        if local_offset > 0 {
            if let Some(upload_id) = self.try_adopt_upload_id_from_list(client, task).await? {
                let mut state = self.session.lock().await;
                state.target_url = Some(task.url().to_string());
                state.upload_id = Some(upload_id);
                return Ok(UploadResumeInfo {
                    completed_file_id: None,
                    next_byte: Some(local_offset),
                });
            }
            return Err(MeowError::from_code(
                InnerErrorCode::InvalidTaskState,
                "local offset > 0 but no OSS multipart session found; cannot safely resume"
                    .to_string(),
            ));
        }

        let upload_id = self.initiate_multipart_upload(client, task).await?;
        let mut state = self.session.lock().await;
        state.target_url = Some(task.url().to_string());
        state.upload_id = Some(upload_id);
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(0),
        })
    }

    async fn upload_chunk(&self, ctx: UploadChunkCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        let client = ctx.client;
        let task = ctx.task;
        let chunk = ctx.chunk;
        let offset = ctx.offset;
        let canonical_uri = self.object_canonical_uri_from_task_url(task)?;
        let upload_id = {
            let state = self.session.lock().await;
            state.upload_id.clone().ok_or_else(|| {
                MeowError::from_code(
                    InnerErrorCode::InvalidTaskState,
                    "multipart upload_id missing; call prepare first".to_string(),
                )
            })?
        };
        let part_number = (offset / task.chunk_size()) + 1;
        if part_number > 10_000 {
            return Err(MeowError::from_code(
                InnerErrorCode::InvalidRange,
                format!("partNumber out of range: {}", part_number),
            ));
        }

        let (url, raw_query) = self.build_query_url(
            task,
            &[
                ("partNumber", part_number.to_string()),
                ("uploadId", upload_id),
            ],
        )?;
        let headers =
            self.build_signed_headers("PUT", &canonical_uri, Some(raw_query.as_str()), &[], None)?;
        let resp = client
            .request(Method::PUT, url)
            .headers(headers)
            .body(chunk.to_vec())
            .send()
            .await
            .map_err(|e| {
                MeowError::from_source(InnerErrorCode::HttpError, "oss upload part failed", e)
            })?;
        let status = resp.status();
        let etag_present = resp.headers().get(ETAG).is_some();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(MeowError::from_code(
                InnerErrorCode::ResponseStatusError,
                format!("oss upload part failed: {status}, body: {body}"),
            ));
        }
        if !etag_present {
            return Err(MeowError::from_code(
                InnerErrorCode::ResponseParseError,
                "oss upload part success but missing ETag header".to_string(),
            ));
        }
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(offset + chunk.len() as u64),
        })
    }

    async fn complete_upload(
        &self,
        client: &reqwest::Client,
        task: &TransferTask,
    ) -> Result<(), MeowError> {
        let canonical_uri = self.object_canonical_uri_from_task_url(task)?;
        let upload_id = {
            let state = self.session.lock().await;
            state.upload_id.clone()
        };
        let Some(upload_id) = upload_id else {
            return Ok(());
        };

        let (url, raw_query) = self.build_query_url(task, &[("uploadId", upload_id.clone())])?;
        let mut headers = self.build_signed_headers(
            "POST",
            &canonical_uri,
            Some(raw_query.as_str()),
            [
                ("content-type", "application/xml"),
                ("x-oss-complete-all", "yes"),
            ]
            .as_slice(),
            None,
        )?;
        headers.insert(CONTENT_LENGTH, header_value("0")?);
        headers.insert(CONTENT_TYPE, header_value("application/xml")?);

        let resp = client
            .request(Method::POST, url)
            .headers(headers)
            .send()
            .await
            .map_err(|e| {
                MeowError::from_source(
                    InnerErrorCode::HttpError,
                    "oss complete multipart upload failed",
                    e,
                )
            })?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(MeowError::from_code(
                InnerErrorCode::ResponseStatusError,
                format!("oss complete multipart upload failed: {status}, body: {body}"),
            ));
        }

        let mut state = self.session.lock().await;
        state.upload_id = None;
        Ok(())
    }

    async fn abort_upload(
        &self,
        client: &reqwest::Client,
        task: &TransferTask,
    ) -> Result<(), MeowError> {
        let canonical_uri = self.object_canonical_uri_from_task_url(task)?;
        let upload_id = {
            let state = self.session.lock().await;
            state.upload_id.clone()
        };
        let Some(upload_id) = upload_id else {
            return Ok(());
        };

        let (url, raw_query) = self.build_query_url(task, &[("uploadId", upload_id.clone())])?;
        let headers = self.build_signed_headers(
            "DELETE",
            &canonical_uri,
            Some(raw_query.as_str()),
            &[],
            None,
        )?;
        let resp = client
            .request(Method::DELETE, url)
            .headers(headers)
            .send()
            .await
            .map_err(|e| {
                MeowError::from_source(
                    InnerErrorCode::HttpError,
                    "oss abort multipart upload failed",
                    e,
                )
            })?;
        if !(resp.status().is_success() || resp.status() == reqwest::StatusCode::NOT_FOUND) {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(MeowError::from_code(
                InnerErrorCode::ResponseStatusError,
                format!("oss abort multipart upload failed: {status}, body: {body}"),
            ));
        }

        let mut state = self.session.lock().await;
        state.upload_id = None;
        Ok(())
    }
}
```


## AzureDownload
```rust
//! Azure Block Blob 断点分片下载（HEAD + Range GET）。
//!
//! 执行器负责分片循环；本实现只负责：
//! - 给 HEAD / GET-Range 请求补齐 SharedKey 鉴权头；
//! - 确保 Range/Accept 头符合分片下载预期。

use reqwest::header::{HeaderMap, ACCEPT, RANGE};
use reqwest::Url;
use rusty_cat::http_breakpoint::{BreakpointDownload, DownloadHeadCtx, DownloadRangeGetCtx};
use rusty_cat::{InnerErrorCode, MeowError, TransferTask};

use super::azure_shared_key::{
    build_authorization, insert_header, now_rfc1123_gmt, HEADER_AUTHORIZATION, HEADER_MS_DATE,
    HEADER_MS_VERSION, MS_VERSION,
};

const DEFAULT_RANGE_ACCEPT: &str = "application/octet-stream";

#[derive(Clone)]
pub struct AzureDownload {
    account_name: String,
    account_key_b64: String,
}

impl AzureDownload {
    pub fn new(account_name: impl Into<String>, account_key_b64: impl Into<String>) -> Self {
        Self {
            account_name: account_name.into(),
            account_key_b64: account_key_b64.into(),
        }
    }

    /// 为当前请求补齐 Azure 必需头并计算 SharedKey `Authorization`。
    fn signed_headers(
        &self,
        task: &TransferTask,
        method: &str,
        base: &mut HeaderMap,
    ) -> Result<(), MeowError> {
        let url = Url::parse(task.url()).map_err(|e| {
            MeowError::from_code(
                InnerErrorCode::ParameterEmpty,
                format!("invalid microsoft blob url: {} ({e})", task.url()),
            )
        })?;
        insert_header(base, HEADER_MS_VERSION, MS_VERSION)?;
        insert_header(base, HEADER_MS_DATE, now_rfc1123_gmt()?.as_str())?;
        let auth = build_authorization(
            method,
            &url,
            base,
            self.account_name.as_str(),
            self.account_key_b64.as_str(),
        )?;
        insert_header(base, HEADER_AUTHORIZATION, auth.as_str())?;
        Ok(())
    }
}

impl BreakpointDownload for AzureDownload {
    fn merge_head_headers(&self, ctx: DownloadHeadCtx<'_>) -> Result<(), MeowError> {
        // HEAD 用于获取远端总大小（Content-Length）。
        self.signed_headers(ctx.task, "HEAD", ctx.base)?;
        Ok(())
    }

    fn merge_range_get_headers(&self, ctx: DownloadRangeGetCtx<'_>) -> Result<(), MeowError> {
        // Range 由执行器传入，格式例如 `bytes=0-5242879`。
        let range = reqwest::header::HeaderValue::from_str(ctx.range_value).map_err(|e| {
            MeowError::from_code(
                InnerErrorCode::ParameterEmpty,
                format!("invalid range header value '{}': {e}", ctx.range_value),
            )
        })?;
        ctx.base.insert(RANGE, range);
        // 未指定 Accept 时使用二进制流，避免服务端内容协商导致数据变形。
        if !ctx.base.contains_key(ACCEPT) {
            ctx.base.insert(
                ACCEPT,
                reqwest::header::HeaderValue::from_static(DEFAULT_RANGE_ACCEPT),
            );
        }
        // 注意：签名必须在 Range/Accept 注入之后执行，确保签名与实际发出的头一致。
        self.signed_headers(ctx.task, "GET", ctx.base)?;
        Ok(())
    }
}

```

## AzureUpload
```rust
//! Azure Block Blob 分片上传（Put Block / Put Block List）。
//!
//! 实现思路：
//! - `prepare`：初始化会话，并在断点场景尝试读取未提交 block；
//! - `upload_chunk`：每个 chunk 对应一个 `Put Block`；
//! - `complete_upload`：`Put Block List` 按顺序提交所有 block；
//! - `abort_upload`：Azure 无显式 abort，清理本地会话状态即可。

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use reqwest::header::HeaderMap;
use reqwest::{Method, Url};
use rusty_cat::http_breakpoint::UploadResumeInfo;
use rusty_cat::upload_trait::{UploadChunkCtx, UploadPrepareCtx};
use rusty_cat::{BreakpointUpload, InnerErrorCode, MeowError, TransferTask};
use std::collections::BTreeSet;
use std::sync::Arc;
use tokio::sync::Mutex;

use super::azure_shared_key::{
    build_authorization, insert_header, now_rfc1123_gmt, HEADER_AUTHORIZATION, HEADER_MS_DATE,
    HEADER_MS_VERSION, MS_VERSION,
};

#[derive(Debug, Default)]
struct PutBlockSession {
    /// 防止一个 uploader 实例在不同 URL 之间串状态。
    target_url: Option<String>,
    /// 已确认上传成功的 block 索引（0-based）。
    uploaded_blocks: BTreeSet<usize>,
}

#[derive(Clone)]
pub struct AzureUpload {
    account_name: String,
    account_key_b64: String,
    session: Arc<Mutex<PutBlockSession>>,
}

impl AzureUpload {
    pub fn new(account_name: impl Into<String>, account_key_b64: impl Into<String>) -> Self {
        Self {
            account_name: account_name.into(),
            account_key_b64: account_key_b64.into(),
            session: Arc::new(Mutex::new(PutBlockSession::default())),
        }
    }

    /// block id 使用固定宽度数字字符串再 Base64：
    /// 例如 index=1 -> "00000001" -> "MDAwMDAwMDE="。
    /// 这样便于调试和排序，且在重试/续传中可稳定复现。
    fn block_id_by_index(idx: usize) -> String {
        BASE64_STANDARD.encode(format!("{idx:08}"))
    }

    /// 基于 `offset/chunk_size` 计算当前分片序号。
    fn part_index(offset: u64, chunk_size: u64) -> Result<usize, MeowError> {
        usize::try_from(offset / chunk_size).map_err(|e| {
            MeowError::from_code(
                InnerErrorCode::InvalidRange,
                format!("part index overflow: {e}"),
            )
        })
    }

    fn build_query_url(
        task: &TransferTask,
        query_pairs: &[(&str, String)],
    ) -> Result<Url, MeowError> {
        let mut url = Url::parse(task.url()).map_err(|e| {
            MeowError::from_code(
                InnerErrorCode::ParameterEmpty,
                format!("invalid microsoft blob url: {} ({e})", task.url()),
            )
        })?;
        {
            let mut pairs = url.query_pairs_mut();
            for (k, v) in query_pairs {
                pairs.append_pair(k, v.as_str());
            }
        }
        Ok(url)
    }

    fn signed_headers(
        &self,
        method: &str,
        url: &Url,
        content_length: Option<usize>,
        content_type: Option<&str>,
        extra_headers: &[(&str, &str)],
    ) -> Result<HeaderMap, MeowError> {
        let mut headers = HeaderMap::new();
        // Azure SharedKey 的最小必需头。
        insert_header(&mut headers, HEADER_MS_VERSION, MS_VERSION)?;
        insert_header(&mut headers, HEADER_MS_DATE, now_rfc1123_gmt()?.as_str())?;
        if let Some(v) = content_type {
            insert_header(&mut headers, "content-type", v)?;
        }
        if let Some(v) = content_length {
            insert_header(&mut headers, "content-length", v.to_string().as_str())?;
        }
        for (k, v) in extra_headers {
            insert_header(&mut headers, k, v)?;
        }
        let authorization = build_authorization(
            method,
            url,
            &headers,
            self.account_name.as_str(),
            self.account_key_b64.as_str(),
        )?;
        insert_header(&mut headers, HEADER_AUTHORIZATION, authorization.as_str())?;
        Ok(headers)
    }

    async fn list_uncommitted_blocks(
        &self,
        client: &reqwest::Client,
        task: &TransferTask,
    ) -> Result<Vec<usize>, MeowError> {
        // 仅列举未提交 block，用于断点恢复（不影响已提交 blob）。
        let url = Self::build_query_url(
            task,
            &[
                ("comp", "blocklist".to_string()),
                ("blocklisttype", "uncommitted".to_string()),
            ],
        )?;
        let headers = self.signed_headers("GET", &url, None, None, &[])?;
        let resp = client
            .request(Method::GET, url)
            .headers(headers)
            .send()
            .await
            .map_err(|e| {
                MeowError::from_source(
                    InnerErrorCode::HttpError,
                    "microsoft list block list failed",
                    e,
                )
            })?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(Vec::new());
        }
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(MeowError::from_code(
                InnerErrorCode::ResponseStatusError,
                format!("microsoft list block list failed: {status}, body: {body}"),
            ));
        }
        Ok(parse_block_indices_from_block_list(body.as_str()))
    }
}

#[async_trait]
impl BreakpointUpload for AzureUpload {
    async fn prepare(&self, ctx: UploadPrepareCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        let client = ctx.client;
        let task = ctx.task;
        let local_offset = ctx.local_offset;
        {
            let mut state = self.session.lock().await;
            if state.target_url.as_deref() != Some(task.url()) {
                *state = PutBlockSession {
                    target_url: Some(task.url().to_string()),
                    uploaded_blocks: BTreeSet::new(),
                };
            }
            // 新任务从 0 开始时清空本地会话缓存。
            if local_offset == 0 {
                state.uploaded_blocks.clear();
                return Ok(UploadResumeInfo {
                    completed_file_id: None,
                    next_byte: Some(0),
                });
            }
            // 同一个任务恢复流程里若已有缓存，直接复用，避免重复调用 ListBlocks。
            if !state.uploaded_blocks.is_empty() {
                return Ok(UploadResumeInfo {
                    completed_file_id: None,
                    next_byte: Some(local_offset),
                });
            }
        }

        let indices = self.list_uncommitted_blocks(client, task).await?;
        if !indices.is_empty() {
            let mut state = self.session.lock().await;
            state.uploaded_blocks.extend(indices.into_iter());
        }
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(local_offset),
        })
    }

    async fn upload_chunk(&self, ctx: UploadChunkCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        let client = ctx.client;
        let task = ctx.task;
        let chunk = ctx.chunk;
        let offset = ctx.offset;
        let idx = Self::part_index(offset, task.chunk_size())?;
        let block_id = Self::block_id_by_index(idx);
        let url = Self::build_query_url(
            task,
            &[("comp", "block".to_string()), ("blockid", block_id)],
        )?;
        let headers = self.signed_headers(
            "PUT",
            &url,
            Some(chunk.len()),
            Some("application/octet-stream"),
            &[],
        )?;
        let resp = client
            .request(Method::PUT, url)
            .headers(headers)
            .body(chunk.to_vec())
            .send()
            .await
            .map_err(|e| {
                MeowError::from_source(InnerErrorCode::HttpError, "microsoft put block failed", e)
            })?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(MeowError::from_code(
                InnerErrorCode::ResponseStatusError,
                format!("microsoft put block failed: {status}, body: {body}"),
            ));
        }
        {
            let mut state = self.session.lock().await;
            state.uploaded_blocks.insert(idx);
        }
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(offset + chunk.len() as u64),
        })
    }

    async fn complete_upload(
        &self,
        client: &reqwest::Client,
        task: &TransferTask,
    ) -> Result<(), MeowError> {
        // 按总大小和 chunk_size 计算应提交的 block 数；
        // block 列表严格按索引递增生成，确保最终 blob 顺序正确。
        let total_chunks =
            ((task.total_size() + task.chunk_size() - 1) / task.chunk_size()) as usize;
        let block_ids: Vec<String> = (0..total_chunks).map(Self::block_id_by_index).collect();
        let mut xml = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList>");
        for id in block_ids {
            xml.push_str("<Latest>");
            xml.push_str(id.as_str());
            xml.push_str("</Latest>");
        }
        xml.push_str("</BlockList>");

        let url = Self::build_query_url(task, &[("comp", "blocklist".to_string())])?;
        let headers =
            self.signed_headers("PUT", &url, Some(xml.len()), Some("application/xml"), &[])?;
        let resp = client
            .request(Method::PUT, url)
            .headers(headers)
            .body(xml)
            .send()
            .await
            .map_err(|e| {
                MeowError::from_source(
                    InnerErrorCode::HttpError,
                    "microsoft put block list failed",
                    e,
                )
            })?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(MeowError::from_code(
                InnerErrorCode::ResponseStatusError,
                format!("microsoft put block list failed: {status}, body: {body}"),
            ));
        }

        let mut state = self.session.lock().await;
        state.uploaded_blocks.clear();
        Ok(())
    }

    async fn abort_upload(
        &self,
        _client: &reqwest::Client,
        _task: &TransferTask,
    ) -> Result<(), MeowError> {
        // Azure Block Blob 没有显式 AbortMultipart 接口；
        // 未提交 block 会在服务端自动过期。这里清理客户端会话，避免后续误续传。
        let mut state = self.session.lock().await;
        state.uploaded_blocks.clear();
        Ok(())
    }
}

fn parse_block_indices_from_block_list(xml: &str) -> Vec<usize> {
    // Azure XML:
    // <BlockList><UncommittedBlocks><Block><Name>base64...</Name>...</Block></...></BlockList>
    // 我们解析 Name -> base64 decode -> "00000001" -> usize(1)。
    let mut out = Vec::new();
    for seg in xml.split("<Name>").skip(1) {
        if let Some((v, _)) = seg.split_once("</Name>") {
            if let Ok(raw) = BASE64_STANDARD.decode(v.trim()) {
                if let Ok(s) = String::from_utf8(raw) {
                    if let Ok(i) = s.parse::<usize>() {
                        out.push(i);
                    }
                }
            }
        }
    }
    out.sort_unstable();
    out
}

```

## Azure Blob SharedKey 鉴权工具
```rust
//! Azure Blob SharedKey 鉴权工具。
//!
//! 目标：
//! - 复用在上传/下载协议实现中；
//! - 严格按 REST 文档构造 `StringToSign` 与 `Authorization`；
//! - 尽量把“规范化”细节收敛在本文件，业务层只关心 method/url/headers。

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::Url;
use rusty_cat::{InnerErrorCode, MeowError};
use sha2::Sha256;
use std::collections::BTreeMap;
use time::{format_description, OffsetDateTime};

type HmacSha256 = Hmac<Sha256>;

pub const MS_VERSION: &str = "2023-11-03";
pub const HEADER_MS_DATE: &str = "x-ms-date";
pub const HEADER_MS_VERSION: &str = "x-ms-version";
pub const HEADER_AUTHORIZATION: &str = "authorization";

/// 安全插入 HTTP 头（带统一错误映射）。
pub fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) -> Result<(), MeowError> {
    let name = HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
        MeowError::from_code(
            InnerErrorCode::ParameterEmpty,
            format!("invalid header name '{name}': {e}"),
        )
    })?;
    let value = HeaderValue::from_str(value).map_err(|e| {
        MeowError::from_code(
            InnerErrorCode::ParameterEmpty,
            format!("invalid header value for '{name}': {e}"),
        )
    })?;
    headers.insert(name, value);
    Ok(())
}

/// 生成 Azure 所需 RFC1123 GMT 时间，例如：
/// `Wed, 15 Apr 2026 03:37:04 GMT`。
pub fn now_rfc1123_gmt() -> Result<String, MeowError> {
    let fmt = format_description::parse(
        "[weekday repr:short], [day padding:zero] [month repr:short] [year] [hour]:[minute]:[second] GMT",
    )
    .map_err(|e| {
        MeowError::from_code(
            InnerErrorCode::ParameterEmpty,
            format!("build RFC1123 format failed: {e}"),
        )
    })?;
    OffsetDateTime::now_utc().format(&fmt).map_err(|e| {
        MeowError::from_code(
            InnerErrorCode::ParameterEmpty,
            format!("format RFC1123 datetime failed: {e}"),
        )
    })
}

/// 构造 Azure Blob `Authorization: SharedKey ...`。
///
/// 调用方只需保证：
/// - `headers` 里包含了本次请求真正会发送到服务端的 `x-ms-*` 和标准头；
/// - `url` 是完整请求 URL（含 path/query）。
pub fn build_authorization(
    method: &str,
    url: &Url,
    headers: &HeaderMap,
    account_name: &str,
    account_key_b64: &str,
) -> Result<String, MeowError> {
    let canonicalized_headers = canonicalized_headers(headers)?;
    let canonicalized_resource = canonicalized_resource(url, account_name);

    // 顺序与 Azure 文档一致：12 个标准行 + CanonicalizedHeaders + CanonicalizedResource。
    let string_to_sign = format!(
        "{method}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{canonicalized_headers}{canonicalized_resource}",
        header_value(headers, "content-encoding"),
        header_value(headers, "content-language"),
        canonicalized_content_length(headers),
        header_value(headers, "content-md5"),
        header_value(headers, "content-type"),
        "", // 使用 x-ms-date 时，Date 行留空
        header_value(headers, "if-modified-since"),
        header_value(headers, "if-match"),
        header_value(headers, "if-none-match"),
        header_value(headers, "if-unmodified-since"),
        header_value(headers, "range"),
    );

    let key = BASE64_STANDARD.decode(account_key_b64).map_err(|e| {
        MeowError::from_code(
            InnerErrorCode::ParameterEmpty,
            format!("decode microsoft account key failed: {e}"),
        )
    })?;

    let mut mac = HmacSha256::new_from_slice(&key).map_err(|e| {
        MeowError::from_code(
            InnerErrorCode::ParameterEmpty,
            format!("build HMAC-SHA256 failed: {e}"),
        )
    })?;
    mac.update(string_to_sign.as_bytes());
    let signature = BASE64_STANDARD.encode(mac.finalize().into_bytes());
    Ok(format!("SharedKey {account_name}:{signature}"))
}

fn canonicalized_headers(headers: &HeaderMap) -> Result<String, MeowError> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    for (k, v) in headers {
        let k = k.as_str().to_ascii_lowercase();
        if !k.starts_with("x-ms-") {
            continue;
        }
        let value = v.to_str().map_err(|e| {
            MeowError::from_code(
                InnerErrorCode::ParameterEmpty,
                format!("x-ms header is not valid ASCII: {e}"),
            )
        })?;
        pairs.push((k, value.trim().to_string()));
    }
    // Azure 要求按 header name 字典序排序。
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = String::new();
    for (k, v) in pairs {
        out.push_str(k.as_str());
        out.push(':');
        out.push_str(v.as_str());
        out.push('\n');
    }
    Ok(out)
}

fn canonicalized_resource(url: &Url, account_name: &str) -> String {
    let mut out = format!("/{account_name}{}", url.path());
    let mut query_map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    // query 参数按 key 小写后聚合，value 再升序后以 ',' 拼接。
    for (k, v) in url.query_pairs() {
        query_map
            .entry(k.to_ascii_lowercase())
            .or_default()
            .push(v.into_owned());
    }
    for (k, values) in query_map {
        let mut values = values;
        values.sort();
        out.push('\n');
        out.push_str(k.as_str());
        out.push(':');
        out.push_str(values.join(",").as_str());
    }
    out
}

fn canonicalized_content_length(headers: &HeaderMap) -> String {
    let raw = header_value(headers, "content-length");
    // 对 Blob 服务（2015-02-21+），GET/HEAD 常见无 Content-Length；"0" 也需要落空行。
    if raw == "0" {
        String::new()
    } else {
        raw
    }
}

fn header_value(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

```

## Aliyun OSS Authorization
```rust
use std::ffi::{c_char, c_void, CStr, CString};

pub(crate) const OSS_UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";

#[repr(C)]
#[derive(Clone, Copy)]
struct OssAuthKv {
    key: *const c_char,
    value: *const c_char,
}

#[repr(C)]
struct OssAuthV4Request {
    method: *const c_char,
    canonical_uri: *const c_char,
    raw_query: *const c_char,
    headers: *const OssAuthKv,
    headers_len: usize,
    payload_sha256_hex: *const c_char,
    additional_headers: *const c_char,
}

#[repr(C)]
struct OssAuthV4Credentials {
    access_key_id: *const c_char,
    access_key_secret: *const c_char,
    region: *const c_char,
    product: *const c_char,
    datetime: *const c_char,
}

// 显式链接 build.rs 生成的静态库。
#[link(name = "oss_auth_standalone", kind = "static")]
extern "C" {
    fn oss_auth_v4_sign_authorization(
        req: *const OssAuthV4Request,
        cred: *const OssAuthV4Credentials,
        out_authorization: *mut *mut c_char,
        out_signature_hex: *mut *mut c_char,
        out_signed_headers: *mut *mut c_char,
    ) -> i32;

    fn oss_auth_v4_free(p: *mut c_void);
}

fn take_c_string(ptr: *mut c_char) -> String {
    debug_assert!(!ptr.is_null());
    let s = unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned();
    unsafe { oss_auth_v4_free(ptr as *mut c_void) };
    s
}

/// Aliyun OSS Authorization V4 输入凭证（对应 CredentialScope 所需字段）。
pub struct OssV4Credentials {
    /// AccessKeyId（AK）
    access_key_id: String,
    /// AccessKeySecret（SK）
    access_key_secret: String,
    /// region，例如 "cn-hangzhou"
    region: String,
    /// product/service，例如 "oss"（可传 None 表示默认 "oss"）
    product: Option<String>,
    /// ISO8601 basic：`YYYYMMDDTHHMMSSZ`（建议与请求头 x-oss-date 一致）
    datetime: String,
}
impl OssV4Credentials {
    pub fn new(ak: &str, sk: &str, region: &str, iso8601_str: &str) -> OssV4Credentials {
        OssV4Credentials {
            access_key_id: ak.to_string(),
            access_key_secret: sk.to_string(),
            region: region.to_string(),
            product: None,
            datetime: iso8601_str.to_string(),
        }
    }
}

/// Aliyun OSS Authorization V4 输出结果。
pub struct OssV4AuthResult {
    /// 完整的 `Authorization` 头内容
    pub authorization: String,
    /// 签名 hex（64 chars，小写）
    pub signature_hex: String,
    /// 本次签名覆盖的 header 名列表（分号分隔，小写）
    pub signed_headers: String,
}

/// 生成阿里云 OSS Authorization V4（OSS4-HMAC-SHA256）。
///
/// - **`method`**: HTTP 方法，例如 "GET"/"PUT"
/// - **`canonical_uri`**: URL 的 path 部分（例如请求 https://bucket.oss-cn-hangzhou.aliyuncs.com/dir/a.txt ⇒ "/dir/a.txt"）。
/// - **`raw_query`**: 不带 '?' 的 query，例如 "uploads&partNumber=1"（None/"" 表示无 query）
///         URL 的 query 部分（不带 ?，例如 uploads&partNumber=1）。
/// - **`headers`**: header 键值对数组（建议至少包含 host、x-oss-date；以及所有需要签名的 x-oss-*）
///     （至少 host、x-oss-date；以及你要签的 x-oss-*、content-type、content-md5 等）。
/// - **`payload_sha256_hex`**: body 的 sha256 hex；None 表示 "UNSIGNED-PAYLOAD"
///     如果要签名 body 的 hash，就用请求 body 的 sha256(hex)；否则用 NULL（走 UNSIGNED-PAYLOAD）。
/// - **`additional_headers`**: OSS V4 文档里的 AdditionalHeaders（分号分隔的小写 header 名）；None 表示无
///     当文档要求把某些非默认 header 纳入签名时，填它们的小写名字列表（; 分隔）；不需要就 NULL。
pub fn generate_authorization_v4(
    method: &str,
    canonical_uri: &str,
    raw_query: Option<&str>,
    headers: &[(&str, &str)],
    payload_sha256_hex: Option<&str>,
    additional_headers: Option<&str>,
    cred: OssV4Credentials,
) -> Result<OssV4AuthResult, i32> {
    // C 侧要求 headers_len > 0
    if headers.is_empty() {
        return Err(1);
    }

    let c_method = CString::new(method).map_err(|_| 1)?;
    let c_uri = CString::new(canonical_uri).map_err(|_| 1)?;
    let c_query = CString::new(raw_query.unwrap_or("")).map_err(|_| 1)?;

    let mut c_header_keys: Vec<CString> = Vec::with_capacity(headers.len());
    let mut c_header_vals: Vec<CString> = Vec::with_capacity(headers.len());
    for (k, v) in headers {
        c_header_keys.push(CString::new(*k).map_err(|_| 1)?);
        c_header_vals.push(CString::new(*v).map_err(|_| 1)?);
    }
    let c_headers: Vec<OssAuthKv> = (0..headers.len())
        .map(|i| OssAuthKv {
            key: c_header_keys[i].as_ptr(),
            value: c_header_vals[i].as_ptr(),
        })
        .collect();

    let c_payload = payload_sha256_hex
        .map(|s| CString::new(s).map_err(|_| 1))
        .transpose()?;
    let c_additional = additional_headers
        .map(|s| CString::new(s).map_err(|_| 1))
        .transpose()?;

    let c_ak = CString::new(cred.access_key_id).map_err(|_| 1)?;
    let c_sk = CString::new(cred.access_key_secret).map_err(|_| 1)?;
    let c_region = CString::new(cred.region).map_err(|_| 1)?;
    let c_product = cred
        .product
        .map(|p| CString::new(p).map_err(|_| 1))
        .transpose()?;
    let c_datetime = CString::new(cred.datetime).map_err(|_| 1)?;

    let req = OssAuthV4Request {
        method: c_method.as_ptr(),
        canonical_uri: c_uri.as_ptr(),
        raw_query: c_query.as_ptr(),
        headers: c_headers.as_ptr(),
        headers_len: c_headers.len(),
        payload_sha256_hex: c_payload
            .as_ref()
            .map(|s| s.as_ptr())
            .unwrap_or(std::ptr::null()),
        additional_headers: c_additional
            .as_ref()
            .map(|s| s.as_ptr())
            .unwrap_or(std::ptr::null()),
    };

    let cred_c = OssAuthV4Credentials {
        access_key_id: c_ak.as_ptr(),
        access_key_secret: c_sk.as_ptr(),
        region: c_region.as_ptr(),
        product: c_product
            .as_ref()
            .map(|s| s.as_ptr())
            .unwrap_or(std::ptr::null()),
        datetime: c_datetime.as_ptr(),
    };

    let mut out_authz: *mut c_char = std::ptr::null_mut();
    let mut out_sig: *mut c_char = std::ptr::null_mut();
    let mut out_signed_headers: *mut c_char = std::ptr::null_mut();

    let rc = unsafe {
        oss_auth_v4_sign_authorization(
            &req,
            &cred_c,
            &mut out_authz,
            &mut out_sig,
            &mut out_signed_headers,
        )
    };
    if rc != 0 {
        return Err(rc);
    }

    Ok(OssV4AuthResult {
        authorization: take_c_string(out_authz),
        signature_hex: take_c_string(out_sig),
        signed_headers: take_c_string(out_signed_headers),
    })
}
use time::{format_description, OffsetDateTime};

/// 获取当前 UTC 时间，返回阿里云 OSS V4 常用的 ISO8601 Basic 格式：
/// `YYYYMMDDTHHMMSSZ`（例如 `20260414T120000Z`）。
pub fn now_iso8601_basic_z() -> String {
    let fmt = format_description::parse("[year][month][day]T[hour][minute][second]Z")
        .expect("valid time format description");
    OffsetDateTime::now_utc()
        .format(&fmt)
        .expect("formatting OffsetDateTime should not fail")
}

/// 从一个 UTC 时间生成 `YYYYMMDDTHHMMSSZ`。
pub fn format_iso8601_basic_z(t: OffsetDateTime) -> String {
    let fmt = format_description::parse("[year][month][day]T[hour][minute][second]Z")
        .expect("valid time format description");
    t.to_offset(time::UtcOffset::UTC)
        .format(&fmt)
        .expect("formatting OffsetDateTime should not fail")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_now_iso8601_basic_z_shape() {
        let s = now_iso8601_basic_z();
        assert_eq!(s.len(), 16, "{s}");
        assert!(s.ends_with('Z'), "{s}");
        assert_eq!(&s[8..9], "T", "{s}");
        assert!(s.chars().take(8).all(|c| c.is_ascii_digit()), "{s}");
        assert!(s.chars().skip(9).take(6).all(|c| c.is_ascii_digit()), "{s}");
    }
}

```

