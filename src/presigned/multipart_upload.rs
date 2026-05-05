use std::sync::Arc;

use async_trait::async_trait;
use reqwest::header::ETAG;
use tokio::sync::Mutex;

use crate::error::{InnerErrorCode, MeowError};
use crate::http_breakpoint::UploadResumeInfo;
use crate::upload_trait::{BreakpointUpload, UploadChunkCtx, UploadPrepareCtx};
use crate::TransferTask;

use super::time::now_unix_secs;
use super::{
    CompletionRequest, PresignedMultipartUploadPlan, PresignedUploadPart,
    PresignedUploadUrlRefresher, PresignedUploadedPart,
};

/// Provider-neutral presigned multipart upload implementation.
#[derive(Clone)]
pub struct PresignedMultipartUpload {
    plan: Arc<PresignedMultipartUploadPlan>,
    uploaded_parts: Arc<Mutex<Vec<PresignedUploadedPart>>>,
    url_refresher: Option<Arc<dyn PresignedUploadUrlRefresher>>,
}

impl PresignedMultipartUpload {
    /// Creates a new protocol instance from an upload plan.
    pub fn new(plan: PresignedMultipartUploadPlan) -> Self {
        Self {
            plan: Arc::new(plan),
            uploaded_parts: Arc::new(Mutex::new(Vec::new())),
            url_refresher: None,
        }
    }

    /// Adds a URL refresher used when part URLs are expired or close to expiry.
    pub fn with_url_refresher(mut self, refresher: Arc<dyn PresignedUploadUrlRefresher>) -> Self {
        self.url_refresher = Some(refresher);
        self
    }

    /// Returns the immutable plan.
    pub fn plan(&self) -> &PresignedMultipartUploadPlan {
        &self.plan
    }

    /// Returns successfully uploaded parts captured in this process.
    pub async fn uploaded_parts(&self) -> Vec<PresignedUploadedPart> {
        self.uploaded_parts.lock().await.clone()
    }

    pub(crate) fn now_unix_secs() -> Result<u64, MeowError> {
        now_unix_secs()
    }

    fn is_expired(part: &PresignedUploadPart) -> Result<bool, MeowError> {
        let Some(expires_at) = part.expires_at_unix_secs else {
            return Ok(false);
        };
        Ok(Self::now_unix_secs()? >= expires_at)
    }

    fn should_refresh_part(&self, part: &PresignedUploadPart) -> Result<bool, MeowError> {
        let Some(expires_at) = part.expires_at_unix_secs else {
            return Ok(false);
        };
        Ok(Self::now_unix_secs()?.saturating_add(self.plan.refresh_before_secs) >= expires_at)
    }

    pub(crate) async fn part_for_upload(
        &self,
        offset: u64,
    ) -> Result<PresignedUploadPart, MeowError> {
        let part = self.plan.part_for_offset(offset).cloned().ok_or_else(|| {
            MeowError::from_code(
                InnerErrorCode::InvalidRange,
                format!("missing presigned upload part for offset {offset}"),
            )
        })?;
        if !self.should_refresh_part(&part)? {
            return Ok(part);
        }

        let Some(refresher) = &self.url_refresher else {
            if Self::is_expired(&part)? {
                return Err(MeowError::from_code(
                    InnerErrorCode::InvalidTaskState,
                    format!(
                        "presigned upload part {} URL expired and no refresher is configured",
                        part.part_number
                    ),
                ));
            }
            return Ok(part);
        };

        let refreshed = refresher.refresh_upload_part(&part).await?;
        if refreshed.part_number != part.part_number
            || refreshed.offset != part.offset
            || refreshed.size != part.size
        {
            return Err(MeowError::from_code(
                InnerErrorCode::InvalidTaskState,
                format!(
                    "refreshed presigned part mismatch: old=({}, {}, {}) new=({}, {}, {})",
                    part.part_number,
                    part.offset,
                    part.size,
                    refreshed.part_number,
                    refreshed.offset,
                    refreshed.size
                ),
            ));
        }
        Ok(refreshed)
    }

