use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use reqwest::{Method, Url};
use tokio::sync::Mutex;

use super::constants::{
    BLOCK_LIST_CONTENT_TYPE, DEFAULT_BLOCK_CONTENT_TYPE, MAX_AZURE_BLOCKS, MAX_AZURE_BLOCK_BYTES,
};
use super::put_block_session::PutBlockSession;
use super::signing::{decode_account_key, signed_headers_with_key};
use super::xml::{block_list_xml, parse_block_indices_from_block_list};
use crate::http_breakpoint::UploadResumeInfo;
use crate::upload_trait::{UploadChunkCtx, UploadPrepareCtx};
use crate::{BreakpointUpload, InnerErrorCode, MeowError, TransferTask};

/// Azure Blob direct multipart upload protocol using SharedKey authentication.
#[derive(Clone)]
pub struct AzureBlobDirectUpload {
    account_name: String,
    account_key_b64: String,
    /// Base64-decoded account key, decoded lazily on first sign and reused for
    /// every block (skips per-block base64 decode). Lazy rather than in `new` so
    /// the public constructor stays infallible. Uses a `std` mutex because the
    /// critical section is tiny and synchronous (no `.await`).
    decoded_key: Arc<StdMutex<Option<Vec<u8>>>>,
    session: Arc<Mutex<PutBlockSession>>,
}

impl AzureBlobDirectUpload {
    pub fn new(account_name: impl Into<String>, account_key_b64: impl Into<String>) -> Self {
        Self {
            account_name: account_name.into(),
            account_key_b64: account_key_b64.into(),
            decoded_key: Arc::new(StdMutex::new(None)),
            session: Arc::new(Mutex::new(PutBlockSession::default())),
        }
    }

    pub fn block_id_by_index(idx: usize) -> String {
        BASE64_STANDARD.encode(format!("{idx:08}"))
    }

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
                format!("invalid azure blob url: {} ({e})", task.url()),
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
    ) -> Result<reqwest::header::HeaderMap, MeowError> {
        let key = self.account_key()?;
        signed_headers_with_key(
            method,
            url,
            content_length,
            content_type,
            extra_headers,
            self.account_name.as_str(),
            &key,
        )
    }

    /// Returns the base64-decoded account key, decoding once and caching it for
    /// reuse across all blocks.
    fn account_key(&self) -> Result<Vec<u8>, MeowError> {
        let mut guard = self
            .decoded_key
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(key) = guard.as_ref() {
            return Ok(key.clone());
        }
        let key = decode_account_key(self.account_key_b64.as_str())?;
        *guard = Some(key.clone());
        Ok(key)
    }

    async fn list_uncommitted_blocks(
        &self,
        client: &reqwest::Client,
        task: &TransferTask,
    ) -> Result<Vec<usize>, MeowError> {
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
                MeowError::from_source(InnerErrorCode::HttpError, "azure list block list failed", e)
            })?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(Vec::new());
        }
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(MeowError::from_code(
                InnerErrorCode::ResponseStatusError,
                format!("azure list block list failed: {status}, body: {body}"),
            )
            .with_http_status(status.as_u16()));
        }
        Ok(parse_block_indices_from_block_list(body.as_str()))
    }
}

