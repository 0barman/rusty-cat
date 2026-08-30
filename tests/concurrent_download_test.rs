//! End-to-end integration tests for opt-in intra-file **parallel + resumable**
//! download (optimization ④, download side). These are the FIRST runtime
//! verification of the whole feature: they drive the real `MeowClient` executor
//! (`enqueue_and_wait` -> scheduler -> `run_group_parallel` ->
//! `download_one_chunk_part_positioned`) against a from-scratch HTTP/1.1 range
//! server on a `tokio::net::TcpListener`, with no cloud dependency.
//!
//! What each scenario locks down:
//!   1. concurrent byte-exactness (positioned writes, no gaps/overwrites);
//!   2. out-of-order completion (high offsets land first) still byte-exact;
//!   3. resume re-fetches ONLY the parts a prior run left unfinished;
//!   4. the load-before-presize integrity fix: a deleted/truncated target
//!      discards the stale `.rcdl` and re-fetches ALL parts (a regression to
//!      presize-before-load would fetch fewer and leave a corrupt file);
//!   5. a `200 OK` to a Range request fails the concurrent download loudly;
//!   6. the serial (default) path is byte-exact and removes `.rcdl` on success;
//!   7. a corrupt/stale `.rcdl` is ignored safely by the serial path;
//!   8. a failed serial run persists digests and resumes only the missing part.
//!
//! Determinism (why these are not flaky): the server is fully controlled by
//! atomics, localhost is loss-free, and every "partial" is engineered so the
//! FAILING part is the LAST offset. The parallel driver never cancels siblings
//! on a part failure (it lets in-flight parts settle) and each part persists its
//! sidecar bit BEFORE returning, so a short last part leaves EXACTLY N-1
//! durable parts regardless of scheduling. `InvalidRange` is non-retryable, so
//! the short body fails fast with no retry/backoff timing to race.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{HeaderValue, IF_MATCH};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use rusty_cat::api::{
    BreakpointDownload, DownloadPounceBuilder, DownloadRangeGetCtx, MeowClient, MeowConfig,
    MeowError, StandardRangeDownload, TaskOutcome, TransferStatus, TransferTask,
};

struct CustomRangeDownloadWithoutPersistedIdentity;

impl BreakpointDownload for CustomRangeDownloadWithoutPersistedIdentity {
    fn supports_parallel_parts(&self) -> bool {
        true
    }

    fn merge_range_get_headers(&self, ctx: DownloadRangeGetCtx<'_>) -> Result<(), MeowError> {
        StandardRangeDownload.merge_range_get_headers(ctx)
    }
}

#[derive(Clone, Copy)]
enum IfMatchMutation {
    Preserve,
    Remove,
    Replace,
    Append,
}

/// Observes the executor-owned conditional header at the exact boundary where
/// a provider signs or otherwise finalizes one range request.
///
/// Azure Shared Key includes `If-Match` in its MAC input, so this hook must see
/// the prepared strong ETag before it delegates to the standard Range behavior.
struct InspectPreparedIfMatchDownload {
    parallel: bool,
    mutation: IfMatchMutation,
    observed: Arc<Mutex<Vec<Option<String>>>>,
    range_url_calls: Arc<AtomicUsize>,
}

impl BreakpointDownload for InspectPreparedIfMatchDownload {
    fn supports_parallel_parts(&self) -> bool {
        self.parallel
    }

    fn range_url(&self, task: &TransferTask) -> String {
        self.range_url_calls.fetch_add(1, Ordering::SeqCst);
        task.url().to_owned()
    }

    fn merge_range_get_headers(&self, ctx: DownloadRangeGetCtx<'_>) -> Result<(), MeowError> {
        self.observed
            .lock()
            .expect("If-Match observation lock")
            .push(
                ctx.base
                    .get(IF_MATCH)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned),
            );

        StandardRangeDownload.merge_range_get_headers(DownloadRangeGetCtx {
            task: ctx.task,
            range_value: ctx.range_value,
            base: ctx.base,
        })?;

        match self.mutation {
            IfMatchMutation::Preserve => {}
            IfMatchMutation::Remove => {
                ctx.base.remove(IF_MATCH);
            }
            IfMatchMutation::Replace => {
                ctx.base
                    .insert(IF_MATCH, HeaderValue::from_static("\"provider-overwrite\""));
            }
            IfMatchMutation::Append => {
                ctx.base
                    .append(IF_MATCH, HeaderValue::from_static("\"provider-append\""));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Mock HTTP/1.1 range server
// ---------------------------------------------------------------------------

/// A single long-lived range server whose behavior is fully controlled by
/// atomics, so one instance can serve a "run 1" (inject a fault) and a "run 2"
/// (all-correct) on the SAME port. The port matters: the `.rcdl` sidecar is
/// bound to the semantic range URL plus the freshly observed strong ETag, so a
/// resume MUST hit the same URL or the sidecar is treated as stale.
struct RangeServer {
    /// Exact source payload; every 206 slices out of this.
    body: RwLock<Vec<u8>>,
    etag: RwLock<String>,
    range_etag_override: RwLock<Option<String>>,
    omit_range_etag: AtomicBool,
    azure_blob_headers: AtomicBool,
    /// Optional second generation used for ranges whose start offset is at or
    /// beyond `alternate_from`. This makes an in-run generation switch fully
    /// deterministic even when requests finish out of order.
    alternate_body: RwLock<Option<Vec<u8>>>,
    alternate_etag: RwLock<Option<String>>,
    alternate_from: AtomicI64,
    matching_if_match_hits: AtomicUsize,
    /// Count of range GETs served (the resume/refetch witness). Resettable.
    hits: AtomicUsize,
    /// Live concurrent range handlers and the max ever observed (fan-out proof).
    in_flight: AtomicUsize,
    peak: AtomicUsize,
    /// When true, answer every request with `200 OK` + full body (Range ignored).
    force_200: AtomicBool,
    /// If `>= 0`, the part whose start offset equals this value gets a body one
    /// byte short of its declared `Content-Range` (a clean, complete-but-short
    /// response → non-retryable `InvalidRange`). `-1` disables.
    short_body_at: AtomicI64,
    /// Uniform per-response delay (ms); used to force genuine fan-out so the peak
    /// concurrency assertion is meaningful without being flaky.
    per_part_delay_ms: AtomicU64,
    /// When true, delay each response inversely to its offset so HIGH offsets
    /// finish FIRST (stresses out-of-order positioned writes + the watermark).
    out_of_order: AtomicBool,
}

impl RangeServer {
    fn new(body: Vec<u8>) -> Arc<Self> {
        Arc::new(Self {
            body: RwLock::new(body),
            etag: RwLock::new("\"generation-a\"".to_string()),
            range_etag_override: RwLock::new(None),
            omit_range_etag: AtomicBool::new(false),
            azure_blob_headers: AtomicBool::new(false),
            alternate_body: RwLock::new(None),
            alternate_etag: RwLock::new(None),
            alternate_from: AtomicI64::new(-1),
            matching_if_match_hits: AtomicUsize::new(0),
            hits: AtomicUsize::new(0),
            in_flight: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            force_200: AtomicBool::new(false),
            short_body_at: AtomicI64::new(-1),
            per_part_delay_ms: AtomicU64::new(0),
            out_of_order: AtomicBool::new(false),
        })
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
    fn reset_hits(&self) {
        self.hits.store(0, Ordering::SeqCst);
    }
    fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }
    fn set_short_body_at(&self, offset: i64) {
        self.short_body_at.store(offset, Ordering::SeqCst);
    }
    fn set_force_200(&self, on: bool) {
        self.force_200.store(on, Ordering::SeqCst);
    }
    fn set_per_part_delay_ms(&self, ms: u64) {
        self.per_part_delay_ms.store(ms, Ordering::SeqCst);
    }
    fn set_out_of_order(&self, on: bool) {
        self.out_of_order.store(on, Ordering::SeqCst);
    }
    fn replace_body_same_length(&self, body: Vec<u8>) {
        let mut current = self.body.write().expect("body write lock");
        assert_eq!(current.len(), body.len(), "test generation must keep total");
        *current = body;
    }
    fn set_etag(&self, etag: &str) {
        *self.etag.write().expect("etag write lock") = etag.to_string();
    }
    fn set_range_etag_override(&self, etag: Option<&str>) {
        *self
            .range_etag_override
            .write()
            .expect("range etag write lock") = etag.map(str::to_owned);
    }
    fn set_omit_range_etag(&self, omit: bool) {
        self.omit_range_etag.store(omit, Ordering::SeqCst);
    }
    fn set_azure_blob_headers(&self, enabled: bool) {
        self.azure_blob_headers.store(enabled, Ordering::SeqCst);
    }
    fn switch_generation_from(&self, offset: i64, body: Vec<u8>, etag: &str) {
        let current_len = self.body.read().expect("body read lock").len();
        assert_eq!(current_len, body.len(), "test generation must keep total");
        *self.alternate_body.write().expect("alternate body lock") = Some(body);
        *self.alternate_etag.write().expect("alternate etag lock") = Some(etag.to_owned());
        self.alternate_from.store(offset, Ordering::SeqCst);
    }
    fn matching_if_match_hits(&self) -> usize {
        self.matching_if_match_hits.load(Ordering::SeqCst)
    }
}

/// Binds an ephemeral localhost port, spawns the accept loop, and returns the
/// `http://127.0.0.1:PORT/f.bin` URL. The caller keeps its own `Arc<RangeServer>`
/// clone to flip knobs / read counters between runs.
async fn spawn_range_server(server: Arc<RangeServer>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => break,
            };
            let server = server.clone();
            tokio::spawn(async move {
                handle_conn(&mut sock, &server).await;
            });
        }
    });
    format!("http://{addr}/f.bin")
}

