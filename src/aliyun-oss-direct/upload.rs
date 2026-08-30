use std::sync::Arc;

use async_trait::async_trait;
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE, ETAG};
use reqwest::{Method, Url};
use tokio::sync::Mutex;

use super::constants::{
    DEFAULT_UPLOAD_CONTENT_TYPE, MAX_OSS_PART_NUMBER, OSS_COMPLETE_ADDITIONAL_HEADERS,
    OSS_COMPLETE_ALL_HEADER, OSS_COMPLETE_ALL_VALUE, OSS_LIST_MAX_PARTS, OSS_LIST_MAX_UPLOADS,
};
use std::sync::Mutex as StdMutex;

use super::multipart_session::MultipartSession;
use super::signing::{
    current_date, derive_signing_key, header_value, param_error, signed_headers_with_key,
    SecretKeyBytes,
};
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
    /// Cached OSS v4 signing key for the current UTC day, keyed by its `YYYYMMDD`
    /// date string. The 4-step derivation depends only on (secret, date, region);
    /// caching it skips 4 HMACs per part. Uses a `std` mutex because the critical
    /// section is tiny and synchronous (no `.await`).
    signing_key_cache: Arc<StdMutex<Option<(String, SecretKeyBytes)>>>,
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
            signing_key_cache: Arc::new(StdMutex::new(None)),
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
        let (now, date) = current_date();
        let signing_key = self.signing_key_for(&date)?;
        signed_headers_with_key(
            method,
            canonical_uri,
            raw_query,
            sign_pairs,
            additional_headers,
            self.access_key_id.as_str(),
            self.region.as_str(),
            now,
            &signing_key,
        )
    }

    /// Returns the OSS v4 signing key for `date`, deriving and caching it on a UTC
    /// day change.
    ///
    /// The cache holds a single `(date, key)` entry; a new UTC day — or a backward
    /// clock jump that changes the date string — recomputes it (passive
    /// invalidation, no timer). The cached material is secret-derived, hence the
    /// non-`Debug` [`SecretKeyBytes`] wrapper.
    fn signing_key_for(&self, date: &str) -> Result<SecretKeyBytes, MeowError> {
        let mut guard = self
            .signing_key_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((cached_date, key)) = guard.as_ref() {
            if cached_date == date {
                return Ok(key.clone());
            }
        }
        let key = derive_signing_key(self.access_key_secret.as_str(), date, self.region.as_str())?;
        *guard = Some((date.to_string(), key.clone()));
        Ok(key)
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
                crate::log::emit_lazy(|| {
                    crate::log::Log::error(
                        "initiate",
                        format!(
                            "oss initiate multipart send failed: {}",
                            crate::log::redact_secrets(&e.to_string())
                        ),
                    )
                    .with_task_id(task.file_sign())
                    .with_url(task.url())
                });
                MeowError::from_source(
                    InnerErrorCode::HttpError,
                    "oss initiate multipart failed",
                    e,
                )
            })?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            crate::log::emit_lazy(|| {
                crate::log::Log::error(
                    "initiate",
                    format!(
                        "oss initiate multipart non-2xx: {status}, body: {}",
                        crate::log::redact_secrets(&body)
                    ),
                )
                .with_task_id(task.file_sign())
                .with_http_status(status.as_u16())
                .with_url(task.url())
            });
            return Err(MeowError::from_code(
                InnerErrorCode::ResponseStatusError,
                format!("oss initiate multipart failed: {status}, body: {body}"),
            )
            .with_http_status(status.as_u16()));
        }
        extract_xml_tag(&body, "UploadId").ok_or_else(|| {
            crate::log::emit_lazy(|| {
                crate::log::Log::error(
                    "initiate",
                    format!(
                        "oss initiate multipart 2xx but UploadId unparseable, body: {}",
                        crate::log::redact_secrets(&body)
                    ),
                )
                .with_task_id(task.file_sign())
                .with_url(task.url())
            });
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
                crate::log::emit_lazy(|| {
                    crate::log::Log::warn(
                        "list_uploads",
                        format!(
                            "oss list multipart uploads send failed: {}",
                            crate::log::redact_secrets(&e.to_string())
                        ),
                    )
                    .with_task_id(task.file_sign())
                    .with_key(object_key.as_str())
                    .with_url(task.url())
                });
                MeowError::from_source(
                    InnerErrorCode::HttpError,
                    "oss list multipart uploads failed",
                    e,
                )
            })?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            crate::log::emit_lazy(|| {
                crate::log::Log::warn(
                    "list_uploads",
                    format!(
                        "oss list multipart uploads non-2xx: {status}, body: {}",
                        crate::log::redact_secrets(&body)
                    ),
                )
                .with_task_id(task.file_sign())
                .with_key(object_key.as_str())
                .with_http_status(status.as_u16())
                .with_url(task.url())
            });
            return Err(MeowError::from_code(
                InnerErrorCode::ResponseStatusError,
                format!("oss list multipart uploads failed: {status}, body: {body}"),
            )
            .with_http_status(status.as_u16()));
        }
        let ids = extract_upload_ids_for_key(&body, &object_key);
        if ids.len() > 1 {
            crate::log::emit_lazy(|| {
                crate::log::Log::error(
                    "list_uploads",
                    format!(
                        "found multiple multipart sessions for object '{object_key}'; cannot safely adopt"
                    ),
                )
                .with_task_id(task.file_sign())
                .with_key(object_key.as_str())
                .with_url(task.url())
            });
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
                crate::log::emit_lazy(|| {
                    crate::log::Log::warn(
                        "list_parts",
                        format!(
                            "oss list parts send failed: {}",
                            crate::log::redact_secrets(&e.to_string())
                        ),
                    )
                    .with_task_id(task.file_sign())
                    .with_url(task.url())
                });
                MeowError::from_source(InnerErrorCode::HttpError, "oss list parts failed", e)
            })?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            crate::log::emit_lazy(|| {
                crate::log::Log::warn(
                    "list_parts",
                    format!(
                        "oss list parts non-2xx: {status}, body: {}",
                        crate::log::redact_secrets(&body)
                    ),
                )
                .with_task_id(task.file_sign())
                .with_http_status(status.as_u16())
                .with_url(task.url())
            });
            return Err(MeowError::from_code(
                InnerErrorCode::ResponseStatusError,
                format!("oss list parts failed: {status}, body: {body}"),
            )
            .with_http_status(status.as_u16()));
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

    /// Returns the active multipart `UploadId` for the current session, if any.
    ///
    /// Callers can persist this id (it is not a credential) so that an orphaned
    /// multipart upload — for example one left behind by a process crash — can be
    /// aborted later via [`AliOssDirectUpload::abort_multipart`], stopping the
    /// uncommitted parts from accruing OSS storage cost. Returns `None` before
    /// `prepare` has initiated/adopted a session, or after a successful
    /// complete/abort.
    pub async fn current_upload_id(&self) -> Option<String> {
        self.session.lock().await.upload_id.clone()
    }

    /// Sends `AbortMultipartUpload` (`DELETE ?uploadId=...`) for an explicit
    /// upload id without reading or mutating the in-memory session state.
    ///
    /// Use this to clean up an orphaned multipart session recovered from durable
    /// storage (see [`AliOssDirectUpload::current_upload_id`]) after the original
    /// task or process is gone. A `404 Not Found` is treated as success because
    /// the session may already have expired or been aborted.
    ///
    /// # Errors
    ///
    /// Returns [`MeowError`] when signing, the network request, or the server
    /// response indicates the abort did not succeed.
    pub async fn abort_multipart(
        &self,
        client: &reqwest::Client,
        task: &TransferTask,
        upload_id: &str,
    ) -> Result<(), MeowError> {
        self.send_abort_multipart(client, task, upload_id).await
    }

    /// Shared `AbortMultipartUpload` request used by both the cancel hook and the
    /// out-of-band [`AliOssDirectUpload::abort_multipart`] cleanup path.
    ///
    /// The `upload_id` is an OSS multipart session id, not a credential, so it is
    /// safe to surface in error messages to make abort failures actionable.
    async fn send_abort_multipart(
        &self,
        client: &reqwest::Client,
        task: &TransferTask,
        upload_id: &str,
    ) -> Result<(), MeowError> {
        let canonical_uri = self.object_canonical_uri_from_task_url(task)?;
        let (url, raw_query) = Self::build_query_url(task, &[("uploadId", upload_id.to_string())])?;
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
                crate::log::emit_lazy(|| {
                    crate::log::Log::warn(
                        "abort",
                        format!(
                            "oss abort multipart send failed (uploadId={upload_id}); session id retained: {}",
                            crate::log::redact_secrets(&e.to_string())
                        ),
                    )
                    .with_task_id(task.file_sign())
                    .with_url(task.url())
                });
                MeowError::from_source(
                    InnerErrorCode::HttpError,
                    format!("oss abort multipart upload failed (uploadId={upload_id})"),
                    e,
                )
            })?;
        let status = resp.status();
        if !(status.is_success() || status == reqwest::StatusCode::NOT_FOUND) {
            let body = resp.text().await.unwrap_or_default();
            crate::log::emit_lazy(|| {
                crate::log::Log::warn(
                    "abort",
                    format!(
                        "oss abort multipart non-2xx-non-404: {status}, uploadId={upload_id}, body: {}; session id retained",
                        crate::log::redact_secrets(&body)
                    ),
                )
                .with_task_id(task.file_sign())
                .with_http_status(status.as_u16())
                .with_url(task.url())
            });
            return Err(MeowError::from_code(
                InnerErrorCode::ResponseStatusError,
                format!(
                    "oss abort multipart upload failed: {status}, uploadId={upload_id}, body: {body}"
                ),
            )
            .with_http_status(status.as_u16()));
        }
        Ok(())
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
            if let Some(upload_id) = state.upload_id.clone() {
                validate_resume_offset(ctx.task, ctx.local_offset)?;
                return Ok(UploadResumeInfo {
                    completed_file_id: None,
                    next_byte: Some(ctx.local_offset),
                    provider_upload_id: Some(upload_id),
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
                state.upload_id = Some(upload_id.clone());
                crate::log::emit_lazy(|| {
                    crate::log::Log::key(
                        "initiate",
                        format!("oss multipart adopted existing session (uploadId={upload_id})"),
                    )
                    .with_task_id(ctx.task.file_sign())
                    .with_offset(ctx.local_offset)
                    .with_url(ctx.task.url())
                });
                return Ok(UploadResumeInfo {
                    completed_file_id: None,
                    next_byte: Some(ctx.local_offset),
                    provider_upload_id: Some(upload_id),
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
        state.upload_id = Some(upload_id.clone());
        crate::log::emit_lazy(|| {
            crate::log::Log::key(
                "initiate",
                format!("oss multipart initiated (uploadId={upload_id})"),
            )
            .with_task_id(ctx.task.file_sign())
            .with_url(ctx.task.url())
        });
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(0),
            provider_upload_id: Some(upload_id),
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
            crate::log::emit_lazy(|| {
                crate::log::Log::error(
                    "upload_part",
                    format!(
                        "computed partNumber {part_number} exceeds MAX_OSS_PART_NUMBER {MAX_OSS_PART_NUMBER}"
                    ),
                )
                .with_task_id(ctx.task.file_sign())
                .with_part(part_number - 1)
                .with_offset(ctx.offset)
                .with_byte_len(ctx.chunk.len() as u64)
                .with_url(ctx.task.url())
            });
            return Err(MeowError::from_code(
                InnerErrorCode::InvalidRange,
                format!("partNumber out of range: {part_number}"),
            ));
        }
        let (url, raw_query) = Self::build_query_url(
            ctx.task,
            &[
                ("partNumber", part_number.to_string()),
                ("uploadId", upload_id.clone()),
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
                crate::log::emit_lazy(|| {
                    crate::log::Log::error(
                        "upload_part",
                        format!(
                            "oss upload part send failed (uploadId={upload_id}): {}",
                            crate::log::redact_secrets(&e.to_string())
                        ),
                    )
                    .with_task_id(ctx.task.file_sign())
                    .with_part(part_number - 1)
                    .with_offset(ctx.offset)
                    .with_byte_len(ctx.chunk.len() as u64)
                    .with_url(ctx.task.url())
                });
                MeowError::from_source(InnerErrorCode::HttpError, "oss upload part failed", e)
            })?;
        let status = resp.status();
        let etag_present = resp.headers().get(ETAG).is_some();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            crate::log::emit_lazy(|| {
                crate::log::Log::error(
                    "upload_part",
                    format!(
                        "oss upload part non-2xx: {status}, uploadId={upload_id}, body: {}",
                        crate::log::redact_secrets(&body)
                    ),
                )
                .with_task_id(ctx.task.file_sign())
                .with_part(part_number - 1)
                .with_offset(ctx.offset)
                .with_byte_len(ctx.chunk.len() as u64)
                .with_http_status(status.as_u16())
                .with_url(ctx.task.url())
            });
            return Err(MeowError::from_code(
                InnerErrorCode::ResponseStatusError,
                format!("oss upload part failed: {status}, body: {body}"),
            )
            .with_http_status(status.as_u16()));
        }
        if !etag_present {
            crate::log::emit_lazy(|| {
                crate::log::Log::error(
                    "upload_part",
                    format!(
                        "oss upload part 2xx but missing ETag header; part not durably acked (uploadId={upload_id})"
                    ),
                )
                .with_task_id(ctx.task.file_sign())
                .with_part(part_number - 1)
                .with_offset(ctx.offset)
                .with_byte_len(ctx.chunk.len() as u64)
                .with_http_status(status.as_u16())
                .with_url(ctx.task.url())
            });
            return Err(MeowError::from_code_str(
                InnerErrorCode::ResponseParseError,
                "oss upload part success but missing ETag header",
            ));
        }
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(ctx.offset + ctx.chunk.len() as u64),
            provider_upload_id: Some(upload_id),
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
        // Cloned before `upload_id` is moved into the query builder so the
        // (non-credential) session id can be attached to the complete logs below.
        let upload_id_for_log = upload_id.clone();
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
                crate::log::emit_lazy(|| {
                    crate::log::Log::error(
                        "complete",
                        format!(
                            "oss complete multipart send failed (uploadId={upload_id_for_log}): {}",
                            crate::log::redact_secrets(&e.to_string())
                        ),
                    )
                    .with_task_id(task.file_sign())
                    .with_url(task.url())
                });
                MeowError::from_source(
                    InnerErrorCode::HttpError,
                    "oss complete multipart upload failed",
                    e,
                )
            })?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            crate::log::emit_lazy(|| {
                crate::log::Log::error(
                    "complete",
                    format!(
                        "oss complete multipart non-2xx: {status}, uploadId={upload_id_for_log}, body: {}",
                        crate::log::redact_secrets(&body)
                    ),
                )
                .with_task_id(task.file_sign())
                .with_http_status(status.as_u16())
                .with_url(task.url())
            });
            return Err(MeowError::from_code(
                InnerErrorCode::ResponseStatusError,
                format!("oss complete multipart upload failed: {status}, body: {body}"),
            )
            .with_http_status(status.as_u16()));
        }
        self.session.lock().await.upload_id = None;
        crate::log::emit_lazy(|| {
            crate::log::Log::key(
                "complete",
                format!("oss multipart completed (uploadId={upload_id_for_log})"),
            )
            .with_task_id(task.file_sign())
            .with_url(task.url())
        });
        Ok(None)
    }

    async fn abort_upload(
        &self,
        client: &reqwest::Client,
        task: &TransferTask,
    ) -> Result<(), MeowError> {
        let upload_id = self.session.lock().await.upload_id.clone();
        let Some(upload_id) = upload_id else {
            return Ok(());
        };
        // On failure the session id is intentionally kept so the upper layer can
        // log it (see executor cancel path) and the caller can retry cleanup.
        self.send_abort_multipart(client, task, &upload_id).await?;
        self.session.lock().await.upload_id = None;
        Ok(())
    }

    /// OSS multipart is out-of-order safe: a part's number is a pure function of
    /// its chunk index (`part_number = offset / chunk_size + 1`), the in-flight
    /// `UploadId` plus part set live behind a `Mutex`, and the committed order is
    /// fixed at completion by OSS (`x-oss-complete-all`, i.e. all uploaded parts
    /// merged by ascending part number) — never by Upload Part arrival order.
    /// Re-uploading the same part number overwrites idempotently. So parts of one
    /// file may be uploaded concurrently and out of order.
    fn supports_parallel_parts(&self) -> bool {
        true
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
        if uploaded_parts.binary_search(&part_number).is_err() {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn upload() -> AliOssDirectUpload {
        AliOssDirectUpload::new("bucket", "ak", "sk", "cn-hangzhou")
    }

    #[tokio::test]
    async fn current_upload_id_is_none_before_prepare() {
        // A freshly constructed protocol has no active multipart session yet.
        assert_eq!(upload().current_upload_id().await, None);
    }

    #[tokio::test]
    async fn current_upload_id_reflects_active_session() {
        // The accessor exposes the in-flight UploadId so callers can persist it
        // for later out-of-band abort.
        let u = upload();
        {
            let mut state = u.session.lock().await;
            state.upload_id = Some("test-upload-id".to_string());
        }
        assert_eq!(
            u.current_upload_id().await.as_deref(),
            Some("test-upload-id")
        );
    }

    #[test]
    fn oss_direct_advertises_parallel_parts() {
        use crate::upload_trait::BreakpointUpload;
        // OSS multipart part identity is offset-derived (`part_number =
        // offset/chunk + 1`, see `upload_chunk`), completion merges all uploaded
        // parts by ascending part number (`x-oss-complete-all`), and re-upload of
        // the same part number is idempotent — so it is out-of-order safe and must
        // advertise intra-file parallel parts (mirrors the Azure-direct / presigned
        // `supports_parallel_parts` guards).
        assert!(upload().supports_parallel_parts());
    }

    #[test]
    fn provider_upload_id_round_trips_in_resume_info() {
        // The new field carries the provider session id end to end.
        let info = UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(0),
            provider_upload_id: Some("uid-42".to_string()),
        };
        assert_eq!(info.provider_upload_id.as_deref(), Some("uid-42"));
        // Default keeps it absent so providers without a session id opt out.
        assert!(UploadResumeInfo::default().provider_upload_id.is_none());
    }
}
