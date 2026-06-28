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

use super::default_http_transfer_chunks::{
    download_one_chunk, map_reqwest, upload_one_chunk, upload_one_chunk_part,
};

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

/// Maximum idle connections kept alive per host in the internal pool.
///
/// Connection reuse removes a TCP+TLS handshake from every chunk that follows
/// the first one on the same host, which dominates per-chunk overhead on
/// high-latency links. The cap stays bounded so long-lived SDK hosts do not
/// accumulate idle sockets; callers that need a different policy can inject
/// their own `reqwest::Client` via `MeowConfig::http_client`.
const DEFAULT_POOL_MAX_IDLE_PER_HOST: usize = 16;

/// How long an idle pooled connection is retained before eviction.
///
/// Sequential chunks on one task are issued back-to-back (the inter-chunk gap
/// is a local file read, i.e. milliseconds), so this comfortably keeps the
/// connection warm within a transfer while trimming sockets left idle across a
/// pause. A connection the server silently closed and we still reuse surfaces
/// as `HttpError`, which every transfer path already retries, so reuse never
/// turns a recoverable stale socket into a terminal failure.
const DEFAULT_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Upper bound applied to the connect phase when building internal clients.
///
/// The total request timeout (`http_timeout`) must stay large enough for a slow
/// chunk body to finish, which would otherwise let a dead TCP/TLS handshake
/// hang for that whole budget. Capping only the connect phase fails an
/// unreachable peer fast without shortening a slow-but-alive transfer. The
/// effective value is `min(http_timeout, cap)`, so small total timeouts are
/// never exceeded and behavior is unchanged when `http_timeout <= cap`.
const DEFAULT_CONNECT_TIMEOUT_CAP: Duration = Duration::from_secs(10);

/// Builds an internal `reqwest::Client` with the library's shared transport
/// policy: a total request timeout, a bounded connect timeout for fast failure
/// on unreachable peers, TCP keepalive, and a bounded idle connection pool for
/// handshake reuse across chunks.
///
/// Centralizing this keeps every internally created client (the transfer
/// backend and [`crate::MeowClient::http_client`]) on the exact same policy, so
/// they can never drift apart.
pub(crate) fn build_internal_client(
    http_timeout: Duration,
    tcp_keepalive: Duration,
) -> Result<reqwest::Client, reqwest::Error> {
    Client::builder()
        .timeout(http_timeout)
        // Fail fast on an unreachable peer while leaving the total budget for
        // slow chunk bodies; see `DEFAULT_CONNECT_TIMEOUT_CAP`.
        .connect_timeout(http_timeout.min(DEFAULT_CONNECT_TIMEOUT_CAP))
        .tcp_keepalive(tcp_keepalive)
        // Reuse idle connections to drop a handshake from every subsequent
        // chunk. A stale socket reused after a pause comes back as `HttpError`,
        // which all transfer paths retry, so reuse is safe; the bounded cap and
        // idle timeout only keep idle sockets in check.
        .pool_max_idle_per_host(DEFAULT_POOL_MAX_IDLE_PER_HOST)
        .pool_idle_timeout(Some(DEFAULT_POOL_IDLE_TIMEOUT))
        .build()
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
        let client = match build_internal_client(http_timeout, tcp_keepalive) {
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
        let client = build_internal_client(http_timeout, tcp_keepalive)
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
        // A missing file here simply means "no local progress yet"; treat it as
        // a fresh download rather than a mid-transfer removal.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0u64,
        Err(e) => {
            return Err(MeowError::from_io(
                format!("download_prepare stat failed: {}", path.display()),
                e,
            ));
        }
    };

    // Use local persisted length as resume start to avoid sparse gaps.
    let start = local_len;
    if let Some(total) = download.total_size_hint(task) {
        if start > total {
            crate::meow_flow_log!(
                "download_prepare",
                "invalid local length larger than hinted remote: local={} remote={}",
                start,
                total
            );
            return Err(MeowError::from_code_str(
                InnerErrorCode::InvalidRange,
                "local file larger than hinted remote total size",
            ));
        }
        crate::meow_flow_log!(
            "download_prepare",
            "prepared from total_size_hint: start={} remote_total={}",
            start,
            total
        );
        return Ok(PrepareOutcome {
            next_offset: start.min(total),
            total_size: total,
        });
    }

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
        )
        .with_http_status(head_resp.status().as_u16()));
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

    /// Parallel parts are only offered for uploads whose resolved protocol
    /// proves out-of-order safety; downloads always stay serial.
    fn supports_parallel_parts(&self, task: &TransferTask) -> bool {
        task.direction() == Direction::Upload && self.upload_arc(task).supports_parallel_parts()
    }

    /// Uploads one chunk without finalizing (parallel path). Completion is run
    /// exactly once by the scheduler via [`Self::complete`].
    async fn transfer_chunk_part(
        &self,
        task: &TransferTask,
        offset: u64,
        chunk_size: u64,
        remote_total_size: u64,
    ) -> Result<ChunkOutcome, MeowError> {
        let client = self.client_for(task);
        match task.direction() {
            Direction::Upload => {
                upload_one_chunk_part(&client, task, self.upload_arc(task), offset, chunk_size).await
            }
            // Download has no finalize step; behaves exactly like transfer_chunk.
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

    /// Finalizes an upload after all parts have been uploaded; delegates to the
    /// protocol's `complete_upload`.
    async fn complete(&self, task: &TransferTask) -> Result<Option<String>, MeowError> {
        if task.direction() != Direction::Upload {
            return Ok(None);
        }
        let client = self.client_for(task);
        self.upload_arc(task).complete_upload(&client, task).await
    }
}
