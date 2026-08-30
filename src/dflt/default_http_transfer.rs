use async_trait::async_trait;
use reqwest::header::{CONTENT_ENCODING, CONTENT_LENGTH, ETAG};
use reqwest::{Client, Method};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;

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
use super::download_generation_validator;

/// Creates default breakpoint protocol instances.
pub(crate) fn default_breakpoint_arcs() -> (
    Arc<dyn BreakpointUpload + Send + Sync>,
    Arc<dyn BreakpointDownload + Send + Sync>,
) {
    (
        Arc::new(DefaultStyleUpload::default()),
        Arc::new(StandardRangeDownload),
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
    /// True only for clients built by this module, whose effective default
    /// headers are known. An injected reqwest client may add representation
    /// selectors that reqwest does not expose for identity canonicalization.
    client_defaults_are_known: bool,
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
            fallback_download: Arc::new(StandardRangeDownload),
            client_defaults_are_known: true,
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
            fallback_download: Arc::new(StandardRangeDownload),
            client_defaults_are_known: true,
        })
    }

    /// Creates backend with an externally provided `reqwest::Client`.
    ///
    /// Because reqwest does not expose effective client default headers, this
    /// backend validates ETags in the current run but does not reuse download
    /// checkpoint parts written by an earlier process.
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
            fallback_download: Arc::new(StandardRangeDownload),
            client_defaults_are_known: false,
        }
    }

    /// Creates backend with explicit fallback upload/download protocol plugins.
    ///
    /// Task-level protocol instances still take precedence when present.
    /// Download checkpoint reuse across processes is disabled because the
    /// externally provided client's default headers cannot be canonicalized.
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
            client_defaults_are_known: false,
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
    // Retry ownership lives in `run_group`, where the worker cancellation token
    // is available. Keeping this layer to one provider call guarantees that a
    // pause/cancel/close observed during backoff cannot launch a new remote
    // prepare or create a fresh multipart session after control was accepted.
    upload_prepare_once(client, task, upload, local_offset).await
}

async fn upload_prepare_once(
    client: &reqwest::Client,
    task: &TransferTask,
    upload: Arc<dyn BreakpointUpload + Send + Sync>,
    local_offset: u64,
) -> Result<PrepareOutcome, MeowError> {
    if let Some(snapshot) = task.upload_file_snapshot() {
        if let Err(error) = snapshot.validate_total_size(task.total_size()) {
            task.require_upload_abort();
            return Err(error);
        }
        if let Err(error) = snapshot.validate_generation(false).await {
            task.require_upload_abort();
            return Err(error);
        }
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
    /// `None` means the protocol could not provide a stable representation
    /// context. The current run still enforces `validator`, but no sidecar from
    /// an earlier process is trusted.
    identity: Option<String>,
    validator: String,
}

/// Opens, validates, and (when needed) hashes a download progress sidecar on
/// Tokio's blocking pool. Sidecar recovery can read and hash a large existing
/// target, so doing it on an async scheduler worker would stall unrelated
/// pause/cancel commands and transfers sharing that runtime.
async fn load_download_progress_off_thread(
    path: std::path::PathBuf,
    total: u64,
    chunk: u64,
    max_parts: usize,
    generation: Option<VerifiedDownloadGeneration>,
    parallel: bool,
) -> Result<crate::dflt::download_progress::DownloadProgress, MeowError> {
    tokio::task::spawn_blocking(move || {
        let progress = match generation {
            Some(VerifiedDownloadGeneration {
                identity,
                validator,
            }) => {
                let mut progress = if let Some(identity) = identity {
                    if parallel {
                        crate::dflt::download_progress::DownloadProgress::load_or_create(
                            &path, total, chunk, max_parts, &identity,
                        )
                    } else {
                        crate::dflt::download_progress::DownloadProgress::load_or_create_serial(
                            &path, total, chunk, max_parts, &identity,
                        )
                    }
                } else {
                    crate::dflt::download_progress::DownloadProgress::create_unverified(
                        &path, total, chunk, max_parts,
                    )
                }
                .map_err(|error| {
                    let operation = if parallel {
                        "load .rcdl sidecar failed"
                    } else {
                        "load serial .rcdl sidecar failed"
                    };
                    MeowError::from_io(operation.to_owned(), error)
                })?;
                progress.set_expected_validator(validator);
                progress
            }
            None => crate::dflt::download_progress::DownloadProgress::create_unverified(
                &path, total, chunk, max_parts,
            )
            .map_err(|error| {
                let operation = if parallel {
                    "create .rcdl sidecar failed"
                } else {
                    "create serial .rcdl sidecar failed"
                };
                MeowError::from_io(operation.to_owned(), error)
            })?,
        };
        Ok(progress)
    })
    .await
    .map_err(|error| {
        MeowError::from_code(
            InnerErrorCode::IoError,
            format!("download sidecar worker failed: {error}"),
        )
    })?
}

fn is_legacy_presigned_auth_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    matches!(
        key.as_str(),
        "signature" | "awsaccesskeyid" | "ossaccesskeyid" | "googleaccessid" | "expires"
    )
}

