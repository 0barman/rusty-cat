use async_trait::async_trait;
use reqwest::header::{CONTENT_LENGTH, ETAG};
use reqwest::{Client, Method};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

use crate::chunk_outcome::ChunkOutcome;
use crate::direction::Direction;
use crate::error::{InnerErrorCode, MeowError};
use crate::http_breakpoint::{
    BreakpointDownload, BreakpointUpload, DefaultStyleUpload, DownloadHeadCtx,
    StandardRangeDownload, UploadPrepareCtx,
};
use crate::prepare_outcome::PrepareOutcome;
use crate::transfer_executor_trait::TransferTrait;
use crate::transfer_task::TransferTask;

use super::default_http_transfer_chunks::{download_one_chunk, map_reqwest, upload_one_chunk};

/// Creates default breakpoint protocol instances.
pub(crate) fn default_breakpoint_arcs() -> (
    Arc<dyn BreakpointUpload + Send + Sync>,
    Arc<dyn BreakpointDownload + Send + Sync>,
) {
    (
        Arc::new(DefaultStyleUpload::default()),
        Arc::new(StandardRangeDownload::default()),
    )
}

/// Built-in HTTP transfer backend based on `reqwest` and async file I/O.
pub struct DefaultHttpTransfer {
    /// Default shared HTTP client.
    client: reqwest::Client,
    /// Fallback upload protocol when task does not provide one.
    fallback_upload: Arc<dyn BreakpointUpload + Send + Sync>,
    /// Fallback download protocol when task does not provide one.
    fallback_download: Arc<dyn BreakpointDownload + Send + Sync>,
}

impl DefaultHttpTransfer {
    /// Creates a backend with default HTTP timeout and keepalive values.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::DefaultHttpTransfer;
    ///
    /// let backend = DefaultHttpTransfer::new();
    /// let _ = backend;
    /// ```
    pub fn new() -> Self {
        Self::with_http_timeouts(Duration::from_secs(5), Duration::from_secs(30))
    }

    /// Creates built-in backend with explicit timeout and keepalive values.
    ///
    /// # Range guidance
    ///
    /// - `http_timeout`: recommended `1s..=120s`
    /// - `tcp_keepalive`: recommended `10s..=300s`
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::time::Duration;
    /// use rusty_cat::DefaultHttpTransfer;
    ///
    /// let backend = DefaultHttpTransfer::with_http_timeouts(
    ///     Duration::from_secs(15),
    ///     Duration::from_secs(60),
    /// );
    /// let _ = backend;
    /// ```
    pub fn with_http_timeouts(http_timeout: Duration, tcp_keepalive: Duration) -> Self {
        // Keep non-fallible constructor for compatibility.
        // Prefer `try_with_http_timeouts` in new code for explicit errors.
        let client = match Client::builder()
            .timeout(http_timeout)
            .tcp_keepalive(tcp_keepalive)
            // Avoid reusing idle TCP connections after user cancel/pause: a pooled conn can be
            // half-closed and then fail the next HEAD/GET with reset/incomplete message (flaky
            // under pause/resume and slow range servers).
            .pool_max_idle_per_host(0)
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                crate::meow_flow_log!(
                    "http_client",
                    "with_http_timeouts build failed, fallback to Client::new(): {}",
                    e
                );
                Client::new()
            }
        };
        Self {
            client,
            fallback_upload: Arc::new(DefaultStyleUpload::default()),
            fallback_download: Arc::new(StandardRangeDownload::default()),
        }
    }

    /// Preferred fallible constructor with explicit error propagation.
    ///
    /// # Errors
    ///
    /// Returns `HttpClientBuildFailed` when `reqwest::Client` cannot be
    /// constructed with the provided timeout/keepalive values.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::time::Duration;
    /// use rusty_cat::DefaultHttpTransfer;
    ///
    /// let backend = DefaultHttpTransfer::try_with_http_timeouts(
    ///     Duration::from_secs(10),
    ///     Duration::from_secs(30),
    /// )?;
    /// let _ = backend;
    /// # Ok::<(), rusty_cat::api::MeowError>(())
    /// ```
    pub fn try_with_http_timeouts(
        http_timeout: Duration,
        tcp_keepalive: Duration,
    ) -> Result<Self, MeowError> {
        let client = Client::builder()
            .timeout(http_timeout)
            .tcp_keepalive(tcp_keepalive)
            .pool_max_idle_per_host(0)
            .build()
            .map_err(|e| {
                MeowError::from_source(
                    InnerErrorCode::HttpClientBuildFailed,
                    format!(
                        "build reqwest client failed (timeout={:?}, keepalive={:?})",
                        http_timeout, tcp_keepalive
                    ),
                    e,
                )
            })?;
        Ok(Self {
            client,
            fallback_upload: Arc::new(DefaultStyleUpload::default()),
            fallback_download: Arc::new(StandardRangeDownload::default()),
        })
    }

    /// Creates backend with an externally provided `reqwest::Client`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::DefaultHttpTransfer;
    ///
    /// let reqwest_client = reqwest::Client::new();
    /// let backend = DefaultHttpTransfer::with_client(reqwest_client);
    /// let _ = backend;
    /// ```
    pub fn with_client(client: reqwest::Client) -> Self {
        Self {
            client,
            fallback_upload: Arc::new(DefaultStyleUpload::default()),
            fallback_download: Arc::new(StandardRangeDownload::default()),
        }
    }

    /// Creates backend with explicit fallback upload/download protocol plugins.
    ///
    /// Task-level protocol instances still take precedence when present.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::sync::Arc;
    /// use rusty_cat::{DefaultHttpTransfer, DefaultStyleUpload, StandardRangeDownload};
    ///
    /// let backend = DefaultHttpTransfer::with_fallbacks(
    ///     reqwest::Client::new(),
    ///     Arc::new(DefaultStyleUpload::default()),
    ///     Arc::new(StandardRangeDownload::default()),
    /// );
    /// let _ = backend;
    /// ```
    pub fn with_fallbacks(
        client: reqwest::Client,
        upload: Arc<dyn BreakpointUpload + Send + Sync>,
        download: Arc<dyn BreakpointDownload + Send + Sync>,
    ) -> Self {
        Self {
            client,
            fallback_upload: upload,
            fallback_download: download,
        }
    }

    /// Selects HTTP client for a task.
    fn client_for(&self, task: &TransferTask) -> reqwest::Client {
        task.http_client_ref()
            .cloned()
            .unwrap_or_else(|| self.client.clone())
    }

    /// Selects upload protocol implementation for a task.
    fn upload_arc(&self, task: &TransferTask) -> Arc<dyn BreakpointUpload + Send + Sync> {
        match task.breakpoint_upload() {
            Some(a) => a.clone(),
            None => self.fallback_upload.clone(),
        }
    }

    /// Selects download protocol implementation for a task.
    fn download_arc(&self, task: &TransferTask) -> Arc<dyn BreakpointDownload + Send + Sync> {
        match task.breakpoint_download() {
            Some(a) => a.clone(),
            None => self.fallback_download.clone(),
        }
    }
}

