use std::sync::Arc;

use async_trait::async_trait;
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE, ETAG};
use reqwest::{Method, Url};
use tokio::sync::Mutex;

use super::constants::{
    DEFAULT_UPLOAD_CONTENT_TYPE, MAX_OSS_PART_NUMBER, OSS_COMPLETE_ADDITIONAL_HEADERS,
    OSS_COMPLETE_ALL_HEADER, OSS_COMPLETE_ALL_VALUE, OSS_LIST_MAX_PARTS, OSS_LIST_MAX_UPLOADS,
};
use super::multipart_session::MultipartSession;
use super::signing::{header_value, param_error, signed_headers};
use super::xml::{extract_part_numbers, extract_upload_ids_for_key, extract_xml_tag};
use crate::http_breakpoint::UploadResumeInfo;
use crate::upload_trait::{UploadChunkCtx, UploadPrepareCtx};
use crate::{BreakpointUpload, InnerErrorCode, MeowError, TransferTask};

/// Aliyun OSS direct multipart upload protocol using AccessKey signing.
#[derive(Clone)]
pub struct AliOssDirectUpload {
    bucket: String,
    access_key_id: String,
    access_key_secret: String,
    region: String,
    session: Arc<Mutex<MultipartSession>>,
}

impl AliOssDirectUpload {
    pub fn new(
        bucket: impl Into<String>,
        access_key_id: impl Into<String>,
        access_key_secret: impl Into<String>,
        region: impl Into<String>,
    ) -> Self {
        Self {
            bucket: bucket.into(),
            access_key_id: access_key_id.into(),
            access_key_secret: access_key_secret.into(),
            region: region.into(),
            session: Arc::new(Mutex::new(MultipartSession::default())),
        }
    }

    fn bucket_canonical_uri(&self) -> String {
        format!("/{}/", self.bucket)
    }

    fn object_key_from_task_url(&self, task: &TransferTask) -> Result<String, MeowError> {
        let url = Url::parse(task.url()).map_err(param_error)?;
        Ok(url.path().trim_start_matches('/').to_string())
    }

    fn object_canonical_uri_from_task_url(&self, task: &TransferTask) -> Result<String, MeowError> {
        let url = Url::parse(task.url()).map_err(param_error)?;
        Ok(format!("/{}{}", self.bucket, url.path()))
    }

    fn build_signed_headers(
        &self,
        method: &str,
        canonical_uri: &str,
        raw_query: Option<&str>,
        sign_pairs: &[(&str, &str)],
        additional_headers: Option<&str>,
    ) -> Result<reqwest::header::HeaderMap, MeowError> {
        signed_headers(
            method,
            canonical_uri,
            raw_query,
            sign_pairs,
            additional_headers,
            self.access_key_id.as_str(),
            self.access_key_secret.as_str(),
            self.region.as_str(),
        )
    }

    async fn initiate_multipart_upload(
        &self,
        client: &reqwest::Client,
        task: &TransferTask,
    ) -> Result<String, MeowError> {
        let canonical_uri = self.object_canonical_uri_from_task_url(task)?;
        let mut url = Url::parse(task.url()).map_err(param_error)?;
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
        extract_xml_tag(&body, "UploadId").ok_or_else(|| {
            MeowError::from_code(
                InnerErrorCode::ResponseParseError,
                format!("oss initiate multipart missing UploadId: {body}"),
            )
        })
    }