async fn handle_conn(sock: &mut TcpStream, server: &RangeServer) {
    // Read request headers up to the terminating CRLFCRLF (GET has no body).
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut tmp = [0u8; 1024];
    loop {
        let n = match sock.read(&mut tmp).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => return,
        };
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 16 * 1024 {
            break;
        }
    }
    let req = String::from_utf8_lossy(&buf);
    let method = req
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().next())
        .unwrap_or_default();

    // Parse `Range: bytes=a-b` (case-insensitive header name).
    let range = req.lines().find_map(|l| {
        let l = l.trim();
        let low = l.to_ascii_lowercase();
        low.strip_prefix("range:")
            .map(|_| l["range:".len()..].trim().to_string())
    });

    let mut body = server.body.read().expect("body read lock").clone();
    let etag = server.etag.read().expect("etag read lock").clone();
    let mut range_etag = server
        .range_etag_override
        .read()
        .expect("range etag read lock")
        .clone()
        .unwrap_or_else(|| etag.clone());
    let mut total = body.len();
    let azure_headers = if server.azure_blob_headers.load(Ordering::SeqCst) {
        "x-ms-blob-type: BlockBlob\r\nx-ms-version: 2009-09-19\r\nx-ms-request-id: 68c7adf4-b01e-0047-1032-35fc95000000\r\n"
    } else {
        ""
    };

    if method == "HEAD" {
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nETag: {etag}\r\n{azure_headers}Connection: close\r\n\r\n"
        );
        let _ = sock.write_all(head.as_bytes()).await;
        let _ = sock.flush().await;
        return;
    }

    if server.force_200.load(Ordering::SeqCst) {
        // Server ignores Range and streams the whole object with a 200.
        let head =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n");
        let _ = sock.write_all(head.as_bytes()).await;
        let _ = sock.write_all(&body).await;
        let _ = sock.flush().await;
        return;
    }

    let Some(r) = range else {
        // No Range (e.g. a stray HEAD). Content-Length 0 makes HEAD-based sizing
        // fail on purpose — every test supplies `with_total_size` to skip HEAD.
        let _ = sock
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
        let _ = sock.flush().await;
        return;
    };

    let if_match = req.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("if-match")
            .then(|| value.trim())
    });
    if if_match == Some(etag.as_str()) {
        server.matching_if_match_hits.fetch_add(1, Ordering::SeqCst);
    }

    server.hits.fetch_add(1, Ordering::SeqCst);
    let cur = server.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    server.peak.fetch_max(cur, Ordering::SeqCst);

    let spec = r.trim_start_matches("bytes=");
    let (a_s, b_s) = spec.split_once('-').unwrap_or(("0", "0"));
    let a: usize = a_s.trim().parse().unwrap_or(0);
    let alternate_from = server.alternate_from.load(Ordering::SeqCst);
    if alternate_from >= 0 && a >= alternate_from as usize {
        if let Some(alternate) = server
            .alternate_body
            .read()
            .expect("alternate body lock")
            .clone()
        {
            body = alternate;
            total = body.len();
        }
        if let Some(alternate) = server
            .alternate_etag
            .read()
            .expect("alternate etag lock")
            .clone()
        {
            range_etag = alternate;
        }
    }
    let b_trim = b_s.trim();
    let b: usize = if b_trim.is_empty() {
        total.saturating_sub(1)
    } else {
        b_trim.parse().unwrap_or(0)
    };
    let end = b.min(total.saturating_sub(1));

    // Optional delays (applied while `in_flight` is counted, so they shape peak).
    let per = server.per_part_delay_ms.load(Ordering::SeqCst);
    if per > 0 {
        tokio::time::sleep(Duration::from_millis(per)).await;
    }
    if server.out_of_order.load(Ordering::SeqCst) {
        // Inverse-to-offset delay: offset 0 waits longest, last offset shortest,
        // so higher parts complete first.
        let delay_ms = (total.saturating_sub(a) as u64) / 200;
        if delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
    }

    let slice = &body[a..=end];
    let short = server.short_body_at.load(Ordering::SeqCst) == a as i64;
    // Full Content-Range (so the SDK's expected length is the FULL part) but a
    // body one byte shorter → the SDK detects "range body short" (InvalidRange).
    let send: &[u8] = if short {
        &slice[..slice.len().saturating_sub(1)]
    } else {
        slice
    };

    let etag_header = if server.omit_range_etag.load(Ordering::SeqCst) {
        String::new()
    } else {
        format!("ETag: {range_etag}\r\n")
    };
    let head = format!(
        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {a}-{end}/{total}\r\nContent-Length: {}\r\n{etag_header}{azure_headers}Connection: close\r\n\r\n",
        send.len()
    );
    let _ = sock.write_all(head.as_bytes()).await;
    let _ = sock.write_all(send).await;
    let _ = sock.flush().await;

    server.in_flight.fetch_sub(1, Ordering::SeqCst);
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Deterministic payload: byte i == (i % 251). 251 is prime, so the pattern does
/// not align to any power-of-two chunk boundary — an off-by-chunk write shows up.
fn make_body(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

fn temp_target(case: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "rusty_cat_concurrent_download_tests_{}",
        std::process::id()
    ));
    fs::create_dir_all(&root).expect("concurrent download test root");
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    root.join(format!(
        "rusty_cat_cdl_{case}_{}_{ts}.bin",
        std::process::id()
    ))
}

/// Mirrors the private hashed namespace used by
/// `download_progress::sidecar_path`, which is `pub(crate)` and therefore not
/// importable from this integration test crate.
fn rcdl_path(target: &Path) -> PathBuf {
    let component = target.file_name().unwrap_or(target.as_os_str());
    let mut hasher = Sha256::new();
    hasher.update(b"rusty-cat/download-sidecar-path/v1\0");
    update_sidecar_hasher_with_os_str(&mut hasher, component);
    let digest = hasher.finalize();
    let mut name = String::with_capacity(digest.len() * 2 + ".rcdl".len());
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut name, "{byte:02x}");
    }
    name.push_str(".rcdl");
    let parent = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    parent.join(".rusty-cat").join(name)
}

#[cfg(unix)]
fn update_sidecar_hasher_with_os_str(hasher: &mut Sha256, value: &std::ffi::OsStr) {
    use std::os::unix::ffi::OsStrExt;
    hasher.update(value.as_bytes());
}

#[cfg(windows)]
fn update_sidecar_hasher_with_os_str(hasher: &mut Sha256, value: &std::ffi::OsStr) {
    use std::os::windows::ffi::OsStrExt;
    for unit in value.encode_wide() {
        hasher.update(unit.to_le_bytes());
    }
}

#[cfg(not(any(unix, windows)))]
fn update_sidecar_hasher_with_os_str(hasher: &mut Sha256, value: &std::ffi::OsStr) {
    hasher.update(value.as_encoded_bytes());
}