fn is_aws_v4_auth_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "x-amz-algorithm"
            | "x-amz-credential"
            | "x-amz-date"
            | "x-amz-expires"
            | "x-amz-signedheaders"
            | "x-amz-signature"
            | "x-amz-security-token"
            | "x-amz-region-set"
    )
}

fn is_google_v4_auth_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "x-goog-algorithm"
            | "x-goog-credential"
            | "x-goog-date"
            | "x-goog-expires"
            | "x-goog-signedheaders"
            | "x-goog-signature"
    )
}

fn is_oss_v4_auth_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "x-oss-signature-version"
            | "x-oss-credential"
            | "x-oss-date"
            | "x-oss-expires"
            | "x-oss-additional-headers"
            | "x-oss-signature"
            | "x-oss-security-token"
    )
}

fn is_presigned_principal_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "x-amz-credential"
            | "x-goog-credential"
            | "x-oss-credential"
            | "awsaccesskeyid"
            | "ossaccesskeyid"
            | "googleaccessid"
            | "skoid"
            | "sktid"
            | "si"
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

/// Binds a validator and stable protocol context to the canonical resource URL.
/// Only a fixed-size digest is returned: an unknown credential-looking query
/// key, request header, tenant id, or ETag can therefore never be written to the
/// checkpoint sidecar verbatim.
fn download_identity(url: &str, validator: &str, resume_context: &[u8]) -> String {
    let mut auth_context: Vec<(String, Vec<u8>)> = Vec::new();
    let resource = reqwest::Url::parse(url)
        .map(|mut parsed| {
            // Never persist URL userinfo. Bind the checkpoint to a one-way hash
            // of its principal instead: password rotation for the same username
            // remains resumable, while a different private-data principal can
            // never reuse completed parts solely because URL/length/ETag match.
            if !parsed.username().is_empty() {
                auth_context.push((
                    "url-username".to_owned(),
                    parsed.username().as_bytes().to_vec(),
                ));
            } else if let Some(password) = parsed.password() {
                auth_context.push((
                    "url-password-without-username".to_owned(),
                    password.as_bytes().to_vec(),
                ));
            }
            let _ = parsed.set_username("");
            let _ = parsed.set_password(None);
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
            for (key, value) in &pairs {
                if is_presigned_principal_key(key) {
                    auth_context.push((
                        format!("query:{}", key.to_ascii_lowercase()),
                        value.as_bytes().to_vec(),
                    ));
                }
            }
            let semantic_pairs: Vec<(String, String)> = pairs
                .into_iter()
                .filter(|(key, _)| {
                    !((aws_v4 && is_aws_v4_auth_key(key))
                        || (google_v4 && is_google_v4_auth_key(key))
                        || (oss_v4 && is_oss_v4_auth_key(key))
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
    auth_context.sort();
    let mut hasher = Sha256::new();
    hasher.update(b"rusty-cat/download-identity/v2\0");
    for value in [resource.as_bytes(), validator.as_bytes(), resume_context] {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value);
    }
    for (kind, value) in auth_context {
        hasher.update((kind.len() as u64).to_be_bytes());
        hasher.update(kind.as_bytes());
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value);
    }
    use std::fmt::Write as _;
    let digest = hasher.finalize();
    let mut identity = String::with_capacity("download-identity-v2-sha256=".len() + 64);
    identity.push_str("download-identity-v2-sha256=");
    for byte in digest {
        let _ = write!(&mut identity, "{byte:02x}");
    }
    identity
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
        // Load (or create) the sidecar BEFORE presizing the target file. The
        // sidecar's own safety guard only invalidates a stale `.rcdl` when the
        // target's on-disk length differs from `total`; if we presized first,
        // the length would already equal `total` and the guard could never
        // fire, letting a stale bitmap survive a deleted/truncated target.
        let progress = load_download_progress_off_thread(
            path.to_path_buf(),
            total,
            task.chunk_size(),
            task.max_parts_in_flight(),
            generation,
            true,
        )
        .await?;
        let watermark = progress.contiguous_watermark();

        // Acquire the real target lock after sidecar recovery (which needs to
        // reopen the path), then reuse this exact handle for every later I/O.
        // Comparing against the prepare-time length closes the stat/open race.
        let locked_len = task.ensure_download_target_file_locked().await?;
        if locked_len != start {
            return Err(MeowError::from_code(
                InnerErrorCode::LocalFileRemoved,
                format!(
                    "download target changed while prepare acquired its lock: expected_len={start} actual_len={locked_len}"
                ),
            ));
        }
        {
            let mut slot = task.download_file_slot().lock().await;
            let file = slot.as_mut().ok_or_else(|| {
                MeowError::from_code_str(
                    InnerErrorCode::InvalidTaskState,
                    "locked download target missing during presize",
                )
            })?;
            // Never truncate existing bytes: set_len(total) only establishes the
            // positioned-write grid and preserves parts validated by the sidecar.
            file.set_len(total)
                .await
                .map_err(|e| MeowError::from_io("presize set_len failed".to_string(), e))?;
            file.sync_all()
                .await
                .map_err(|e| MeowError::from_io("presize sync failed".to_string(), e))?;
        }

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

    // Serial and parallel paths share the same digest sidecar. A serial task may
    // therefore resume a contiguous prefix created by either mode without ever
    // trusting the pre-sized file length by itself.
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
    // URL + length cannot prove that a persisted prefix belongs to the current
    // remote representation. Without a verified generation the helper creates
    // a fresh bitmap; the first 206 may still latch a strong ETag for this run.
    let progress = load_download_progress_off_thread(
        path.to_path_buf(),
        total,
        task.chunk_size(),
        task.max_parts_in_flight(),
        generation,
        false,
    )
    .await?;
    let locked_len = task.ensure_download_target_file_locked().await?;
    if locked_len != start {
        return Err(MeowError::from_code(
            InnerErrorCode::LocalFileRemoved,
            format!(
                "download target changed while prepare acquired its lock: expected_len={start} actual_len={locked_len}"
            ),
        ));
    }
    if let Ok(mut slot) = task.download_progress().try_lock() {
        *slot = Some(progress);
    } else {
        return Err(MeowError::from_code_str(
            InnerErrorCode::InvalidTaskState,
            "download progress slot unexpectedly locked during serial prepare",
        ));
    }

    // A legacy length-only local file has no proof that its bytes belong to the
    // current remote generation. Never manufacture trust by hashing those same
    // unknown bytes: without a matching sidecar the fresh bitmap below forces
    // a safe restart from zero, including the same-length/full-file case.

    // Serial layout remains append-like: the visible length is exactly the
    // verified contiguous prefix. Compacting after target sync prevents sparse
    // parallel bits or a torn legacy tail from being trusted on the next run.
    let resume_offset = task.retain_serial_download_contiguous_progress().await?;
    crate::meow_key_log!(
        "download_prepare",
        "prepared serial digest resume: observed_len={} resume_watermark={} remote_total={}",
        start,
        resume_offset,
        total
    );
    Ok(PrepareOutcome {
        next_offset: resume_offset,
        total_size: total,
    })
}

/// Runs download prepare stage and computes resume offset/total size.
async fn download_prepare(
    client: &reqwest::Client,
    task: &TransferTask,
    download: Arc<dyn BreakpointDownload + Send + Sync>,
    allow_persisted_resume_identity: bool,
    _local_offset: u64,
) -> Result<PrepareOutcome, MeowError> {
    crate::meow_flow_log!(
        "download_prepare",
        "start: file={} path={}",
        task.file_name(),
        task.file_path().display()
    );
    let path = task.file_path();
    crate::dflt::download_progress::ensure_target_outside_sidecar_namespace(path).map_err(
        |error| MeowError::from_io("validate download target namespace".to_owned(), error),
    )?;
    // A not-yet-created nested target cannot be canonicalized for its path
    // lease until its parent exists. Create only the non-empty parent tree;
    // bare relative targets correctly use the process working directory.
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        tokio::fs::create_dir_all(parent).await.map_err(|e| {
            MeowError::from_io(
                format!("create download dir failed: {}", parent.display()),
                e,
            )
        })?;
    }
    task.ensure_download_target_lease().await?;
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
        .inspect_err(|e| {
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
        .inspect_err(|e| {
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
    let generation = if let Some(validator) = download_generation_validator(head_resp.headers()) {
        let identity = if allow_persisted_resume_identity {
            download
                .resume_identity(task)?
                .map(|context| download_identity(&download.range_url(task), &validator, &context))
        } else {
            None
        };
        Some(VerifiedDownloadGeneration {
            identity,
            validator,
        })
    } else {
        None
    };
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
                let allow_persisted_resume_identity =
                    self.client_defaults_are_known && task.http_client_ref().is_none();
                download_prepare(
                    &client,
                    task,
                    self.download_arc(task),
                    allow_persisted_resume_identity,
                    local_offset,
                )
                .await
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
                download_one_chunk_part_positioned(
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
                let expected = task
                    .download_progress()
                    .lock()
                    .map_err(|_| {
                        MeowError::from_code_str(
                            InnerErrorCode::LockPoisoned,
                            "download checkpoint lock poisoned",
                        )
                    })?
                    .as_ref()
                    .ok_or_else(|| {
                        MeowError::from_code_str(
                            InnerErrorCode::InvalidTaskState,
                            "download progress missing during complete",
                        )
                    })?
                    .total();
                task.finalize_download_content(expected).await?;
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod identity_tests {
    use super::download_identity as download_identity_with_context;

    fn download_identity_with_headers(
        url: &str,
        validator: &str,
        headers: &reqwest::header::HeaderMap,
    ) -> String {
        let context = crate::http_breakpoint::canonical_resume_headers(headers.clone());
        download_identity_with_context(url, validator, &context)
    }

    fn download_identity(url: &str, validator: &str) -> String {
        download_identity_with_context(url, validator, b"test-stable-range-context")
    }

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
    fn url_userinfo_is_not_persisted_in_download_identity() {
        let identity = download_identity(
            "https://alice:s3cr%65t@example.test/object?versionId=v1",
            "\"etag\"",
        );

        assert!(identity.starts_with("download-identity-v2-sha256="));
        assert_eq!(identity.len(), "download-identity-v2-sha256=".len() + 64);
        assert!(!identity.contains("alice"));
        assert!(!identity.contains("s3cr"));
        assert!(!identity.contains('@'));
        assert!(!identity.contains("versionId"));
    }

    #[test]
    fn basic_auth_password_rotation_is_stable_but_principal_rotation_is_not() {
        let old = download_identity(
            "https://same-user:old-password@example.test/object?versionId=v1",
            "\"etag\"",
        );
        let rotated = download_identity(
            "https://same-user:new-password@example.test/object?versionId=v1",
            "\"etag\"",
        );
        let other_principal = download_identity(
            "https://other-user:new-password@example.test/object?versionId=v1",
            "\"etag\"",
        );
        let without_userinfo =
            download_identity("https://example.test/object?versionId=v1", "\"etag\"");

        assert_eq!(old, rotated);
        assert_ne!(rotated, other_principal);
        assert_ne!(rotated, without_userinfo);
    }

    #[test]
    fn userinfo_redaction_preserves_resource_boundaries() {
        let baseline = download_identity(
            "https://user:password@example.test/object?versionId=v1",
            "\"etag\"",
        );
        let other_host = download_identity(
            "https://user:password@cdn.example.test/object?versionId=v1",
            "\"etag\"",
        );
        let other_path = download_identity(
            "https://user:password@example.test/other?versionId=v1",
            "\"etag\"",
        );
        let other_query = download_identity(
            "https://user:password@example.test/object?versionId=v2",
            "\"etag\"",
        );

        assert_ne!(baseline, other_host);
        assert_ne!(baseline, other_path);
        assert_ne!(baseline, other_query);
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

    #[test]
    fn oss_process_remains_a_semantic_representation_selector() {
        let resized = download_identity(
            "https://example.test/image?x-oss-process=image/resize,w_100&x-oss-credential=acct/a&x-oss-date=1&x-oss-signature=old",
            "\"etag\"",
        );
        let cropped = download_identity(
            "https://example.test/image?x-oss-process=image/crop,w_100&x-oss-credential=acct/a&x-oss-date=2&x-oss-signature=new",
            "\"etag\"",
        );
        assert_ne!(resized, cropped);
    }

    #[test]
    fn authorization_header_is_hashed_and_partitions_private_representations() {
        let mut alice_headers = reqwest::header::HeaderMap::new();
        alice_headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_static("Bearer alice-token"),
        );
        let mut bob_headers = reqwest::header::HeaderMap::new();
        bob_headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_static("Bearer bob-token"),
        );
        let alice = download_identity_with_headers(
            "https://example.test/private",
            "\"etag\"",
            &alice_headers,
        );
        let bob = download_identity_with_headers(
            "https://example.test/private",
            "\"etag\"",
            &bob_headers,
        );

        assert_ne!(alice, bob);
        assert!(!alice.contains("alice-token"));
        assert!(!bob.contains("bob-token"));
    }

    #[test]
    fn representation_headers_are_hashed_and_partition_download_identity() {
        let mut english = reqwest::header::HeaderMap::new();
        english.insert(
            reqwest::header::ACCEPT_LANGUAGE,
            reqwest::header::HeaderValue::from_static("en-US"),
        );
        english.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("application/octet-stream"),
        );
        let mut chinese = english.clone();
        chinese.insert(
            reqwest::header::ACCEPT_LANGUAGE,
            reqwest::header::HeaderValue::from_static("zh-CN"),
        );

        let english_identity = download_identity_with_headers(
            "https://example.test/localized-object",
            "\"etag\"",
            &english,
        );
        let chinese_identity = download_identity_with_headers(
            "https://example.test/localized-object",
            "\"etag\"",
            &chinese,
        );

        assert_ne!(english_identity, chinese_identity);
        assert!(english_identity.starts_with("download-identity-v2-sha256="));
        assert!(!english_identity.contains("en-US"));
        assert!(!chinese_identity.contains("zh-CN"));
    }

    #[test]
    fn unknown_credential_query_is_never_persisted_verbatim() {
        let identity = download_identity(
            "https://example.test/object?custom_api_token=top-secret&view=raw",
            "\"etag-secret\"",
        );

        assert!(identity.starts_with("download-identity-v2-sha256="));
        assert!(!identity.contains("custom_api_token"));
        assert!(!identity.contains("top-secret"));
        assert!(!identity.contains("etag-secret"));
    }

    #[test]
    fn request_header_order_does_not_change_download_identity() {
        let mut first = reqwest::header::HeaderMap::new();
        first.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("application/octet-stream"),
        );
        first.insert(
            reqwest::header::ACCEPT_LANGUAGE,
            reqwest::header::HeaderValue::from_static("en-US"),
        );
        let mut reversed = reqwest::header::HeaderMap::new();
        reversed.insert(
            reqwest::header::ACCEPT_LANGUAGE,
            reqwest::header::HeaderValue::from_static("en-US"),
        );
        reversed.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("application/octet-stream"),
        );

        assert_eq!(
            download_identity_with_headers("https://example.test/object", "\"etag\"", &first,),
            download_identity_with_headers("https://example.test/object", "\"etag\"", &reversed,)
        );
    }

    #[test]
    fn injected_http_client_defaults_are_not_assumed_identity_safe() {
        let injected = super::DefaultHttpTransfer::with_client(reqwest::Client::new());
        let internal = super::DefaultHttpTransfer::try_with_http_timeouts(
            std::time::Duration::from_secs(5),
            std::time::Duration::from_secs(30),
        )
        .expect("internal client fixture");

        assert!(!injected.client_defaults_are_known);
        assert!(internal.client_defaults_are_known);
    }
}