    async fn try_adopt_upload_id_from_list(
        &self,
        client: &reqwest::Client,
        task: &TransferTask,
    ) -> Result<Option<String>, MeowError> {
        let object_key = self.object_key_from_task_url(task)?;
        let mut url = Url::parse(task.url()).map_err(param_error)?;
        url.set_path("/");
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("uploads", "");
            pairs.append_pair("prefix", &object_key);
            pairs.append_pair("max-uploads", OSS_LIST_MAX_UPLOADS);
        }
        let raw_query = url.query().ok_or_else(|| {
            MeowError::from_code_str(
                InnerErrorCode::ParameterEmpty,
                "build list multipart query failed",
            )
        })?;
        let headers = self.build_signed_headers(
            "GET",
            self.bucket_canonical_uri().as_str(),
            Some(raw_query),
            &[],
            None,
        )?;
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
        let ids = extract_upload_ids_for_key(&body, &object_key);
        if ids.len() > 1 {
            return Err(MeowError::from_code(
                InnerErrorCode::InvalidTaskState,
                format!("found multiple multipart sessions for object '{object_key}'"),
            ));
        }
        Ok(ids.into_iter().next())
    }

    async fn list_uploaded_part_numbers(
        &self,
        client: &reqwest::Client,
        task: &TransferTask,
        upload_id: &str,
    ) -> Result<Vec<u64>, MeowError> {
        let canonical_uri = self.object_canonical_uri_from_task_url(task)?;
        let (url, raw_query) = Self::build_query_url(
            task,
            &[
                ("uploadId", upload_id.to_string()),
                ("max-parts", OSS_LIST_MAX_PARTS.to_string()),
            ],
        )?;
        let headers =
            self.build_signed_headers("GET", &canonical_uri, Some(raw_query.as_str()), &[], None)?;
        let resp = client
            .request(Method::GET, url)
            .headers(headers)
            .send()
            .await
            .map_err(|e| {
                MeowError::from_source(InnerErrorCode::HttpError, "oss list parts failed", e)
            })?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(MeowError::from_code(
                InnerErrorCode::ResponseStatusError,
                format!("oss list parts failed: {status}, body: {body}"),
            ));
        }
        Ok(extract_part_numbers(&body))
    }

    fn build_query_url(
        task: &TransferTask,
        query_pairs: &[(&str, String)],
    ) -> Result<(Url, String), MeowError> {
        let mut url = Url::parse(task.url()).map_err(param_error)?;
        {
            let mut pairs = url.query_pairs_mut();
            for (k, v) in query_pairs {
                pairs.append_pair(k, v.as_str());
            }
        }
        let query = url.query().map(|q| q.to_string()).ok_or_else(|| {
            MeowError::from_code_str(InnerErrorCode::ParameterEmpty, "build query url failed")
        })?;
        Ok((url, query))
    }
}

