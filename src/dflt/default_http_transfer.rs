use async_trait::async_trait;
use reqwest::header::{CONTENT_ENCODING, CONTENT_LENGTH, ETAG};
use reqwest::{Client, Method};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs::OpenOptions;
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
    download_one_chunk, download_one_chunk_part_positioned, map_reqwest, upload_one_chunk,
    upload_one_chunk_part,
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
                crate::meow_warn_log!(
                    "http_client",
                    "with_http_timeouts build failed, fallback to Client::new(): {}",
                    crate::log::redact_secrets(&e.to_string())
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
        let client = build_internal_client(http_timeout, tcp_keepalive).map_err(|e| {
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
                    crate::meow_key_log!(
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
                    crate::log::emit_lazy(|| {
                        let mut log = crate::log::Log::error(
                            "upload_prepare",
                            format!(
                                "prepare give up: file={} attempt={} max_retries={} retryable={} err={}",
                                task.file_name(),
                                attempt,
                                max_retries,
                                retryable,
                                crate::log::redact_secrets(&err.to_string())
                            ),
                        )
                        .with_key(task.file_name())
                        .with_offset(local_offset)
                        .with_attempt(attempt)
                        .with_max_retries(max_retries);
                        if let Some(s) = err.http_status() {
                            log = log.with_http_status(s);
                        }
                        log
                    });
                    return Err(err);
                }
                let delay_ms = crate::inner::exec_impl::retry::calc_backoff_with_jitter_ms(attempt);
                crate::meow_warn_log!(
                    "upload_prepare",
                    "prepare retry scheduled: file={} next_attempt={} delay_ms={} err={}",
                    task.file_name(),
                    attempt + 1,
                    delay_ms,
                    crate::log::redact_secrets(&err.to_string())
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
    if let Some(snapshot) = task.upload_file_snapshot() {
        snapshot.validate_total_size(task.total_size())?;
        snapshot.validate_generation(false).await?;
    }
    let info = upload
        .prepare(UploadPrepareCtx {
            client,
            task,
            local_offset,
        })
        .await?;
    crate::meow_key_log!(
        "upload_prepare",
        "prepare protocol completed: file={} local_offset={}",
        task.file_name(),
        local_offset
    );
    if info.completed_file_id.is_some() {
        let total = task.total_size();
        crate::meow_key_log!(
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

/// Whether this download task should take the concurrent path (same condition
/// as the executor gate, evaluable from a task snapshot).
fn download_is_parallel(
    task: &TransferTask,
    download: &Arc<dyn BreakpointDownload + Send + Sync>,
) -> bool {
    task.direction() == Direction::Download
        && task.max_parts_in_flight() > 1
        && download.supports_parallel_parts()
}

struct VerifiedDownloadGeneration {
    identity: String,
    validator: String,
}

fn strong_etag(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let etag = headers.get(ETAG)?.to_str().ok()?.trim();
    if etag.len() < 2
        || etag
            .get(..2)
            .map(|prefix| prefix.eq_ignore_ascii_case("W/"))
            .unwrap_or(false)
        || !etag.starts_with('"')
        || !etag.ends_with('"')
    {
        return None;
    }
    Some(etag.to_string())
}

fn is_legacy_presigned_auth_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    matches!(
        key.as_str(),
        "signature" | "awsaccesskeyid" | "ossaccesskeyid" | "googleaccessid" | "expires"
    )
}

fn is_azure_sas_auth_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        // Representation selectors such as `snapshot`, `versionid`, `comp`
        // and response-content overrides are deliberately not included.
        "sig"
            | "sv"
            | "ss"
            | "srt"
            | "sp"
            | "se"
            | "st"
            | "spr"
            | "sip"
            | "si"
            | "skoid"
            | "sktid"
            | "skt"
            | "ske"
            | "sks"
            | "skv"
            | "sr"
    )
}

/// Binds a validator to the canonical resource URL. Only known presigned-auth
/// credentials are discarded; semantic query parameters (for example S3
/// `versionId` or Azure `snapshot`) remain part of the identity so two objects
/// with the same length/ETag cannot accidentally share completed parts.
fn download_identity(url: &str, validator: &str) -> String {
    let resource = reqwest::Url::parse(url)
        .map(|mut parsed| {
            let pairs: Vec<(String, String)> = parsed
                .query_pairs()
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect();
            let has_key = |wanted: &str| {
                pairs
                    .iter()
                    .any(|(key, _)| key.eq_ignore_ascii_case(wanted))
            };
            let aws_v4 = has_key("X-Amz-Signature");
            let google_v4 = has_key("X-Goog-Signature");
            let oss_v4 = has_key("x-oss-signature");
            let legacy = has_key("Signature")
                && (has_key("AWSAccessKeyId")
                    || has_key("OSSAccessKeyId")
                    || has_key("GoogleAccessId"));
            let azure_sas = has_key("sig")
                && (has_key("sv") || has_key("se") || has_key("sp") || has_key("sr"));
            let semantic_pairs: Vec<(String, String)> = pairs
                .into_iter()
                .filter(|(key, _)| {
                    let lower = key.to_ascii_lowercase();
                    !((aws_v4 && lower.starts_with("x-amz-"))
                        || (google_v4 && lower.starts_with("x-goog-"))
                        || (oss_v4 && lower.starts_with("x-oss-"))
                        || (legacy && is_legacy_presigned_auth_key(key))
                        || (azure_sas && is_azure_sas_auth_key(key)))
                })
                .collect();
            parsed.set_query(None);
            if !semantic_pairs.is_empty() {
                parsed.query_pairs_mut().extend_pairs(semantic_pairs);
            }
            parsed.set_fragment(None);
            parsed.to_string()
        })
        .unwrap_or_else(|_| url.split('#').next().unwrap_or(url).to_string());
    format!("resource={resource}\nstrong-etag={validator}")
}

/// Shared tail of [`download_prepare`]: given a resolved remote `total`
/// (`0` == unknown) and the local resume `start`, either drives the concurrent
/// pre-size + `.rcdl` sidecar path, or the serial length-based path. Splitting
/// this out lets every size source (hint / `with_total_size` / HEAD) run the
/// exact same branch without duplicating it or re-indenting the HEAD block.
async fn download_prepare_finish(
    task: &TransferTask,
    download: &Arc<dyn BreakpointDownload + Send + Sync>,
    start: u64,
    total: u64,
    generation: Option<VerifiedDownloadGeneration>,
) -> Result<PrepareOutcome, MeowError> {
    let path = task.file_path();
    if download_is_parallel(task, download) {
        if total == 0 {
            // Unknown size cannot be windowed; let the caller fall back to serial.
            return Ok(PrepareOutcome {
                next_offset: 0,
                total_size: 0,
            });
        }
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    MeowError::from_io(
                        format!("create download dir failed: {}", parent.display()),
                        e,
                    )
                })?;
            }
        }
        // Load (or create) the sidecar BEFORE presizing the target file. The
        // sidecar's own safety guard only invalidates a stale `.rcdl` when the
        // target's on-disk length differs from `total`; if we presized first,
        // the length would already equal `total` and the guard could never
        // fire, letting a stale bitmap survive a deleted/truncated target.
        let progress = match generation {
            Some(generation) => {
                let mut progress =
                    crate::dflt::download_progress::DownloadProgress::load_or_create(
                        path,
                        total,
                        task.chunk_size(),
                        task.max_parts_in_flight(),
                        &generation.identity,
                    )
                    .map_err(|e| MeowError::from_io("load .rcdl sidecar failed".to_string(), e))?;
                progress.set_expected_validator(generation.validator);
                progress
            }
            None => crate::dflt::download_progress::DownloadProgress::create_unverified(
                path,
                total,
                task.chunk_size(),
                task.max_parts_in_flight(),
            )
            .map_err(|e| MeowError::from_io("create .rcdl sidecar failed".to_string(), e))?,
        };
        let watermark = progress.contiguous_watermark();

        // Pre-size once so every part can positioned-write into its slot. Never
        // truncate: on resume the file already holds partial bytes that the
        // sidecar bitmap accounts for; `set_len(total)` only fixes the length.
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .await
            .map_err(|e| {
                MeowError::from_io(format!("open for presize failed: {}", path.display()), e)
            })?;
        file.set_len(total)
            .await
            .map_err(|e| MeowError::from_io("presize set_len failed".to_string(), e))?;
        file.sync_all()
            .await
            .map_err(|e| MeowError::from_io("presize sync failed".to_string(), e))?;
        drop(file);

        if let Ok(mut slot) = task.download_progress().try_lock() {
            *slot = Some(progress);
        } else {
            // The slot is task-owned and only touched here before dispatch; a
            // contended lock is an internal invariant violation.
            return Err(MeowError::from_code_str(
                InnerErrorCode::InvalidTaskState,
                "download progress slot unexpectedly locked during prepare",
            ));
        }
        crate::meow_key_log!(
            "download_prepare",
            "prepared concurrent download: resume_watermark={} remote_total={}",
            watermark,
            total
        );
        return Ok(PrepareOutcome {
            next_offset: watermark,
            total_size: total,
        });
    }

    // Serial path: guard against silently "completing" a pre-sized file left by
    // a prior parallel run (its length == total would otherwise look finished).
    if crate::dflt::download_progress::DownloadProgress::sidecar_exists(path) {
        return Err(MeowError::from_code_str(
            InnerErrorCode::InvalidTaskState,
            "found an in-progress parallel download sidecar (.rcdl); resume with \
             max_parts_in_flight > 1, or delete the sidecar and the partial file",
        ));
    }

    // Existing serial length-based outcome (resume from the local length).
    if start > total {
        crate::log::emit_lazy(|| {
            crate::log::Log::error(
                "download_prepare",
                format!(
                    "invalid local length larger than remote: local={} remote={}",
                    start, total
                ),
            )
            .with_key(task.file_name())
            .with_offset(start)
        });
        return Err(MeowError::from_code_str(
            InnerErrorCode::InvalidRange,
            "local file larger than remote total size",
        ));
    }
    if start >= total {
        crate::meow_key_log!(
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
    crate::meow_key_log!(
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
    task.ensure_download_target_lease().await?;
    let path = task.file_path();
    let local_len = match tokio::fs::metadata(path).await {
        Ok(meta) => meta.len(),
        // A missing file here simply means "no local progress yet"; treat it as
        // a fresh download rather than a mid-transfer removal.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0u64,
        Err(e) => {
            crate::log::emit_lazy(|| {
                crate::log::Log::error(
                    "download_prepare",
                    format!("stat failed: path={} err={}", path.display(), e),
                )
                .with_key(task.file_name())
            });
            return Err(MeowError::from_io(
                format!("download_prepare stat failed: {}", path.display()),
                e,
            ));
        }
    };

    // Use local persisted length as resume start to avoid sparse gaps.
    let start = local_len;

    // Resolve the remote total size. Order (first non-zero source wins):
    //   1) protocol `total_size_hint` (e.g. presigned downloads),
    //   2) builder-supplied `task.total_size()` (`with_total_size`),
    //   3) a HEAD request (only when both hints are absent/zero).
    // Whichever source resolves `total`, the same parallel/serial branch runs.
    // Match builder/HEAD semantics: zero means "unknown", never "already
    // complete". Treating `Some(0)` as a real total would let an invalid custom
    // protocol complete without issuing HEAD or GET.
    if let Some(hinted) = download.total_size_hint(task).filter(|hinted| *hinted > 0) {
        crate::meow_key_log!(
            "download_prepare",
            "resolved total from total_size_hint: start={} remote_total={}",
            start,
            hinted
        );
        return download_prepare_finish(task, &download, start, hinted, None).await;
    }
    if task.total_size() > 0 {
        // Builder supplied a known size via with_total_size(): skip HEAD.
        let hinted = task.total_size();
        crate::meow_key_log!(
            "download_prepare",
            "resolved total from with_total_size: start={} remote_total={}",
            start,
            hinted
        );
        return download_prepare_finish(task, &download, start, hinted, None).await;
    }

    let head_url = download.head_url(task);
    let mut head_headers = task.headers().clone();
    download
        .merge_head_headers(DownloadHeadCtx {
            task,
            base: &mut head_headers,
        })
        .map_err(|e| {
            crate::log::emit_lazy(|| {
                crate::log::Log::warn(
                    "head",
                    format!(
                        "merge_head_headers failed: err={}",
                        crate::log::redact_secrets(&e.to_string())
                    ),
                )
                .with_key(task.file_name())
                .with_url(head_url.as_str())
            });
            e
        })?;
    let head_resp = client
        .request(Method::HEAD, &head_url)
        .headers(head_headers)
        .send()
        .await
        .map_err(|e| {
            crate::log::emit_lazy(|| {
                crate::log::Log::error(
                    "head",
                    format!(
                        "HEAD send failed: err={}",
                        crate::log::redact_secrets(&e.to_string())
                    ),
                )
                .with_key(task.file_name())
                .with_url(head_url.as_str())
            });
            map_reqwest(e)
        })?;
    if !head_resp.status().is_success() {
        let head_status = head_resp.status();
        crate::log::emit_lazy(|| {
            crate::log::Log::error("head", format!("head failed: status={}", head_status))
                .with_key(task.file_name())
                .with_http_status(head_status.as_u16())
                .with_url(head_url.as_str())
        });
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
        crate::log::sanitize_url(&head_url),
        head_content_length,
        head_etag
    );
    let total = download
        .total_size_from_head(head_resp.headers())
        .map_err(|e| {
            crate::log::emit_lazy(|| {
                crate::log::Log::error(
                    "head",
                    format!(
                        "total_size_from_head parse failed: err={}",
                        crate::log::redact_secrets(&e.to_string())
                    ),
                )
                .with_key(task.file_name())
                .with_url(head_url.as_str())
            });
            e
        })?;
    if download_is_parallel(task, &download) {
        if let Some(encoding) = head_resp.headers().get(CONTENT_ENCODING) {
            let encoding = encoding.to_str().unwrap_or("<invalid>");
            if !encoding.eq_ignore_ascii_case("identity") {
                return Err(MeowError::from_code(
                    InnerErrorCode::InvalidRange,
                    format!("parallel download requires identity content-encoding, got {encoding}"),
                ));
            }
        }
    }
    let generation = strong_etag(head_resp.headers()).map(|validator| VerifiedDownloadGeneration {
        identity: download_identity(&download.range_url(task), &validator),
        validator,
    });
    // HEAD resolved the size; run the shared parallel/serial branch.
    download_prepare_finish(task, &download, start, total, generation).await
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
        if !task.begin_upload_abort() {
            return Ok(());
        }
        let client = self.client_for(task);
        self.upload_arc(task).abort_upload(&client, task).await
    }

    /// Parallel parts are offered for uploads whose resolved protocol proves
    /// out-of-order safety, and for downloads whose range protocol declares it.
    fn supports_parallel_parts(&self, task: &TransferTask) -> bool {
        match task.direction() {
            Direction::Upload => self.upload_arc(task).supports_parallel_parts(),
            Direction::Download => self.download_arc(task).supports_parallel_parts(),
        }
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
                upload_one_chunk_part(&client, task, self.upload_arc(task), offset, chunk_size)
                    .await
            }
            Direction::Download => {
                // Resume short-circuit: a part already recorded done in the
                // sidecar needs no network I/O. Keep the lock scope short and
                // never hold it across the network call below.
                {
                    let guard = task.download_progress().lock().map_err(|_| {
                        MeowError::from_code_str(
                            InnerErrorCode::LockPoisoned,
                            "download checkpoint lock poisoned",
                        )
                    })?;
                    if let Some(p) = guard.as_ref() {
                        if p.is_done(offset) {
                            let part_end = offset.saturating_add(chunk_size).min(remote_total_size);
                            return Ok(ChunkOutcome {
                                next_offset: part_end,
                                total_size: remote_total_size,
                                done: part_end >= remote_total_size,
                                completion_payload: None,
                            });
                        }
                    }
                }
                let (outcome, digest) = download_one_chunk_part_positioned(
                    &client,
                    task,
                    self.download_arc(task),
                    offset,
                    chunk_size,
                    remote_total_size,
                )
                .await?;
                // Stage this completed write in the open checkpoint epoch. The
                // blocking durability work runs off Tokio workers and happens
                // once per batch; a bit is never published before data sync.
                task.stage_download_part(offset, digest).await?;
                Ok(outcome)
            }
        }
    }

    /// Finalizes a transfer after all parts have been transferred.
    ///
    /// Upload delegates to the protocol's `complete_upload`. Download validates
    /// the concurrent path's result (pre-sized length matches `total` and every
    /// part is recorded done) and then drops the `.rcdl` sidecar; serial
    /// downloads finalize inline and never set up progress, so they no-op here.
    async fn complete(&self, task: &TransferTask) -> Result<Option<String>, MeowError> {
        match task.direction() {
            Direction::Upload => {
                let client = self.client_for(task);
                let upload = self.upload_arc(task);
                if let Some(snapshot) = task.upload_file_snapshot() {
                    if let Err(error) = snapshot.validate_generation(true).await {
                        if task.begin_upload_abort() {
                            if let Err(abort_error) = upload.abort_upload(&client, task).await {
                                crate::meow_warn_log!(
                                    "upload_complete",
                                    "abort after source validation failure also failed: {}",
                                    crate::log::redact_secrets(&abort_error.to_string())
                                );
                            }
                        }
                        return Err(error);
                    }
                }
                upload.complete_upload(&client, task).await
            }
            Direction::Download => {
                // Only the concurrent path sets up progress; serial downloads
                // never reach complete() with progress (they finalize inline).
                let progress = task.take_download_progress_after_checkpoint().await?;
                if let Some(p) = progress {
                    let expected = p.total();
                    let actual = tokio::fs::metadata(task.file_path())
                        .await
                        .map(|m| m.len())
                        .map_err(|e| {
                            MeowError::from_io("stat completed download failed".to_string(), e)
                        })?;
                    if actual != expected {
                        return Err(MeowError::from_code(
                            InnerErrorCode::InvalidRange,
                            format!(
                                "download length mismatch on complete: expected {expected}, got {actual}"
                            ),
                        ));
                    }
                    if !p.all_done() {
                        return Err(MeowError::from_code_str(
                            InnerErrorCode::InvalidRange,
                            "download complete called before all parts recorded done",
                        ));
                    }
                    // Success: drop the sidecar. Best effort; a leftover .rcdl is
                    // re-validated (and ignored) on any future download.
                    if let Err(e) = p.delete() {
                        crate::meow_warn_log!("download_complete", "sidecar delete failed: {}", e);
                    }
                }
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod identity_tests {
    use super::download_identity;

    #[test]
    fn refreshed_presigned_credentials_keep_the_same_identity() {
        let old = download_identity(
            "https://example.test/object?part=1&X-Amz-Signature=old&X-Amz-Date=1",
            "\"etag\"",
        );
        let refreshed = download_identity(
            "https://example.test/object?X-Amz-Date=2&X-Amz-Signature=new&part=1",
            "\"etag\"",
        );
        assert_eq!(old, refreshed);
    }

    #[test]
    fn semantic_query_parameters_are_part_of_download_identity() {
        let v1 = download_identity(
            "https://example.test/object?versionId=v1&X-Amz-Signature=old",
            "\"etag\"",
        );
        let v2 = download_identity(
            "https://example.test/object?versionId=v2&X-Amz-Signature=new",
            "\"etag\"",
        );
        assert_ne!(v1, v2);
    }

    #[test]
    fn short_query_names_are_semantic_without_a_recognized_signature_bundle() {
        let first = download_identity("https://example.test/object?sp=chapter-1", "\"etag\"");
        let second = download_identity("https://example.test/object?sp=chapter-2", "\"etag\"");
        assert_ne!(first, second);
    }

    #[test]
    fn repeated_semantic_query_order_is_not_assumed_commutative() {
        let first = download_identity("https://example.test/object?a=1&a=2", "\"etag\"");
        let second = download_identity("https://example.test/object?a=2&a=1", "\"etag\"");
        assert_ne!(first, second);
    }

    #[test]
    fn azure_sas_refresh_drops_only_auth_fields() {
        let first = download_identity(
            "https://example.test/object?versionid=v1&sv=1&sp=r&sig=old",
            "\"etag\"",
        );
        let refreshed = download_identity(
            "https://example.test/object?sig=new&sp=r&sv=2&versionid=v1",
            "\"etag\"",
        );
        assert_eq!(first, refreshed);
    }
}