impl Default for DefaultHttpTransfer {
    fn default() -> Self {
        Self::new()
    }
}

async fn upload_prepare(
    client: &reqwest::Client,
    task: &TransferTask,
    upload: Arc<dyn BreakpointUpload + Send + Sync>,
    local_offset: u64,
) -> Result<PrepareOutcome, MeowError> {
    let max_retries = task.max_upload_prepare_retries();
    let mut attempt: u32 = 0;
    loop {
        crate::meow_flow_log!(
            "upload_prepare",
            "start: file={} local_offset={} total={} attempt={} max_retries={}",
            task.file_name(),
            local_offset,
            task.total_size(),
            attempt,
            max_retries
        );
        match upload_prepare_once(client, task, upload.clone(), local_offset).await {
            Ok(outcome) => {
                if attempt > 0 {
                    crate::meow_flow_log!(
                        "upload_prepare",
                        "prepare retry recovered: file={} attempts_used={}",
                        task.file_name(),
                        attempt
                    );
                }
                return Ok(outcome);
            }
            Err(err) => {
                let retryable = crate::inner::exec_impl::retry::is_transport_retryable(&err);
                let reached_limit = attempt >= max_retries;
                if !retryable || reached_limit {
                    crate::meow_flow_log!(
                        "upload_prepare",
                        "prepare give up: file={} attempt={} max_retries={} retryable={} err={}",
                        task.file_name(),
                        attempt,
                        max_retries,
                        retryable,
                        err
                    );
                    return Err(err);
                }
                let delay_ms = crate::inner::exec_impl::retry::calc_backoff_with_jitter_ms(attempt);
                crate::meow_flow_log!(
                    "upload_prepare",
                    "prepare retry scheduled: file={} next_attempt={} delay_ms={} err={}",
                    task.file_name(),
                    attempt + 1,
                    delay_ms,
                    err
                );
                sleep(Duration::from_millis(delay_ms)).await;
                attempt += 1;
            }
        }
    }
}