fn cleanup(target: &Path) {
    let _ = fs::remove_file(target);
    let _ = fs::remove_file(rcdl_path(target));
}

fn initialize_rcdl_namespace(target: &Path) {
    let sidecar = rcdl_path(target);
    let namespace = sidecar.parent().expect("sidecar namespace");
    fs::create_dir_all(namespace).expect("create sidecar namespace fixture");
    let marker = namespace.join(".download-state-v1");
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
    {
        Ok(file) => file.sync_all().expect("sync sidecar namespace marker"),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(marker).expect("sidecar namespace marker");
            assert!(metadata.is_file());
            assert!(!metadata.file_type().is_symlink());
            assert_eq!(metadata.len(), 0);
        }
        Err(error) => panic!("create sidecar namespace marker: {error}"),
    }
}

/// One full download run against `url` into `target`. A fresh `MeowClient` per
/// run avoids duplicate-task issues and mirrors a real restart. `max_parts == 1`
/// (or default) takes the legacy serial path; `> 1` takes the concurrent path
/// (StandardRangeDownload is the default protocol — we intentionally do NOT call
/// `with_breakpoint_download`). `with_total_size` skips the HEAD probe.
async fn run_download(
    url: &str,
    target: &Path,
    total: u64,
    chunk: u64,
    max_parts: usize,
) -> Result<TaskOutcome, MeowError> {
    run_download_inner(url, target, total, chunk, max_parts, true).await
}

async fn run_download_resolving_head(
    url: &str,
    target: &Path,
    total: u64,
    chunk: u64,
    max_parts: usize,
) -> Result<TaskOutcome, MeowError> {
    run_download_inner(url, target, total, chunk, max_parts, false).await
}

async fn run_download_inner(
    url: &str,
    target: &Path,
    total: u64,
    chunk: u64,
    max_parts: usize,
    use_total_hint: bool,
) -> Result<TaskOutcome, MeowError> {
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_download_concurrency(1)
            .http_timeout(Duration::from_secs(30))
            .build()
            .expect("valid config"),
    );
    let file_name = target
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("f.bin")
        .to_string();

    let mut builder = DownloadPounceBuilder::new(file_name, target, chunk, url.to_string())
        .with_max_parts_in_flight(max_parts);
    if use_total_hint {
        builder = builder.with_total_size(total);
    }
    let task = builder.build();

    let result = client.enqueue_and_wait(task, |_record| {}).await;
    client.close().await.expect("close client");
    result
}

async fn run_custom_protocol_download(
    url: &str,
    target: &Path,
    chunk: u64,
    max_parts: usize,
) -> Result<TaskOutcome, MeowError> {
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_download_concurrency(1)
            .http_timeout(Duration::from_secs(30))
            .build()
            .expect("valid config"),
    );
    let task = DownloadPounceBuilder::new("custom-protocol.bin", target, chunk, url.to_string())
        .with_breakpoint_download(Arc::new(CustomRangeDownloadWithoutPersistedIdentity))
        .with_max_parts_in_flight(max_parts)
        .with_max_chunk_retries(0)
        .build();

    let result = client.enqueue_and_wait(task, |_record| {}).await;
    client.close().await.expect("close custom protocol client");
    result
}

async fn run_if_match_contract_download(
    url: &str,
    target: &Path,
    chunk: u64,
    max_parts: usize,
    protocol: Arc<InspectPreparedIfMatchDownload>,
) -> Result<TaskOutcome, MeowError> {
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_download_concurrency(1)
            .http_timeout(Duration::from_secs(30))
            .build()
            .expect("valid config"),
    );
    let task = DownloadPounceBuilder::new("if-match-contract.bin", target, chunk, url.to_string())
        .with_breakpoint_download(protocol)
        .with_max_parts_in_flight(max_parts)
        // Contract violations use InvalidRange and must not consume this retry
        // budget or call the provider hook more than once.
        .with_max_chunk_retries(3)
        .build();

    let result = client.enqueue_and_wait(task, |_record| {}).await;
    client
        .close()
        .await
        .expect("close If-Match contract client");
    result
}

async fn run_injected_client_download(
    url: &str,
    target: &Path,
    chunk: u64,
    max_parts: usize,
) -> Result<TaskOutcome, MeowError> {
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_download_concurrency(1)
            .http_client(reqwest::Client::new())
            .build()
            .expect("valid injected-client config"),
    );
    let task = DownloadPounceBuilder::new("injected-client.bin", target, chunk, url.to_string())
        .with_max_parts_in_flight(max_parts)
        .with_max_chunk_retries(0)
        .build();

    let result = client.enqueue_and_wait(task, |_record| {}).await;
    client.close().await.expect("close injected HTTP client");
    result
}