#[async_trait]
impl BreakpointUpload for AzureBlobDirectUpload {
    async fn prepare(&self, ctx: UploadPrepareCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        validate_azure_task(ctx.task)?;
        {
            let mut state = self.session.lock().await;
            if state.target_url.as_deref() != Some(ctx.task.url()) {
                *state = PutBlockSession {
                    target_url: Some(ctx.task.url().to_string()),
                    uploaded_blocks: Default::default(),
                };
            }
            if ctx.local_offset == 0 {
                state.uploaded_blocks.clear();
                return Ok(UploadResumeInfo {
                    completed_file_id: None,
                    next_byte: Some(0),
                    // Azure Block Blob has no separate session id; resume state is
                    // the uncommitted block list keyed by the blob URL.
                    provider_upload_id: None,
                });
            }
            validate_resume_offset(ctx.task, ctx.local_offset)?;
            if !state.uploaded_blocks.is_empty() {
                validate_remote_blocks_for_resume(
                    ctx.task,
                    ctx.local_offset,
                    &state.uploaded_blocks,
                )?;
                return Ok(UploadResumeInfo {
                    completed_file_id: None,
                    next_byte: Some(ctx.local_offset),
                    provider_upload_id: None,
                });
            }
        }
        let indices = self.list_uncommitted_blocks(ctx.client, ctx.task).await?;
        let mut state = self.session.lock().await;
        if !indices.is_empty() {
            state.uploaded_blocks.extend(indices);
        }
        validate_remote_blocks_for_resume(ctx.task, ctx.local_offset, &state.uploaded_blocks)?;
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(ctx.local_offset),
            provider_upload_id: None,
        })
    }

    async fn upload_chunk(&self, ctx: UploadChunkCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        validate_azure_task(ctx.task)?;
        if ctx.chunk.len() as u64 > MAX_AZURE_BLOCK_BYTES {
            return Err(MeowError::from_code(
                InnerErrorCode::InvalidRange,
                format!(
                    "Azure block size {} exceeds max {MAX_AZURE_BLOCK_BYTES}",
                    ctx.chunk.len()
                ),
            ));
        }
        let idx = Self::part_index(ctx.offset, ctx.task.chunk_size())?;
        if idx >= MAX_AZURE_BLOCKS {
            return Err(MeowError::from_code(
                InnerErrorCode::InvalidRange,
                format!(
                    "Azure block index {idx} exceeds max index {}",
                    MAX_AZURE_BLOCKS - 1
                ),
            ));
        }
        let block_id = Self::block_id_by_index(idx);
        let url = Self::build_query_url(
            ctx.task,
            &[("comp", "block".to_string()), ("blockid", block_id)],
        )?;
        let headers = self.signed_headers(
            "PUT",
            &url,
            Some(ctx.chunk.len()),
            Some(DEFAULT_BLOCK_CONTENT_TYPE),
            &[],
        )?;
        let resp = ctx
            .client
            .request(Method::PUT, url)
            .headers(headers)
            .body(reqwest::Body::from(ctx.chunk.clone()))
            .send()
            .await
            .map_err(|e| {
                MeowError::from_source(InnerErrorCode::HttpError, "azure put block failed", e)
            })?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(MeowError::from_code(
                InnerErrorCode::ResponseStatusError,
                format!("azure put block failed: {status}, body: {body}"),
            )
            .with_http_status(status.as_u16()));
        }
        self.session.lock().await.uploaded_blocks.insert(idx);
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(ctx.offset + ctx.chunk.len() as u64),
            provider_upload_id: None,
        })
    }

    async fn complete_upload(
        &self,
        client: &reqwest::Client,
        task: &TransferTask,
    ) -> Result<Option<String>, MeowError> {
        validate_azure_task(task)?;
        let total_chunks = total_chunks(task)?;
        {
            let state = self.session.lock().await;
            validate_all_blocks_present(total_chunks, &state.uploaded_blocks)?;
        }
        let block_ids = (0..total_chunks)
            .map(Self::block_id_by_index)
            .collect::<Vec<_>>();
        let xml = block_list_xml(block_ids.iter().map(String::as_str));
        let url = Self::build_query_url(task, &[("comp", "blocklist".to_string())])?;
        let headers = self.signed_headers(
            "PUT",
            &url,
            Some(xml.len()),
            Some(BLOCK_LIST_CONTENT_TYPE),
            &[],
        )?;
        let resp = client
            .request(Method::PUT, url)
            .headers(headers)
            .body(xml)
            .send()
            .await
            .map_err(|e| {
                MeowError::from_source(InnerErrorCode::HttpError, "azure put block list failed", e)
            })?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(MeowError::from_code(
                InnerErrorCode::ResponseStatusError,
                format!("azure put block list failed: {status}, body: {body}"),
            )
            .with_http_status(status.as_u16()));
        }
        self.session.lock().await.uploaded_blocks.clear();
        Ok(None)
    }

    async fn abort_upload(
        &self,
        client: &reqwest::Client,
        task: &TransferTask,
    ) -> Result<(), MeowError> {
        let url = Url::parse(task.url()).map_err(|e| {
            MeowError::from_code(
                InnerErrorCode::ParameterEmpty,
                format!("invalid azure blob url: {} ({e})", task.url()),
            )
        })?;
        let headers = self.signed_headers("DELETE", &url, None, None, &[])?;
        let resp = client
            .request(Method::DELETE, url)
            .headers(headers)
            .send()
            .await
            .map_err(|e| {
                MeowError::from_source(
                    InnerErrorCode::HttpError,
                    "azure delete blob on cancel failed",
                    e,
                )
            })?;
        let status = resp.status();
        if !(status.is_success() || status == reqwest::StatusCode::NOT_FOUND) {
            let body = resp.text().await.unwrap_or_default();
            return Err(MeowError::from_code(
                InnerErrorCode::ResponseStatusError,
                format!("azure delete blob on cancel failed: {status}, body: {body}"),
            )
            .with_http_status(status.as_u16()));
        }
        self.session.lock().await.uploaded_blocks.clear();
        Ok(())
    }

    /// Azure block blobs are out-of-order safe: a block's id is a pure function
    /// of its chunk index (`block_id_by_index`), the uploaded-block set is a
    /// `BTreeSet` behind a `Mutex`, and the committed order is fixed by the Block
    /// List (built in index order) submitted at completion — never by Put Block
    /// arrival order. Re-uploading a block id overwrites idempotently.
    fn supports_parallel_parts(&self) -> bool {
        true
    }
}