async fn upload_prepare_once(
    client: &reqwest::Client,
    task: &TransferTask,
    upload: Arc<dyn BreakpointUpload + Send + Sync>,
    local_offset: u64,
) -> Result<PrepareOutcome, MeowError> {
    let info = upload
        .prepare(UploadPrepareCtx {
            client,
            task,
            local_offset,
        })
        .await?;
    if info.completed_file_id.is_some() {
        let total = task.total_size();
        crate::meow_flow_log!(
            "upload_prepare",
            "server indicates upload already complete: file={} total={}",
            task.file_name(),
            total
        );
        return Ok(PrepareOutcome {
            next_offset: total,
            total_size: total,
        });
    }
    let server_off = info.next_byte.unwrap_or(0);
    let next = local_offset.max(server_off).min(task.total_size());
    crate::meow_flow_log!(
        "upload_prepare",
        "prepared: server_next={} local_offset={} final_next={}",
        server_off,
        local_offset,
        next
    );
    Ok(PrepareOutcome {
        next_offset: next,
        total_size: task.total_size(),
    })
}

/// Runs download prepare stage and computes resume offset/total size.
async fn download_prepare(
    client: &reqwest::Client,
    task: &TransferTask,
    download: Arc<dyn BreakpointDownload + Send + Sync>,
    _local_offset: u64,
) -> Result<PrepareOutcome, MeowError> {
    crate::meow_flow_log!(
        "download_prepare",
        "start: file={} path={}",
        task.file_name(),
        task.file_path().display()
    );
    let path = task.file_path();
    let local_len = match tokio::fs::metadata(path).await {
        Ok(meta) => meta.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0u64,
        Err(e) => {
            return Err(MeowError::from_source(
                InnerErrorCode::IoError,
                format!("download_prepare stat failed: {}", path.display()),
                e,
            ));
        }
    };

    // Use local persisted length as resume start to avoid sparse gaps.
    let start = local_len;
    let head_url = download.head_url(task);
    let mut head_headers = task.headers().clone();
    download.merge_head_headers(DownloadHeadCtx {
        task,
        base: &mut head_headers,
    })?;
    let head_resp = client
        .request(Method::HEAD, &head_url)
        .headers(head_headers)
        .send()
        .await
        .map_err(map_reqwest)?;
    if !head_resp.status().is_success() {
        crate::meow_flow_log!(
            "download_prepare",
            "head failed: status={}",
            head_resp.status()
        );
        return Err(MeowError::from_code(
            InnerErrorCode::ResponseStatusError,
            format!("download_prepare HEAD failed: {}", head_resp.status()),
        ));
    }
    let head_content_length = head_resp
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("<missing>");
    let head_etag = head_resp
        .headers()
        .get(ETAG)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("<missing>");
    crate::meow_flow_log!(
        "download_prepare",
        "head metadata: url={} content_length={} etag={}",
        head_url,
        head_content_length,
        head_etag
    );
    let total = download.total_size_from_head(head_resp.headers())?;
    if start > total {
        crate::meow_flow_log!(
            "download_prepare",
            "invalid local length larger than remote: local={} remote={}",
            start,
            total
        );
        return Err(MeowError::from_code_str(
            InnerErrorCode::InvalidRange,
            "local file larger than remote content-length",
        ));
    }
    if start >= total {
        crate::meow_flow_log!(
            "download_prepare",
            "already complete by local length: local={} remote={}",
            start,
            total
        );
        return Ok(PrepareOutcome {
            next_offset: total,
            total_size: total,
        });
    }
    crate::meow_flow_log!(
        "download_prepare",
        "prepared resume offset: start={} remote_total={}",
        start,
        total
    );
    Ok(PrepareOutcome {
        next_offset: start,
        total_size: total,
    })
}

#[async_trait]
impl TransferTrait for DefaultHttpTransfer {
    /// Prepares transfer execution according to task direction.
    async fn prepare(
        &self,
        task: &TransferTask,
        local_offset: u64,
    ) -> Result<PrepareOutcome, MeowError> {
        let client = self.client_for(task);
        match task.direction() {
            Direction::Upload => {
                upload_prepare(&client, task, self.upload_arc(task), local_offset).await
            }
            Direction::Download => {
                download_prepare(&client, task, self.download_arc(task), local_offset).await
            }
        }
    }

    /// Transfers one chunk according to task direction.
    async fn transfer_chunk(
        &self,
        task: &TransferTask,
        offset: u64,
        chunk_size: u64,
        remote_total_size: u64,
    ) -> Result<ChunkOutcome, MeowError> {
        let client = self.client_for(task);
        match task.direction() {
            Direction::Upload => {
                upload_one_chunk(&client, task, self.upload_arc(task), offset, chunk_size).await
            }
            Direction::Download => {
                download_one_chunk(
                    &client,
                    task,
                    self.download_arc(task),
                    offset,
                    chunk_size,
                    remote_total_size,
                )
                .await
            }
        }
    }

    /// Handles task cancel; upload direction may trigger protocol abort.
    async fn cancel(&self, task: &TransferTask) -> Result<(), MeowError> {
        if task.direction() != Direction::Upload {
            return Ok(());
        }
        let client = self.client_for(task);
        self.upload_arc(task).abort_upload(&client, task).await
    }
}