fn assert_byte_exact(target: &Path, expected: &[u8]) {
    let got = fs::read(target).expect("read downloaded file");
    assert_eq!(
        got.len(),
        expected.len(),
        "downloaded length mismatch: got {}, expected {}",
        got.len(),
        expected.len()
    );
    assert!(
        got == expected,
        "downloaded bytes differ from source (first mismatch at {:?})",
        got.iter().zip(expected).position(|(a, b)| a != b)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn download_creates_missing_nested_parent_before_target_lease() {
    let total = 4_097usize;
    let body = make_body(total);
    let server = RangeServer::new(body.clone());
    let url = spawn_range_server(server).await;
    let root = temp_target("missing_nested_parent").with_extension("dir");
    let target = root.join("one").join("two").join("payload.bin");
    let _ = fs::remove_dir_all(&root);
    assert!(!root.exists(), "fixture parent tree must start absent");

    let outcome = run_download(&url, &target, total as u64, 1_000, 1).await;
    assert!(outcome.is_ok(), "download failed: {:?}", outcome.err());
    assert_byte_exact(&target, &body);

    let _ = fs::remove_dir_all(root);
}

// ---------------------------------------------------------------------------
// Scenario 1 — concurrent byte-exact
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_download_is_byte_exact() {
    // 20011 bytes, chunk 1000 => 21 parts, last part is a short 11-byte tail.
    let total = 20_011usize;
    let chunk = 1000u64;
    let body = make_body(total);
    let server = RangeServer::new(body.clone());
    // A uniform small delay forces the 4-wide window to genuinely overlap so the
    // peak-concurrency assertion is meaningful (not a serialized fast-path).
    server.set_per_part_delay_ms(25);
    let url = spawn_range_server(server.clone()).await;

    let target = temp_target("byte_exact");
    cleanup(&target);

    let outcome = run_download(&url, &target, total as u64, chunk, 4).await;
    assert!(outcome.is_ok(), "download failed: {:?}", outcome.err());

    assert_byte_exact(&target, &body);
    assert!(
        !rcdl_path(&target).exists(),
        "the .rcdl sidecar must be deleted after a successful download"
    );
    let peak = server.peak();
    assert!(
        (2..=4).contains(&peak),
        "expected bounded genuine fan-out (2..=4), observed peak={peak}"
    );
    // Every part fetched exactly once (21 parts, no re-fetch on a clean run).
    assert_eq!(server.hits(), 21, "expected exactly one GET per part");

    cleanup(&target);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordinary_legacy_dot_rcdl_file_is_preserved() {
    const SENTINEL: &[u8] = b"this is a user file, not resume metadata";
    let body = make_body(4_097);
    let server = RangeServer::new(body.clone());
    let url = spawn_range_server(server).await;
    let root = temp_target("legacy_rcdl_sentinel").with_extension("dir");
    fs::create_dir_all(&root).unwrap();
    let target = root.join("foo");
    let legacy = root.join("foo.rcdl");
    fs::write(&legacy, SENTINEL).unwrap();

    run_download(&url, &target, body.len() as u64, 1_000, 4)
        .await
        .expect("download alongside legacy .rcdl file");

    assert_byte_exact(&target, &body);
    assert_eq!(fs::read(&legacy).unwrap(), SENTINEL);
    let _ = fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn foo_and_foo_dot_rcdl_can_download_concurrently_without_path_collision() {
    let foo_body = make_body(5_003);
    let mut rcdl_body = make_body(6_007);
    for byte in &mut rcdl_body {
        *byte = byte.wrapping_add(73);
    }
    let foo_server = RangeServer::new(foo_body.clone());
    let rcdl_server = RangeServer::new(rcdl_body.clone());
    foo_server.set_per_part_delay_ms(10);
    rcdl_server.set_per_part_delay_ms(10);
    let foo_url = spawn_range_server(foo_server).await;
    let rcdl_url = spawn_range_server(rcdl_server).await;
    let root = temp_target("foo_and_foo_rcdl").with_extension("dir");
    fs::create_dir_all(&root).unwrap();
    let foo = root.join("foo");
    let foo_rcdl = root.join("foo.rcdl");

    let (foo_result, rcdl_result) = tokio::join!(
        run_download(&foo_url, &foo, foo_body.len() as u64, 1_000, 4),
        run_download(&rcdl_url, &foo_rcdl, rcdl_body.len() as u64, 1_000, 4)
    );
    foo_result.expect("download foo");
    rcdl_result.expect("download foo.rcdl");

    assert_byte_exact(&foo, &foo_body);
    assert_byte_exact(&foo_rcdl, &rcdl_body);
    let _ = fs::remove_dir_all(root);
}

// ---------------------------------------------------------------------------
// Scenario 2 — out-of-order completion still byte-exact
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn out_of_order_parts_still_byte_exact() {
    // 16000 bytes, chunk 1000 => 16 parts. The server delays inversely to offset
    // so HIGH parts land FIRST; positioned writes + the contiguous watermark must
    // still reassemble a byte-exact file.
    let total = 16_000usize;
    let chunk = 1000u64;
    let body = make_body(total);
    let server = RangeServer::new(body.clone());
    server.set_out_of_order(true);
    let url = spawn_range_server(server.clone()).await;

    let target = temp_target("ooo");
    cleanup(&target);

    let outcome = run_download(&url, &target, total as u64, chunk, 4).await;
    assert!(outcome.is_ok(), "download failed: {:?}", outcome.err());

    assert_byte_exact(&target, &body);
    assert!(
        !rcdl_path(&target).exists(),
        "the .rcdl sidecar must be deleted after a successful download"
    );

    cleanup(&target);
}

// ---------------------------------------------------------------------------
// Scenario 3 — resume re-fetches ONLY the missing part
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_refetches_only_missing_parts() {
    // 8000 bytes, chunk 1000 => 8 parts. Run 1 serves the last part (offset 7000)
    // one byte short → that part fails (non-retryable InvalidRange). Because the
    // parallel driver never cancels siblings on failure and each part persists
    // its bit before returning, run 1 durably leaves EXACTLY 7 parts + a valid
    // .rcdl. Run 2 (all-correct, SAME server/port so the sidecar identity
    // matches) must re-fetch ONLY the 1 missing part.
    let total = 8_000usize;
    let chunk = 1000u64;
    let n_parts = 8;
    let last_offset = 7_000i64;
    let body = make_body(total);
    let server = RangeServer::new(body.clone());
    let url = spawn_range_server(server.clone()).await;

    let target = temp_target("resume");
    cleanup(&target);

    // --- Run 1: fault on the last part ---
    server.set_short_body_at(last_offset);
    let run1 = run_download_resolving_head(&url, &target, total as u64, chunk, 4).await;
    assert!(
        run1.is_err(),
        "run 1 must fail because the last part is short"
    );
    assert!(
        rcdl_path(&target).exists(),
        "run 1 must leave a .rcdl sidecar so run 2 can resume"
    );
    // The presized target survives at full length (positioned writes never shrink
    // it), which is what lets run 2 trust the persisted bits.
    let len_after_run1 = fs::metadata(&target).expect("stat target").len();
    assert_eq!(
        len_after_run1, total as u64,
        "target stays pre-sized to total"
    );

    // --- Run 2: all-correct; only the missing part should be fetched ---
    server.set_short_body_at(-1);
    server.reset_hits();
    let run2 = run_download_resolving_head(&url, &target, total as u64, chunk, 4).await;
    assert!(run2.is_ok(), "run 2 failed: {:?}", run2.err());

    let run2_hits = server.hits();
    assert!(
        run2_hits < n_parts,
        "resume must skip persisted parts: run2_hits={run2_hits} should be < {n_parts}"
    );
    assert_eq!(
        run2_hits, 1,
        "short-last-part strategy leaves exactly N-1 parts, so run 2 fetches exactly 1"
    );
    assert_byte_exact(&target, &body);
    assert!(
        !rcdl_path(&target).exists(),
        "the .rcdl must be deleted after the resumed download completes"
    );

    cleanup(&target);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_url_same_total_without_validator_never_mixes_remote_generations() {
    let total = 8_000usize;
    let chunk = 1000u64;
    let payload_a = make_body(total);
    let payload_b: Vec<u8> = payload_a.iter().map(|byte| byte.wrapping_add(17)).collect();
    let server = RangeServer::new(payload_a);
    let url = spawn_range_server(server.clone()).await;
    let target = temp_target("remote_generation_change");
    cleanup(&target);

    server.set_short_body_at(7_000);
    let run1 = run_download(&url, &target, total as u64, chunk, 4).await;
    assert!(run1.is_err());
    assert!(rcdl_path(&target).exists());

    server.replace_body_same_length(payload_b.clone());
    server.set_short_body_at(-1);
    server.reset_hits();
    let run2 = run_download(&url, &target, total as u64, chunk, 4).await;
    assert!(
        run2.is_ok(),
        "fresh generation download failed: {:?}",
        run2.err()
    );
    assert_eq!(
        server.hits(),
        8,
        "without a proven remote validator every part must be fetched again"
    );
    assert_byte_exact(&target, &payload_b);

    cleanup(&target);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serial_same_url_same_total_without_prepared_validator_refetches_new_generation() {
    let total = 8_000usize;
    let chunk = 1000u64;
    let payload_a = make_body(total);
    let payload_b: Vec<u8> = payload_a.iter().map(|byte| byte.wrapping_add(29)).collect();
    let server = RangeServer::new(payload_a);
    let url = spawn_range_server(server.clone()).await;
    let target = temp_target("serial_remote_generation_change");
    cleanup(&target);

    server.set_short_body_at(7_000);
    let run1 = run_download(&url, &target, total as u64, chunk, 1).await;
    assert!(run1.is_err(), "run 1 must leave an incomplete A generation");
    assert!(
        rcdl_path(&target).exists(),
        "run 1 must leave A sidecar state"
    );

    server.replace_body_same_length(payload_b.clone());
    server.set_etag("\"generation-b\"");
    server.set_short_body_at(-1);
    server.reset_hits();
    let run2 = run_download(&url, &target, total as u64, chunk, 1).await;
    assert!(
        run2.is_ok(),
        "new serial generation download failed: {:?}",
        run2.err()
    );
    assert_eq!(
        server.hits(),
        total / chunk as usize,
        "without a prepare-time validator, serial resume must not reuse A parts for generation B"
    );
    assert_byte_exact(&target, &payload_b);
    assert!(!rcdl_path(&target).exists());
    cleanup(&target);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn presigned_query_change_with_same_strong_etag_resumes_safely() {
    let total = 8_000usize;
    let chunk = 1000u64;
    let body = make_body(total);
    let server = RangeServer::new(body.clone());
    let base_url = spawn_range_server(server.clone()).await;
    let target = temp_target("query_refresh_same_etag");
    cleanup(&target);

    server.set_short_body_at(7_000);
    let first_url = format!("{base_url}?X-Amz-Signature=old");
    let run1 = run_download_resolving_head(&first_url, &target, total as u64, chunk, 4).await;
    assert!(run1.is_err());

    server.set_short_body_at(-1);
    server.reset_hits();
    let refreshed_url = format!("{base_url}?X-Amz-Signature=new");
    let run2 = run_download_resolving_head(&refreshed_url, &target, total as u64, chunk, 4).await;
    assert!(
        run2.is_ok(),
        "query refresh resume failed: {:?}",
        run2.err()
    );
    assert_eq!(server.hits(), 1);
    assert!(server.matching_if_match_hits() >= 1);
    assert_byte_exact(&target, &body);
    cleanup(&target);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn changed_strong_etag_invalidates_every_old_part() {
    let total = 8_000usize;
    let chunk = 1000u64;
    let payload_a = make_body(total);
    let payload_b: Vec<u8> = payload_a.iter().map(|byte| byte.wrapping_add(31)).collect();
    let server = RangeServer::new(payload_a);
    let url = spawn_range_server(server.clone()).await;
    let target = temp_target("strong_etag_change");
    cleanup(&target);

    server.set_short_body_at(7_000);
    assert!(
        run_download_resolving_head(&url, &target, total as u64, chunk, 4)
            .await
            .is_err()
    );
    server.replace_body_same_length(payload_b.clone());
    server.set_etag("\"generation-b\"");
    server.set_short_body_at(-1);
    server.reset_hits();

    let run2 = run_download_resolving_head(&url, &target, total as u64, chunk, 4).await;
    assert!(run2.is_ok(), "new ETag download failed: {:?}", run2.err());
    assert_eq!(server.hits(), 8);
    assert_byte_exact(&target, &payload_b);
    cleanup(&target);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn weak_etag_is_not_sufficient_for_cross_process_resume() {
    let total = 8_000usize;
    let chunk = 1000u64;
    let body = make_body(total);
    let server = RangeServer::new(body.clone());
    server.set_etag("W/\"weak-generation\"");
    let url = spawn_range_server(server.clone()).await;
    let target = temp_target("weak_etag");
    cleanup(&target);

    server.set_short_body_at(7_000);
    assert!(
        run_download_resolving_head(&url, &target, total as u64, chunk, 4)
            .await
            .is_err()
    );
    server.set_short_body_at(-1);
    server.reset_hits();
    let error = run_download_resolving_head(&url, &target, total as u64, chunk, 4)
        .await
        .expect_err("weak range ETags cannot prove one immutable generation");
    assert!(error.to_string().contains("strong ETag"));
    cleanup(&target);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn azure_front_door_unquoted_blob_etag_is_replayed_as_a_validator() {
    let total = 4_000usize;
    let chunk = 1_000u64;
    let body = make_body(total);
    let server = RangeServer::new(body.clone());
    server.set_etag("0x8DEFF4FCC6C92AC");
    server.set_azure_blob_headers(true);
    let url = spawn_range_server(server.clone()).await;
    let target = temp_target("azure_unquoted_etag");
    cleanup(&target);

    let outcome = run_download_resolving_head(&url, &target, total as u64, chunk, 4).await;
    assert!(
        outcome.is_ok(),
        "Azure Front Door's unquoted Blob ETag should remain a generation validator: {:?}",
        outcome.err()
    );
    assert_eq!(server.hits(), 4);
    assert_eq!(
        server.matching_if_match_hits(),
        4,
        "every range must replay the exact unquoted token in If-Match"
    );
    assert_byte_exact(&target, &body);
    cleanup(&target);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serial_range_merge_observes_prepared_strong_if_match() {
    let total = 4_000usize;
    let chunk = 1_000u64;
    let body = make_body(total);
    let server = RangeServer::new(body.clone());
    let url = spawn_range_server(server.clone()).await;
    let target = temp_target("serial_merge_sees_if_match");
    cleanup(&target);

    let observed = Arc::new(Mutex::new(Vec::new()));
    let protocol = Arc::new(InspectPreparedIfMatchDownload {
        // The one-part window below is what selects the serial executor. Keeping
        // this true proves both contract tests exercise the same wrapper.
        parallel: true,
        mutation: IfMatchMutation::Preserve,
        observed: observed.clone(),
        range_url_calls: Arc::new(AtomicUsize::new(0)),
    });
    let outcome = run_if_match_contract_download(&url, &target, chunk, 1, protocol).await;
    let observed = observed.lock().expect("If-Match observations").clone();
    let hits = server.hits();
    cleanup(&target);

    assert!(
        outcome.is_ok(),
        "serial contract fixture must complete: {:?}",
        outcome.err()
    );
    assert_eq!(hits, total / chunk as usize);
    assert_eq!(
        observed.len(),
        total / chunk as usize,
        "the provider hook must run once for every serial range"
    );
    assert!(
        observed
            .iter()
            .all(|value| value.as_deref() == Some("\"generation-a\"")),
        "every serial provider hook must see the exact strong ETag in If-Match before signing; observed {observed:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_range_merge_observes_prepared_strong_if_match() {
    let total = 4_000usize;
    let chunk = 1_000u64;
    let body = make_body(total);
    let server = RangeServer::new(body.clone());
    server.set_per_part_delay_ms(30);
    let url = spawn_range_server(server.clone()).await;
    let target = temp_target("parallel_merge_sees_if_match");
    cleanup(&target);

    let observed = Arc::new(Mutex::new(Vec::new()));
    let protocol = Arc::new(InspectPreparedIfMatchDownload {
        parallel: true,
        mutation: IfMatchMutation::Preserve,
        observed: observed.clone(),
        range_url_calls: Arc::new(AtomicUsize::new(0)),
    });
    let outcome = run_if_match_contract_download(&url, &target, chunk, 4, protocol).await;
    let observed = observed.lock().expect("If-Match observations").clone();
    let hits = server.hits();
    let peak = server.peak();
    cleanup(&target);

    assert!(
        outcome.is_ok(),
        "parallel contract fixture must complete: {:?}",
        outcome.err()
    );
    assert_eq!(hits, total / chunk as usize);
    assert!(peak > 1, "fixture must exercise the parallel range path");
    assert_eq!(
        observed.len(),
        total / chunk as usize,
        "the provider hook must run once for every parallel range"
    );
    assert!(
        observed
            .iter()
            .all(|value| value.as_deref() == Some("\"generation-a\"")),
        "every parallel provider hook must see the exact strong ETag in If-Match before signing; observed {observed:?}"
    );
}

async fn assert_provider_if_match_mutation_fails_before_range_io(
    case: &str,
    mutation: IfMatchMutation,
) {
    let total = 2_000usize;
    let chunk = 1_000u64;
    let server = RangeServer::new(make_body(total));
    let url = spawn_range_server(server.clone()).await;
    let target = temp_target(case);
    cleanup(&target);

    let observed = Arc::new(Mutex::new(Vec::new()));
    let range_url_calls = Arc::new(AtomicUsize::new(0));
    let protocol = Arc::new(InspectPreparedIfMatchDownload {
        parallel: false,
        mutation,
        observed: observed.clone(),
        range_url_calls: range_url_calls.clone(),
    });
    let outcome = run_if_match_contract_download(&url, &target, chunk, 1, protocol).await;
    let observed = observed.lock().expect("If-Match observations").clone();
    let hits = server.hits();
    cleanup(&target);

    let error = outcome
        .expect_err("a provider must not remove, replace, or append executor-owned If-Match");
    assert!(
        error.to_string().to_ascii_lowercase().contains("if-match"),
        "contract error must identify the modified If-Match header: {error}"
    );
    assert_eq!(
        observed,
        vec![Some("\"generation-a\"".to_string())],
        "the hook must receive the exact prepared validator before mutating it"
    );
    assert_eq!(
        hits, 0,
        "a modified executor-owned If-Match must fail before any range network I/O"
    );
    assert_eq!(
        range_url_calls.load(Ordering::SeqCst),
        1,
        "the provider URL must be resolved exactly once before the non-retryable contract failure"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provider_cannot_remove_prepared_if_match() {
    assert_provider_if_match_mutation_fails_before_range_io(
        "provider_removes_if_match",
        IfMatchMutation::Remove,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provider_cannot_replace_prepared_if_match() {
    assert_provider_if_match_mutation_fails_before_range_io(
        "provider_replaces_if_match",
        IfMatchMutation::Replace,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provider_cannot_append_prepared_if_match() {
    assert_provider_if_match_mutation_fails_before_range_io(
        "provider_appends_if_match",
        IfMatchMutation::Append,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn changed_unquoted_azure_etag_invalidates_every_old_part() {
    let total = 8_000usize;
    let chunk = 1_000u64;
    let payload_a = make_body(total);
    let payload_b: Vec<u8> = payload_a.iter().map(|byte| byte.wrapping_add(41)).collect();
    let server = RangeServer::new(payload_a);
    server.set_etag("0x8DEFF4FCC6C92AC");
    server.set_azure_blob_headers(true);
    let url = spawn_range_server(server.clone()).await;
    let target = temp_target("azure_unquoted_etag_change");
    cleanup(&target);

    server.set_short_body_at(7_000);
    assert!(
        run_download_resolving_head(&url, &target, total as u64, chunk, 4)
            .await
            .is_err()
    );
    server.replace_body_same_length(payload_b.clone());
    server.set_etag("0x8DEFF4FCC6C92AD");
    server.set_short_body_at(-1);
    server.reset_hits();

    let outcome = run_download_resolving_head(&url, &target, total as u64, chunk, 4).await;
    assert!(
        outcome.is_ok(),
        "new Azure ETag failed: {:?}",
        outcome.err()
    );
    assert_eq!(
        server.hits(),
        8,
        "changed ETag must invalidate every old part"
    );
    assert_byte_exact(&target, &payload_b);
    cleanup(&target);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mismatched_unquoted_azure_range_etag_is_rejected() {
    let total = 4_000usize;
    let chunk = 1_000u64;
    let server = RangeServer::new(make_body(total));
    server.set_etag("0x8DEFF4FCC6C92AC");
    server.set_range_etag_override(Some("0x8DEFF4FCC6C92AD"));
    server.set_azure_blob_headers(true);
    let url = spawn_range_server(server).await;
    let target = temp_target("azure_unquoted_etag_mismatch");
    cleanup(&target);

    let error = run_download_resolving_head(&url, &target, total as u64, chunk, 1)
        .await
        .expect_err("a different unquoted Azure range ETag must fail");
    assert!(error.to_string().contains("generation mismatch"));
    cleanup(&target);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_ranges_without_strong_etag_are_rejected() {
    let total = 4_000usize;
    let chunk = 1000u64;
    let server = RangeServer::new(make_body(total));
    server.set_omit_range_etag(true);
    let url = spawn_range_server(server).await;
    let target = temp_target("missing_range_etag");
    cleanup(&target);

    let error = run_download(&url, &target, total as u64, chunk, 4)
        .await
        .expect_err("parallel ranges without a validator must fail closed");
    assert!(error.to_string().contains("strong ETag"));
    cleanup(&target);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn single_part_parallel_download_without_strong_etag_remains_compatible() {
    // A one-part grid cannot mix bytes from multiple remote generations. Keep
    // that legacy-compatible case working even when max_parts opts into the
    // parallel driver and neither HEAD nor the sole 206 proves a strong ETag.
    let total = 777usize;
    let chunk = 1_000u64;
    let body = make_body(total);
    let server = RangeServer::new(body.clone());
    server.set_etag("W/\"weak-generation\"");
    server.set_omit_range_etag(true);
    let url = spawn_range_server(server.clone()).await;
    let target = temp_target("single_part_without_strong_etag");
    cleanup(&target);

    let outcome = run_download_resolving_head(&url, &target, total as u64, chunk, 4).await;
    assert!(
        outcome.is_ok(),
        "one range needs no cross-range generation validator: {:?}",
        outcome.err()
    );
    assert_eq!(
        server.hits(),
        1,
        "the object must use exactly one range GET"
    );
    assert_byte_exact(&target, &body);
    assert!(!rcdl_path(&target).exists());
    cleanup(&target);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminal_is_not_visible_until_same_target_lease_is_released() {
    let total = 2_000usize;
    let chunk = 1_000u64;
    let body = make_body(total);
    let server = RangeServer::new(body.clone());
    let url = spawn_range_server(server.clone()).await;
    let target = temp_target("terminal_lease_release");
    cleanup(&target);

    let client = MeowClient::new(
        MeowConfig::builder()
            .max_download_concurrency(1)
            .http_timeout(Duration::from_secs(30))
            .build()
            .expect("valid config"),
    );
    let task = || {
        DownloadPounceBuilder::new("lease.bin", &target, chunk, url.clone())
            .with_total_size(total as u64)
            .with_max_parts_in_flight(4)
            .build()
    };

    client
        .enqueue_and_wait(task(), |_record| {})
        .await
        .expect("first same-target download");

    // No yield or delay is allowed between observing terminal and the second
    // admission: the terminal boundary itself must guarantee that both the
    // actual-file lock and path lease have already been released.
    client
        .enqueue_and_wait(task(), |_record| {})
        .await
        .expect("immediate same-target re-enqueue must not see the old lease");

    client.close().await.expect("close client");
    assert_byte_exact(&target, &body);
    assert_eq!(server.hits(), 4, "both two-part runs must reach the server");
    cleanup(&target);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn canceled_callback_allows_immediate_cross_client_same_target_resume() {
    let total = 20_000usize;
    let chunk = 1_000u64;
    let body = make_body(total);
    let server = RangeServer::new(body.clone());
    server.set_per_part_delay_ms(100);
    let url = spawn_range_server(server.clone()).await;
    let target = temp_target("cancel_callback_lease_release");
    cleanup(&target);

    let client = MeowClient::new(
        MeowConfig::builder()
            .max_download_concurrency(1)
            .http_timeout(Duration::from_secs(30))
            .build()
            .expect("valid config"),
    );
    let (terminal_tx, terminal_rx) = tokio::sync::oneshot::channel();
    let terminal_tx = Arc::new(Mutex::new(Some(terminal_tx)));
    let callback_tx = Arc::clone(&terminal_tx);
    let task = DownloadPounceBuilder::new("cancel.bin", &target, chunk, url.clone())
        .with_total_size(total as u64)
        .with_max_parts_in_flight(4)
        .build();
    let task_id = client
        .try_enqueue(
            task,
            move |record| {
                if matches!(record.status(), TransferStatus::Canceled) {
                    if let Some(sender) = callback_tx.lock().expect("callback sender").take() {
                        let _ = sender.send(());
                    }
                }
            },
            |_, _| {},
        )
        .await
        .expect("enqueue cancellable download");

    for _ in 0..200 {
        if server.hits() > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(server.hits() > 0, "download must be active before cancel");
    client
        .cancel(task_id)
        .await
        .expect("cancel active download");
    tokio::time::timeout(Duration::from_secs(10), terminal_rx)
        .await
        .expect("Canceled callback timeout")
        .expect("Canceled callback sender dropped");

    // No yield or retry after observing Canceled: the callback itself is the
    // boundary proving checkpoint, actual-file lock and path lease are released.
    let resumed = run_download(&url, &target, total as u64, chunk, 4).await;
    assert!(
        resumed.is_ok(),
        "immediate cross-client same-target resume failed: {:?}",
        resumed.err()
    );

    client.close().await.expect("close first client");
    assert_byte_exact(&target, &body);
    cleanup(&target);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_paused_callback_fires_only_after_same_target_lease_release() {
    let total = 20_000usize;
    let chunk = 1_000u64;
    let body = make_body(total);
    let server = RangeServer::new(body.clone());
    server.set_per_part_delay_ms(100);
    let url = spawn_range_server(server.clone()).await;
    let target = temp_target("close_callback_lease_release");
    cleanup(&target);

    let client = Arc::new(MeowClient::new(
        MeowConfig::builder()
            .max_download_concurrency(1)
            .http_timeout(Duration::from_secs(30))
            .build()
            .expect("valid config"),
    ));
    let (paused_tx, paused_rx) = tokio::sync::oneshot::channel();
    let paused_tx = Arc::new(Mutex::new(Some(paused_tx)));
    let callback_tx = Arc::clone(&paused_tx);
    let task = DownloadPounceBuilder::new("close.bin", &target, chunk, url.clone())
        .with_total_size(total as u64)
        .with_max_parts_in_flight(4)
        .build();
    client
        .try_enqueue(
            task,
            move |record| {
                if matches!(record.status(), TransferStatus::Paused) {
                    if let Some(sender) = callback_tx.lock().expect("callback sender").take() {
                        let _ = sender.send(());
                    }
                }
            },
            |_, _| {},
        )
        .await
        .expect("enqueue close-drained download");

    for _ in 0..200 {
        if server.hits() > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(server.hits() > 0, "download must be active before close");
    let close_client = Arc::clone(&client);
    let close_task = tokio::spawn(async move { close_client.close().await });
    tokio::time::timeout(Duration::from_secs(10), paused_rx)
        .await
        .expect("Paused callback timeout")
        .expect("Paused callback sender dropped");

    // The Paused callback is emitted only after the close drain consumed the
    // worker cleanup acknowledgement, so a second client can take ownership
    // immediately even before the first close future has been joined here.
    let resumed = run_download(&url, &target, total as u64, chunk, 4).await;
    assert!(
        resumed.is_ok(),
        "same-target resume from Paused callback boundary failed: {:?}",
        resumed.err()
    );
    close_task
        .await
        .expect("close task join")
        .expect("close first client");

    assert_byte_exact(&target, &body);
    cleanup(&target);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn generation_switch_between_parallel_ranges_never_produces_a_mixed_file() {
    let total = 8_000usize;
    let chunk = 1000u64;
    let payload_a = make_body(total);
    let payload_b: Vec<u8> = payload_a.iter().map(|byte| byte.wrapping_add(73)).collect();
    let server = RangeServer::new(payload_a);
    server.switch_generation_from(4_000, payload_b, "\"generation-b\"");
    let url = spawn_range_server(server).await;
    let target = temp_target("in_run_generation_switch");
    cleanup(&target);

    let error = run_download(&url, &target, total as u64, chunk, 4)
        .await
        .expect_err("one download must not accept ranges from two generations");
    assert!(error.to_string().contains("generation mismatch"));
    cleanup(&target);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn custom_protocol_without_resume_identity_refetches_every_part_after_restart() {
    let total = 8_000usize;
    let chunk = 1_000u64;
    let body = make_body(total);
    let server = RangeServer::new(body.clone());
    server.set_short_body_at(7_000);
    let url = spawn_range_server(server.clone()).await;
    let target = temp_target("custom_protocol_fail_closed_resume");
    cleanup(&target);

    run_custom_protocol_download(&url, &target, chunk, 4)
        .await
        .expect_err("the injected final short range must leave a checkpoint");
    assert!(
        rcdl_path(&target).exists(),
        "failed run must retain sidecar"
    );

    server.set_short_body_at(-1);
    server.reset_hits();
    run_custom_protocol_download(&url, &target, chunk, 4)
        .await
        .expect("custom protocol restart must safely download from zero");

    assert_eq!(
        server.hits(),
        total / chunk as usize,
        "a custom protocol without stable identity must not trust prior-process parts"
    );
    assert_byte_exact(&target, &body);
    cleanup(&target);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn injected_http_client_refetches_every_part_after_restart() {
    let total = 8_000usize;
    let chunk = 1_000u64;
    let body = make_body(total);
    let server = RangeServer::new(body.clone());
    server.set_short_body_at(7_000);
    let url = spawn_range_server(server.clone()).await;
    let target = temp_target("injected_client_fail_closed_resume");
    cleanup(&target);

    run_injected_client_download(&url, &target, chunk, 4)
        .await
        .expect_err("the injected final short range must leave a checkpoint");
    assert!(
        rcdl_path(&target).exists(),
        "failed run must retain sidecar"
    );

    server.set_short_body_at(-1);
    server.reset_hits();
    run_injected_client_download(&url, &target, chunk, 4)
        .await
        .expect("injected HTTP client restart must safely download from zero");

    assert_eq!(
        server.hits(),
        total / chunk as usize,
        "hidden reqwest default headers must prevent prior-process part reuse"
    );
    assert_byte_exact(&target, &body);
    cleanup(&target);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn generation_switch_between_serial_ranges_fails_without_completing() {
    let total = 8_000usize;
    let chunk = 1000u64;
    let payload_a = make_body(total);
    let payload_b: Vec<u8> = payload_a.iter().map(|byte| byte.wrapping_add(91)).collect();
    let server = RangeServer::new(payload_a);
    server.switch_generation_from(4_000, payload_b, "\"generation-b\"");
    let url = spawn_range_server(server).await;
    let target = temp_target("serial_in_run_generation_switch");
    cleanup(&target);

    let error = run_download(&url, &target, total as u64, chunk, 1)
        .await
        .expect_err("one serial download must not accept ranges from two generations");
    assert!(error.to_string().contains("generation mismatch"));
    assert!(
        fs::metadata(&target)
            .expect("stat failed serial target")
            .len()
            < total as u64,
        "a generation mismatch must stop before the mixed file reaches complete length"
    );
    assert!(
        rcdl_path(&target).exists(),
        "failed serial generation validation must retain resumable sidecar state"
    );
    cleanup(&target);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn semantic_query_change_invalidates_resume_even_with_same_etag() {
    let total = 8_000usize;
    let chunk = 1000u64;
    let body = make_body(total);
    let server = RangeServer::new(body.clone());
    let base_url = spawn_range_server(server.clone()).await;
    let target = temp_target("semantic_query_change");
    cleanup(&target);

    server.set_short_body_at(7_000);
    assert!(run_download_resolving_head(
        &format!("{base_url}?versionId=v1"),
        &target,
        total as u64,
        chunk,
        4,
    )
    .await
    .is_err());

    server.set_short_body_at(-1);
    server.reset_hits();
    let outcome = run_download_resolving_head(
        &format!("{base_url}?versionId=v2"),
        &target,
        total as u64,
        chunk,
        4,
    )
    .await;
    assert!(
        outcome.is_ok(),
        "semantic URL refresh failed: {:?}",
        outcome.err()
    );
    assert_eq!(
        server.hits(),
        8,
        "a different object version must not reuse parts"
    );
    assert_byte_exact(&target, &body);
    cleanup(&target);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn range_response_from_another_etag_is_rejected() {
    let total = 4_000usize;
    let chunk = 1000u64;
    let server = RangeServer::new(make_body(total));
    server.set_range_etag_override(Some("\"generation-b\""));
    let url = spawn_range_server(server).await;
    let target = temp_target("range_generation_mismatch");
    cleanup(&target);

    let err = run_download_resolving_head(&url, &target, total as u64, chunk, 4)
        .await
        .expect_err("GET ETag differing from HEAD must fail");
    assert!(err.to_string().contains("generation mismatch"));
    cleanup(&target);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serial_range_response_from_another_etag_is_rejected() {
    let total = 4_000usize;
    let chunk = 1000u64;
    let server = RangeServer::new(make_body(total));
    // The test server intentionally records but does not enforce If-Match,
    // modeling a broken origin/proxy. The client must still compare the 206 ETag.
    server.set_range_etag_override(Some("\"generation-b\""));
    let url = spawn_range_server(server).await;
    let target = temp_target("serial_range_generation_mismatch");
    cleanup(&target);

    let err = run_download_resolving_head(&url, &target, total as u64, chunk, 1)
        .await
        .expect_err("serial GET ETag differing from HEAD must fail");
    assert!(err.to_string().contains("generation mismatch"));
    cleanup(&target);
}

// ---------------------------------------------------------------------------
// Scenario 4 — resume INTEGRITY: a deleted target discards the stale sidecar
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_integrity_deleted_target_refetches_all() {
    // Guards the load-before-presize fix. Run 1 leaves a valid .rcdl (7 bits) +
    // a presized file. Then DELETE the target (keep the .rcdl). Run 2 must see
    // the target absent, DISCARD the stale sidecar, and re-fetch ALL 8 parts,
    // yielding a byte-exact file. A regression to presize-before-load would honor
    // the stale bits, fetch < N, and leave a partially-zero (corrupt) file.
    let total = 8_000usize;
    let chunk = 1000u64;
    let n_parts = 8;
    let body = make_body(total);
    let server = RangeServer::new(body.clone());
    let url = spawn_range_server(server.clone()).await;

    let target = temp_target("integrity_delete");
    cleanup(&target);

    // --- Run 1: leave partial + sidecar ---
    server.set_short_body_at(7_000);
    let run1 = run_download(&url, &target, total as u64, chunk, 4).await;
    assert!(run1.is_err(), "run 1 must fail");
    assert!(rcdl_path(&target).exists(), "run 1 leaves a .rcdl");

    // Delete the target output file but LEAVE the hidden .rcdl.
    fs::remove_file(&target).expect("delete target");
    assert!(
        rcdl_path(&target).exists(),
        "the stale .rcdl must remain after deleting the target"
    );

    // --- Run 2: all-correct; must refetch every part ---
    server.set_short_body_at(-1);
    server.reset_hits();
    let run2 = run_download(&url, &target, total as u64, chunk, 4).await;
    assert!(run2.is_ok(), "run 2 failed: {:?}", run2.err());

    assert_eq!(
        server.hits(),
        n_parts,
        "a deleted target must discard the stale sidecar and re-fetch ALL parts"
    );
    assert_byte_exact(&target, &body);
    assert!(
        !rcdl_path(&target).exists(),
        "the .rcdl is deleted on success"
    );

    cleanup(&target);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_integrity_truncated_target_refetches_all() {
    // Same integrity guard, but the target is TRUNCATED to a non-total length
    // instead of deleted. The sidecar's length guard (`target_len != total`) must
    // fire and force a full re-fetch.
    let total = 8_000usize;
    let chunk = 1000u64;
    let n_parts = 8;
    let body = make_body(total);
    let server = RangeServer::new(body.clone());
    let url = spawn_range_server(server.clone()).await;

    let target = temp_target("integrity_truncate");
    cleanup(&target);

    // --- Run 1: leave partial + sidecar ---
    server.set_short_body_at(7_000);
    let run1 = run_download(&url, &target, total as u64, chunk, 4).await;
    assert!(run1.is_err(), "run 1 must fail");
    assert!(rcdl_path(&target).exists(), "run 1 leaves a .rcdl");

    // Truncate the target to a non-total length (keep the .rcdl).
    fs::write(&target, vec![0u8; 5]).expect("truncate target");
    assert_eq!(fs::metadata(&target).unwrap().len(), 5);

    // --- Run 2: all-correct; must refetch every part ---
    server.set_short_body_at(-1);
    server.reset_hits();
    let run2 = run_download(&url, &target, total as u64, chunk, 4).await;
    assert!(run2.is_ok(), "run 2 failed: {:?}", run2.err());

    assert_eq!(
        server.hits(),
        n_parts,
        "a truncated target must discard the stale sidecar and re-fetch ALL parts"
    );
    assert_byte_exact(&target, &body);
    assert!(
        !rcdl_path(&target).exists(),
        "the .rcdl is deleted on success"
    );

    cleanup(&target);
}

// ---------------------------------------------------------------------------
// Scenario 5 — a 200 OK to a Range request fails the concurrent download
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_hundred_response_fails_concurrent_download() {
    let total = 8_000usize;
    let chunk = 1000u64;
    let body = make_body(total);
    let server = RangeServer::new(body);
    server.set_force_200(true);
    let url = spawn_range_server(server.clone()).await;

    let target = temp_target("http200");
    cleanup(&target);

    let outcome = run_download(&url, &target, total as u64, chunk, 4).await;
    let err = outcome.expect_err("a 200 to a Range request must fail the download");
    assert!(
        err.to_string().contains("200"),
        "error must mention the offending 200 status, got: {err}"
    );

    cleanup(&target);
}

// ---------------------------------------------------------------------------
// Scenario 6 — serial path is byte-exact and removes its sidecar on success
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serial_download_byte_exact_and_cleans_sidecar() {
    let total = 8_000usize;
    let chunk = 1000u64;
    let n_parts = 8;
    let body = make_body(total);
    let server = RangeServer::new(body.clone());
    let url = spawn_range_server(server.clone()).await;

    let target = temp_target("serial");
    cleanup(&target);

    // max_parts_in_flight == 1 forces the legacy serial path.
    let outcome = run_download(&url, &target, total as u64, chunk, 1).await;
    assert!(
        outcome.is_ok(),
        "serial download failed: {:?}",
        outcome.err()
    );

    assert_byte_exact(&target, &body);
    assert!(
        !rcdl_path(&target).exists(),
        "successful final validation must delete the serial .rcdl sidecar"
    );
    assert_eq!(
        server.hits(),
        n_parts,
        "serial fetches each part once, in order"
    );
    assert_eq!(
        server.peak(),
        1,
        "the serial path must issue one request at a time"
    );

    cleanup(&target);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serial_without_sidecar_refetches_a_same_length_unknown_local_file() {
    let total = 8_000usize;
    let chunk = 1000u64;
    let n_parts = total / chunk as usize;
    let body = make_body(total);
    let unknown_local = vec![0xa5; total];
    assert_ne!(
        unknown_local, body,
        "fixture must differ from the remote body"
    );
    let server = RangeServer::new(body.clone());
    let url = spawn_range_server(server.clone()).await;

    let target = temp_target("serial_unknown_full_without_sidecar");
    cleanup(&target);
    fs::write(&target, unknown_local).expect("write unknown full-length local file");
    assert!(!rcdl_path(&target).exists(), "fixture must have no sidecar");

    let outcome = run_download(&url, &target, total as u64, chunk, 1).await;
    assert!(
        outcome.is_ok(),
        "serial replacement download failed: {:?}",
        outcome.err()
    );
    assert_eq!(
        server.hits(),
        n_parts,
        "unknown local bytes have no remote-generation proof and must be fetched from offset 0"
    );
    assert_byte_exact(&target, &body);
    assert!(!rcdl_path(&target).exists());
    cleanup(&target);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serial_without_sidecar_refetches_an_unknown_partial_file_from_zero() {
    let total = 8_000usize;
    let chunk = 1000u64;
    let n_parts = total / chunk as usize;
    let partial_len = 2_333usize;
    let body = make_body(total);
    let unknown_partial = vec![0x5a; partial_len];
    assert_ne!(
        unknown_partial,
        body[..partial_len],
        "fixture prefix must differ from the remote body"
    );
    let server = RangeServer::new(body.clone());
    let url = spawn_range_server(server.clone()).await;

    let target = temp_target("serial_unknown_partial_without_sidecar");
    cleanup(&target);
    fs::write(&target, unknown_partial).expect("write unknown partial local file");
    assert!(!rcdl_path(&target).exists(), "fixture must have no sidecar");

    let outcome = run_download(&url, &target, total as u64, chunk, 1).await;
    assert!(
        outcome.is_ok(),
        "serial partial replacement download failed: {:?}",
        outcome.err()
    );
    assert_eq!(
        server.hits(),
        n_parts,
        "a length-only partial file is untrusted and must not skip its prefix ranges"
    );
    assert_byte_exact(&target, &body);
    assert!(!rcdl_path(&target).exists());
    cleanup(&target);
}

// ---------------------------------------------------------------------------
// Scenario 7 — a corrupt/stale sidecar is ignored and safely replaced
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serial_download_ignores_corrupt_sidecar_and_refetches_all() {
    let total = 8_000usize;
    let chunk = 1000u64;
    let body = make_body(total);
    let server = RangeServer::new(body);
    let url = spawn_range_server(server.clone()).await;

    let target = temp_target("guard");
    cleanup(&target);
    // Touch an empty/corrupt sidecar. It contains no trustworthy part digest.
    initialize_rcdl_namespace(&target);
    fs::write(rcdl_path(&target), b"").expect("touch .rcdl");

    let outcome = run_download(&url, &target, total as u64, chunk, 1).await;
    assert!(
        outcome.is_ok(),
        "serial recovery failed: {:?}",
        outcome.err()
    );
    assert_byte_exact(&target, &make_body(total));
    assert_eq!(server.hits(), total / chunk as usize);
    assert!(
        !rcdl_path(&target).exists(),
        "the replacement sidecar is deleted after verified completion"
    );

    cleanup(&target);
}

// ---------------------------------------------------------------------------
// Scenario 8 — serial failure persists completed-part digests for resume
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serial_failure_resumes_only_the_missing_part() {
    let total = 8_000usize;
    let chunk = 1000u64;
    let body = make_body(total);
    let server = RangeServer::new(body.clone());
    let url = spawn_range_server(server.clone()).await;

    let target = temp_target("serial_resume");
    cleanup(&target);
    server.set_short_body_at(7_000);
    let first = run_download_resolving_head(&url, &target, total as u64, chunk, 1).await;
    assert!(
        first.is_err(),
        "the injected final-part short read must fail"
    );
    assert!(
        rcdl_path(&target).exists(),
        "seven completed serial part digests must be durable on failure"
    );

    server.set_short_body_at(-1);
    server.reset_hits();
    let resumed = run_download_resolving_head(&url, &target, total as u64, chunk, 1).await;
    assert!(resumed.is_ok(), "serial resume failed: {:?}", resumed.err());
    assert_eq!(server.hits(), 1, "only the missing final part is fetched");
    assert_byte_exact(&target, &body);
    assert!(!rcdl_path(&target).exists());
    cleanup(&target);
}
