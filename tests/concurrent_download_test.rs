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
//!   6. the serial (default) path stays byte-exact and never writes a `.rcdl`;
//!   7. the cross-mode guard: a stray `.rcdl` blocks a serial re-download.
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
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use rusty_cat::api::{
    DownloadPounceBuilder, InnerErrorCode, MeowClient, MeowConfig, MeowError, TaskOutcome,
};

// ---------------------------------------------------------------------------
// Mock HTTP/1.1 range server
// ---------------------------------------------------------------------------

/// A single long-lived range server whose behavior is fully controlled by
/// atomics, so one instance can serve a "run 1" (inject a fault) and a "run 2"
/// (all-correct) on the SAME port. The port matters: the `.rcdl` sidecar is
/// bound to `FNV(range_url)` (which includes the port), so a resume MUST hit the
/// same URL or the sidecar is treated as stale.
struct RangeServer {
    /// Exact source payload; every 206 slices out of this.
    body: Vec<u8>,
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
            body,
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

    // Parse `Range: bytes=a-b` (case-insensitive header name).
    let range = req.lines().find_map(|l| {
        let l = l.trim();
        let low = l.to_ascii_lowercase();
        low.strip_prefix("range:").map(|_| l["range:".len()..].trim().to_string())
    });

    let total = server.body.len();

    if server.force_200.load(Ordering::SeqCst) {
        // Server ignores Range and streams the whole object with a 200.
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n"
        );
        let _ = sock.write_all(head.as_bytes()).await;
        let _ = sock.write_all(&server.body).await;
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

    server.hits.fetch_add(1, Ordering::SeqCst);
    let cur = server.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    server.peak.fetch_max(cur, Ordering::SeqCst);

    let spec = r.trim_start_matches("bytes=");
    let (a_s, b_s) = spec.split_once('-').unwrap_or(("0", "0"));
    let a: usize = a_s.trim().parse().unwrap_or(0);
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

    let slice = &server.body[a..=end];
    let short = server.short_body_at.load(Ordering::SeqCst) == a as i64;
    // Full Content-Range (so the SDK's expected length is the FULL part) but a
    // body one byte shorter → the SDK detects "range body short" (InvalidRange).
    let send: &[u8] = if short {
        &slice[..slice.len().saturating_sub(1)]
    } else {
        slice
    };

    let head = format!(
        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {a}-{end}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    p.push(format!(
        "rusty_cat_cdl_{case}_{}_{ts}.bin",
        std::process::id()
    ));
    p
}

/// Sidecar path mirrors `download_progress::sidecar_path` (`<target>.rcdl`), which
/// is `pub(crate)` and thus not importable from this external test crate.
fn rcdl_path(target: &Path) -> PathBuf {
    let mut s = target.as_os_str().to_os_string();
    s.push(".rcdl");
    PathBuf::from(s)
}

fn cleanup(target: &Path) {
    let _ = fs::remove_file(target);
    let _ = fs::remove_file(rcdl_path(target));
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

    let task = DownloadPounceBuilder::new(file_name, target, chunk, url.to_string())
        .with_total_size(total)
        .with_max_parts_in_flight(max_parts)
        .build();

    let result = client.enqueue_and_wait(task, |_record| {}).await;
    client.close().await.expect("close client");
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
    let run1 = run_download(&url, &target, total as u64, chunk, 4).await;
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
    assert_eq!(len_after_run1, total as u64, "target stays pre-sized to total");

    // --- Run 2: all-correct; only the missing part should be fetched ---
    server.set_short_body_at(-1);
    server.reset_hits();
    let run2 = run_download(&url, &target, total as u64, chunk, 4).await;
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
    assert!(!rcdl_path(&target).exists(), "the .rcdl is deleted on success");

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
    assert!(!rcdl_path(&target).exists(), "the .rcdl is deleted on success");

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
// Scenario 6 — serial (legacy) path is byte-exact and writes NO sidecar
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serial_download_byte_exact_no_sidecar() {
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
    assert!(outcome.is_ok(), "serial download failed: {:?}", outcome.err());

    assert_byte_exact(&target, &body);
    assert!(
        !rcdl_path(&target).exists(),
        "the serial path must never create a .rcdl sidecar"
    );
    assert_eq!(server.hits(), n_parts, "serial fetches each part once, in order");
    assert_eq!(server.peak(), 1, "the serial path must issue one request at a time");

    cleanup(&target);
}

// ---------------------------------------------------------------------------
// Scenario 7 — cross-mode guard: a stray .rcdl blocks a serial re-download
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serial_download_blocked_by_existing_sidecar() {
    let total = 8_000usize;
    let chunk = 1000u64;
    let body = make_body(total);
    let server = RangeServer::new(body);
    let url = spawn_range_server(server.clone()).await;

    let target = temp_target("guard");
    cleanup(&target);
    // Touch an empty sidecar (the guard only checks existence, not content).
    fs::write(rcdl_path(&target), b"").expect("touch .rcdl");

    // Default max_parts_in_flight (== 1) => serial path, which refuses to run
    // while a parallel-download sidecar is present.
    let outcome = run_download(&url, &target, total as u64, chunk, 1).await;
    let err = outcome.expect_err("a stray .rcdl must block the serial path");
    assert_eq!(
        err.code(),
        InnerErrorCode::InvalidTaskState as i32,
        "cross-mode guard must surface InvalidTaskState, got code {}",
        err.code()
    );
    assert!(
        err.to_string().contains("max_parts_in_flight > 1"),
        "guard error must point the caller to max_parts_in_flight > 1, got: {err}"
    );

    cleanup(&target);
}