#[async_trait]
impl BreakpointUpload for AliOssDirectUpload {
    async fn prepare(&self, ctx: UploadPrepareCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        validate_oss_task(ctx.task)?;
        {
            let mut state = self.session.lock().await;
            if state.target_url.as_deref() != Some(ctx.task.url()) {
                *state = MultipartSession {
                    target_url: Some(ctx.task.url().to_string()),
                    upload_id: None,
                };
            }
            if state.upload_id.is_some() {
                validate_resume_offset(ctx.task, ctx.local_offset)?;
                return Ok(UploadResumeInfo {
                    completed_file_id: None,
                    next_byte: Some(ctx.local_offset),
                });
            }
        }
        if ctx.local_offset > 0 {
            validate_resume_offset(ctx.task, ctx.local_offset)?;
            if let Some(upload_id) = self
                .try_adopt_upload_id_from_list(ctx.client, ctx.task)
                .await?
            {
                let uploaded_parts = self
                    .list_uploaded_part_numbers(ctx.client, ctx.task, &upload_id)
                    .await?;
                validate_remote_parts_for_resume(ctx.task, ctx.local_offset, &uploaded_parts)?;
                let mut state = self.session.lock().await;
                state.target_url = Some(ctx.task.url().to_string());
                state.upload_id = Some(upload_id);
                return Ok(UploadResumeInfo {
                    completed_file_id: None,
                    next_byte: Some(ctx.local_offset),
                });
            }
            return Err(MeowError::from_code_str(
                InnerErrorCode::InvalidTaskState,
                "local offset > 0 but no OSS multipart session found; cannot safely resume",
            ));
        }
        let upload_id = self.initiate_multipart_upload(ctx.client, ctx.task).await?;
        let mut state = self.session.lock().await;
        state.target_url = Some(ctx.task.url().to_string());
        state.upload_id = Some(upload_id);
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(0),
        })
    }

    async fn upload_chunk(&self, ctx: UploadChunkCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        validate_oss_task(ctx.task)?;
        let canonical_uri = self.object_canonical_uri_from_task_url(ctx.task)?;
        let upload_id = self.session.lock().await.upload_id.clone().ok_or_else(|| {
            MeowError::from_code_str(
                InnerErrorCode::InvalidTaskState,
                "multipart upload_id missing; call prepare first",
            )
        })?;
        let part_number = (ctx.offset / ctx.task.chunk_size()) + 1;
        if part_number > MAX_OSS_PART_NUMBER {
            return Err(MeowError::from_code(
                InnerErrorCode::InvalidRange,
                format!("partNumber out of range: {part_number}"),
            ));
        }
        let (url, raw_query) = Self::build_query_url(
            ctx.task,
            &[
                ("partNumber", part_number.to_string()),
                ("uploadId", upload_id),
            ],
        )?;
        let headers =
            self.build_signed_headers("PUT", &canonical_uri, Some(raw_query.as_str()), &[], None)?;
        let resp = ctx
            .client
            .request(Method::PUT, url)
            .headers(headers)
            .body(reqwest::Body::from(ctx.chunk.clone()))
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
            return Err(MeowError::from_code_str(
                InnerErrorCode::ResponseParseError,
                "oss upload part success but missing ETag header",
            ));
        }
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(ctx.offset + ctx.chunk.len() as u64),
        })
    }

    async fn complete_upload(
        &self,
        client: &reqwest::Client,
        task: &TransferTask,
    ) -> Result<Option<String>, MeowError> {
        validate_oss_task(task)?;
        let canonical_uri = self.object_canonical_uri_from_task_url(task)?;
        let upload_id = self.session.lock().await.upload_id.clone();
        let Some(upload_id) = upload_id else {
            return Ok(None);
        };
        let (url, raw_query) = Self::build_query_url(task, &[("uploadId", upload_id)])?;
        let mut headers = self.build_signed_headers(
            "POST",
            &canonical_uri,
            Some(raw_query.as_str()),
            &[
                ("content-type", DEFAULT_UPLOAD_CONTENT_TYPE),
                (OSS_COMPLETE_ALL_HEADER, OSS_COMPLETE_ALL_VALUE),
            ],
            Some(OSS_COMPLETE_ADDITIONAL_HEADERS),
        )?;
        headers.insert(CONTENT_LENGTH, header_value("0")?);
        headers.insert(CONTENT_TYPE, header_value(DEFAULT_UPLOAD_CONTENT_TYPE)?);
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
        self.session.lock().await.upload_id = None;
        Ok(None)
    }

    async fn abort_upload(
        &self,
        client: &reqwest::Client,
        task: &TransferTask,
    ) -> Result<(), MeowError> {
        let canonical_uri = self.object_canonical_uri_from_task_url(task)?;
        let upload_id = self.session.lock().await.upload_id.clone();
        let Some(upload_id) = upload_id else {
            return Ok(());
        };
        let (url, raw_query) = Self::build_query_url(task, &[("uploadId", upload_id)])?;
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
        let status = resp.status();
        if !(status.is_success() || status == reqwest::StatusCode::NOT_FOUND) {
            let body = resp.text().await.unwrap_or_default();
            return Err(MeowError::from_code(
                InnerErrorCode::ResponseStatusError,
                format!("oss abort multipart upload failed: {status}, body: {body}"),
            ));
        }
        self.session.lock().await.upload_id = None;
        Ok(())
    }
}

fn validate_oss_task(task: &TransferTask) -> Result<(), MeowError> {
    let total_chunks = task.total_size().div_ceil(task.chunk_size());
    if total_chunks > MAX_OSS_PART_NUMBER {
        return Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!(
                "OSS multipart upload supports at most {MAX_OSS_PART_NUMBER} parts; task requires {total_chunks}"
            ),
        ));
    }
    Ok(())
}

fn validate_resume_offset(task: &TransferTask, local_offset: u64) -> Result<(), MeowError> {
    if local_offset > task.total_size() {
        return Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!(
                "local offset {local_offset} exceeds total size {}",
                task.total_size()
            ),
        ));
    }
    if local_offset != task.total_size() && local_offset % task.chunk_size() != 0 {
        return Err(MeowError::from_code(
            InnerErrorCode::InvalidTaskState,
            format!(
                "local offset {local_offset} is not aligned to chunk size {}; cannot safely resume OSS multipart upload",
                task.chunk_size()
            ),
        ));
    }
    Ok(())
}

fn validate_remote_parts_for_resume(
    task: &TransferTask,
    local_offset: u64,
    uploaded_parts: &[u64],
) -> Result<(), MeowError> {
    let expected_parts = if local_offset == task.total_size() {
        task.total_size().div_ceil(task.chunk_size())
    } else {
        local_offset / task.chunk_size()
    };
    for part_number in 1..=expected_parts {
        if !uploaded_parts.binary_search(&part_number).is_ok() {
            return Err(MeowError::from_code(
                InnerErrorCode::InvalidTaskState,
                format!(
                    "local offset {local_offset} requires OSS part {part_number}, but remote multipart session is missing it"
                ),
            ));
        }
    }
    Ok(())
}