    async fn send_callback(
        client: &reqwest::Client,
        req: &CompletionRequest,
        body: Option<Vec<u8>>,
        label: &str,
    ) -> Result<Option<String>, MeowError> {
        let mut builder = client.request(req.method.clone(), req.url.as_str());
        builder = builder.headers(req.headers.clone());
        if let Some(body) = body {
            builder = builder.body(body);
        }
        let resp = builder.send().await.map_err(|e| {
            MeowError::from_source(InnerErrorCode::HttpError, format!("{label} failed"), e)
        })?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(MeowError::from_code(
                InnerErrorCode::ResponseStatusError,
                format!("{label} failed: {status}, body: {body}"),
            ));
        }
        Ok(if body.is_empty() { None } else { Some(body) })
    }

    pub(crate) fn completion_json_body(
        &self,
        uploaded_parts: &[PresignedUploadedPart],
    ) -> Result<Vec<u8>, MeowError> {
        #[derive(serde::Serialize)]
        struct CompletionBody<'a> {
            upload_id: &'a Option<String>,
            total_size: u64,
            chunk_size: u64,
            parts: &'a [PresignedUploadedPart],
        }

        serde_json::to_vec(&CompletionBody {
            upload_id: &self.plan.upload_id,
            total_size: self.plan.total_size,
            chunk_size: self.plan.chunk_size,
            parts: uploaded_parts,
        })
        .map_err(|e| {
            MeowError::from_code(
                InnerErrorCode::ResponseParseError,
                format!("serialize presigned completion body failed: {e}"),
            )
        })
    }
}

#[async_trait]
impl BreakpointUpload for PresignedMultipartUpload {
    async fn prepare(&self, ctx: UploadPrepareCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        self.plan.validate()?;
        if self.plan.total_size != ctx.task.total_size() {
            return Err(MeowError::from_code(
                InnerErrorCode::InvalidTaskState,
                format!(
                    "presigned plan total_size mismatch: plan={} task={}",
                    self.plan.total_size,
                    ctx.task.total_size()
                ),
            ));
        }
        if self.plan.chunk_size != ctx.task.chunk_size() {
            return Err(MeowError::from_code(
                InnerErrorCode::InvalidTaskState,
                format!(
                    "presigned plan chunk_size mismatch: plan={} task={}",
                    self.plan.chunk_size,
                    ctx.task.chunk_size()
                ),
            ));
        }
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(ctx.local_offset),
        })
    }

    async fn upload_chunk(&self, ctx: UploadChunkCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        let part = self.part_for_upload(ctx.offset).await?;
        if part.size != ctx.chunk.len() as u64 {
            return Err(MeowError::from_code(
                InnerErrorCode::InvalidRange,
                format!(
                    "presigned part size mismatch: part={} chunk={}",
                    part.size,
                    ctx.chunk.len()
                ),
            ));
        }

        let resp = ctx
            .client
            .request(part.method.clone(), part.url.as_str())
            .headers(part.headers.clone())
            .body(reqwest::Body::from(ctx.chunk.clone()))
            .send()
            .await
            .map_err(|e| {
                MeowError::from_source(InnerErrorCode::HttpError, "presigned upload part failed", e)
            })?;
        let status = resp.status();
        let etag = resp
            .headers()
            .get(ETAG)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(MeowError::from_code(
                InnerErrorCode::ResponseStatusError,
                format!("presigned upload part failed: {status}, body: {body}"),
            ));
        }

        let mut uploaded = self.uploaded_parts.lock().await;
        if let Some(existing) = uploaded.iter_mut().find(|p| p.offset == part.offset) {
            existing.etag = etag;
            existing.size = part.size;
            existing.part_number = part.part_number;
            existing.provider_part_id = part.provider_part_id.clone();
        } else {
            uploaded.push(PresignedUploadedPart {
                part_number: part.part_number,
                provider_part_id: part.provider_part_id.clone(),
                offset: part.offset,
                size: part.size,
                etag,
            });
        }

        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(ctx.offset + ctx.chunk.len() as u64),
        })
    }

    async fn complete_upload(
        &self,
        client: &reqwest::Client,
        _task: &TransferTask,
    ) -> Result<Option<String>, MeowError> {
        let Some(req) = &self.plan.complete_request else {
            return Ok(None);
        };
        let uploaded_parts = self.uploaded_parts.lock().await.clone();
        let body = if let Some(body) = &req.body {
            Some(body.clone())
        } else if req.uploaded_parts_json_body {
            Some(self.completion_json_body(&uploaded_parts)?)
        } else {
            None
        };
        Self::send_callback(client, req, body, "presigned complete callback").await
    }

    async fn abort_upload(
        &self,
        client: &reqwest::Client,
        _task: &TransferTask,
    ) -> Result<(), MeowError> {
        let Some(req) = &self.plan.abort_request else {
            return Ok(());
        };
        Self::send_callback(client, req, req.body.clone(), "presigned abort callback").await?;
        Ok(())
    }
}