fn total_chunks(task: &TransferTask) -> Result<usize, MeowError> {
    usize::try_from(task.total_size().div_ceil(task.chunk_size())).map_err(|e| {
        MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!("total chunk count overflow: {e}"),
        )
    })
}

fn validate_azure_task(task: &TransferTask) -> Result<(), MeowError> {
    if task.chunk_size() > MAX_AZURE_BLOCK_BYTES {
        return Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!(
                "Azure block size {} exceeds max {MAX_AZURE_BLOCK_BYTES}",
                task.chunk_size()
            ),
        ));
    }
    let chunks = total_chunks(task)?;
    if chunks > MAX_AZURE_BLOCKS {
        return Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!(
                "Azure block blob supports at most {MAX_AZURE_BLOCKS} blocks; task requires {chunks}"
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
                "local offset {local_offset} is not aligned to chunk size {}; cannot safely resume Azure block upload",
                task.chunk_size()
            ),
        ));
    }
    Ok(())
}

fn validate_remote_blocks_for_resume(
    task: &TransferTask,
    local_offset: u64,
    uploaded_blocks: &std::collections::BTreeSet<usize>,
) -> Result<(), MeowError> {
    let expected_blocks = if local_offset == task.total_size() {
        total_chunks(task)?
    } else {
        usize::try_from(local_offset / task.chunk_size()).map_err(|e| {
            MeowError::from_code(
                InnerErrorCode::InvalidRange,
                format!("completed block count overflow: {e}"),
            )
        })?
    };
    validate_all_blocks_present(expected_blocks, uploaded_blocks)
}

fn validate_all_blocks_present(
    expected_blocks: usize,
    uploaded_blocks: &std::collections::BTreeSet<usize>,
) -> Result<(), MeowError> {
    for idx in 0..expected_blocks {
        if !uploaded_blocks.contains(&idx) {
            return Err(MeowError::from_code(
                InnerErrorCode::InvalidTaskState,
                format!("remote Azure block list is missing block index {idx}"),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::AzureBlobDirectUpload;
    use crate::upload_trait::BreakpointUpload;

    #[test]
    fn test_block_id_by_index_stable_encoding() {
        assert_eq!(AzureBlobDirectUpload::block_id_by_index(1), "MDAwMDAwMDE=");
    }

    #[test]
    fn azure_direct_advertises_parallel_parts() {
        let upload = AzureBlobDirectUpload::new("acct", "a2V5");
        assert!(upload.supports_parallel_parts());
    }
}
