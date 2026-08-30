//! Integration tests for optimization ④ — opt-in intra-file parallel parts.
//!
//! These drive the real executor (`MeowClient::try_enqueue` -> scheduler ->
//! `run_group_parallel`) through a recording mock upload protocol, verifying the
//! load-bearing invariants without any network:
//!   - out-of-order part completion still yields a contiguous, byte-correct file;
//!   - `complete` fires exactly once, only after every part finished;
//!   - the in-flight window is bounded by `max_parts_in_flight`;
//!   - a protocol that is NOT parallel-safe, or `max_parts_in_flight == 1`, stays
//!     strictly serial even when a large `max_parts_in_flight` is requested.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use rusty_cat::http_breakpoint::UploadResumeInfo;
use rusty_cat::upload_trait::{UploadChunkCtx, UploadPrepareCtx};
use rusty_cat::{
    BreakpointUpload, MeowClient, MeowConfig, MeowError, TransferStatus, UploadPounceBuilder,
};

fn temp_upload_path(case: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    p.push(format!("rusty_cat_parallel_parts_{case}_{ts}.bin"));
    p
}

#[derive(Default, Clone)]
struct Recorder {
    /// offset -> the exact bytes the protocol received for that chunk.
    received: BTreeMap<u64, Vec<u8>>,
    /// Concurrent `upload_chunk` calls in flight right now.
    in_flight: usize,
    /// Maximum concurrency ever observed (1 == serial).
    peak_in_flight: usize,
    /// Number of `complete_upload` invocations (must be exactly 1).
    complete_calls: usize,
    /// Number of provider-side upload aborts.
    abort_calls: usize,
}

/// Mock upload protocol that records what the executor sends it. Optionally
/// advertises out-of-order safety and scrambles completion order so the highest
/// offsets finish first (stressing the contiguous-watermark logic).
struct RecordingUpload {
    rec: Arc<Mutex<Recorder>>,
    parallel: bool,
    total: u64,
    chunk: u64,
    scramble: bool,
    /// Per-chunk base delay (ms); scramble mode scales it by the inverse offset
    /// so higher offsets finish first.
    chunk_delay_ms: u64,
    /// If set, the part whose start offset equals this value panics instead of
    /// uploading, exercising the JoinSet `JoinError` path (MISSED-F).
    panic_at_offset: Option<u64>,
}

#[async_trait]
impl BreakpointUpload for RecordingUpload {
    async fn prepare(&self, _ctx: UploadPrepareCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(0),
            provider_upload_id: None,
        })
    }

    async fn upload_chunk(&self, ctx: UploadChunkCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        let offset = ctx.offset;
        if self.panic_at_offset == Some(offset) {
            // Simulate a part task crashing mid-flight. The driver must treat the
            // resulting JoinError as a hard failure: cancel siblings, drain, emit
            // exactly one Failed, and NEVER finalize a torn object.
            panic!("injected upload part panic at offset {offset}");
        }
        let bytes = ctx.chunk.to_vec();
        {
            let mut r = self.rec.lock().expect("rec lock");
            r.in_flight += 1;
            r.peak_in_flight = r.peak_in_flight.max(r.in_flight);
        }
        // Never hold the std mutex across the await.
        let delay_ms = if self.scramble {
            // Higher offsets finish first: delay inversely to offset.
            (self.total.saturating_sub(offset) / self.chunk.max(1)) * self.chunk_delay_ms
        } else {
            self.chunk_delay_ms
        };
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        {
            let mut r = self.rec.lock().expect("rec lock");
            r.received.insert(offset, bytes);
            r.in_flight -= 1;
        }
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(offset + ctx.chunk.len() as u64),
            provider_upload_id: None,
        })
    }

    async fn complete_upload(
        &self,
        _client: &reqwest::Client,
        _task: &rusty_cat::TransferTask,
    ) -> Result<Option<String>, MeowError> {
        self.rec.lock().expect("rec lock").complete_calls += 1;
        Ok(Some("done".to_string()))
    }

    async fn abort_upload(
        &self,
        _client: &reqwest::Client,
        _task: &rusty_cat::TransferTask,
    ) -> Result<(), MeowError> {
        self.rec.lock().expect("rec lock").abort_calls += 1;
        Ok(())
    }

    fn supports_parallel_parts(&self) -> bool {
        self.parallel
    }
}

/// Runs one upload to terminal status and returns the recorder snapshot.
async fn run_upload(
    case: &str,
    payload: &[u8],
    chunk: u64,
    max_parts: usize,
    parallel: bool,
    scramble: bool,
) -> Recorder {
    let path = temp_upload_path(case);
    fs::write(&path, payload).expect("write upload fixture");

    let rec = Arc::new(Mutex::new(Recorder::default()));
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(1)
            .build()
            .expect("valid config"),
    );
    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();

    let task = UploadPounceBuilder::new("p.bin", &path, chunk)
        .with_url("https://placeholder/upload")
        .with_breakpoint_upload(Arc::new(RecordingUpload {
            rec: rec.clone(),
            parallel,
            total: payload.len() as u64,
            chunk,
            scramble,
            chunk_delay_ms: 5,
            panic_at_offset: None,
        }))
        .with_max_parts_in_flight(max_parts)
        .build()
        .expect("build upload task");

    client
        .try_enqueue(
            task,
            move |record| {
                statuses_cb
                    .lock()
                    .expect("lock statuses")
                    .push(record.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("enqueue upload task");

    let mut terminal = None;
    for _ in 0..500 {
        let snapshot = statuses.lock().expect("lock statuses").clone();
        terminal = snapshot
            .iter()
            .rev()
            .find(|s| {
                matches!(
                    s,
                    TransferStatus::Complete | TransferStatus::Failed(_) | TransferStatus::Canceled
                )
            })
            .cloned();
        if terminal.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    client.close().await.expect("close client");
    let _ = fs::remove_file(&path);

    assert!(
        matches!(terminal, Some(TransferStatus::Complete)),
        "expected Complete terminal status, got {terminal:?}"
    );
    let snapshot = rec.lock().expect("rec lock").clone();
    snapshot
}

/// Sorted reassembly of the recorded chunks; the offsets and bytes together
/// prove a contiguous, gap-free, byte-correct file.
fn reassemble(rec: &Recorder) -> (Vec<u64>, Vec<u8>) {
    let offsets: Vec<u64> = rec.received.keys().copied().collect();
    let mut bytes = Vec::new();
    for chunk in rec.received.values() {
        bytes.extend_from_slice(chunk);
    }
    (offsets, bytes)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_out_of_order_uploads_all_bytes_contiguously_and_completes_once() {
    // 6 chunks of 1000 bytes, completion order scrambled (highest offset first).
    let payload: Vec<u8> = (0..6000u32).map(|i| (i % 251) as u8).collect();
    let rec = run_upload("ooo", &payload, 1000, 8, true, true).await;

    let (offsets, reassembled) = reassemble(&rec);
    assert_eq!(
        offsets,
        vec![0, 1000, 2000, 3000, 4000, 5000],
        "every chunk-aligned offset must be present exactly once (no gap, no dup)"
    );
    assert_eq!(
        reassembled, payload,
        "reassembled bytes must equal the source despite concurrent out-of-order reads"
    );
    assert_eq!(
        rec.complete_calls, 1,
        "complete must fire exactly once, only after all parts finished"
    );
    assert!(
        rec.peak_in_flight >= 2,
        "expected genuine fan-out, observed peak in-flight = {}",
        rec.peak_in_flight
    );
    assert!(
        rec.peak_in_flight <= 8,
        "in-flight must never exceed max_parts_in_flight"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_respects_max_parts_in_flight_window() {
    // 10 chunks, window bounded to 3.
    let payload: Vec<u8> = (0..10_000u32).map(|i| (i % 97) as u8).collect();
    let rec = run_upload("window", &payload, 1000, 3, true, false).await;

    let (offsets, reassembled) = reassemble(&rec);
    assert_eq!(offsets.len(), 10, "all 10 chunks must arrive");
    assert_eq!(reassembled, payload);
    assert_eq!(rec.complete_calls, 1);
    assert!(
        rec.peak_in_flight >= 2,
        "expected concurrency, peak = {}",
        rec.peak_in_flight
    );
    assert!(
        rec.peak_in_flight <= 3,
        "the window must bound in-flight parts to max_parts_in_flight=3, observed {}",
        rec.peak_in_flight
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsafe_effective_part_window_fails_before_dispatching_parts() {
    let payload = vec![b'x'; 257];
    let path = temp_upload_path("resource_limit");
    fs::write(&path, &payload).expect("write upload fixture");
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let client = MeowClient::new(MeowConfig::default());
    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();
    let task = UploadPounceBuilder::new("p.bin", &path, 1)
        .with_url("https://placeholder/upload")
        .with_breakpoint_upload(Arc::new(RecordingUpload {
            rec: rec.clone(),
            parallel: true,
            total: payload.len() as u64,
            chunk: 1,
            scramble: false,
            chunk_delay_ms: 0,
            panic_at_offset: None,
        }))
        .with_max_parts_in_flight(257)
        .build()
        .expect("build upload task");
    client
        .try_enqueue(
            task,
            move |record| {
                statuses_cb
                    .lock()
                    .expect("lock statuses")
                    .push(record.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("enqueue upload task");

    let terminal = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(status) = statuses
                .lock()
                .expect("lock statuses")
                .iter()
                .find(|status| matches!(status, TransferStatus::Failed(_)))
                .cloned()
            {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("resource failure must not hang");

    let TransferStatus::Failed(error) = terminal else {
        unreachable!("filtered to Failed")
    };
    assert!(error.to_string().contains("part window exceeds task limit"));
    let snapshot = rec.lock().expect("rec lock").clone();
    assert!(snapshot.received.is_empty());
    assert_eq!(snapshot.complete_calls, 0);
    client.close().await.expect("close client");
    let _ = fs::remove_file(path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn incapable_protocol_stays_serial_even_with_high_max_parts() {
    // Protocol advertises supports_parallel_parts() == false. Even with
    // max_parts_in_flight=257 the executor MUST keep it strictly serial — a
    // refactor that accidentally enabled it would silently corrupt order.
    let payload: Vec<u8> = (0..6000u32).map(|i| (i % 251) as u8).collect();
    let rec = run_upload("incapable", &payload, 1000, 257, false, false).await;

    let (offsets, reassembled) = reassemble(&rec);
    assert_eq!(offsets, vec![0, 1000, 2000, 3000, 4000, 5000]);
    assert_eq!(reassembled, payload);
    assert_eq!(
        rec.peak_in_flight, 1,
        "a protocol without parallel-parts capability must stay serial"
    );
    assert_eq!(rec.complete_calls, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn max_parts_one_stays_serial_for_capable_protocol() {
    // Default max_parts_in_flight == 1 must take the legacy serial path even for
    // an out-of-order-safe protocol.
    let payload: Vec<u8> = (0..6000u32).map(|i| (i % 251) as u8).collect();
    let rec = run_upload("serial1", &payload, 1000, 1, true, false).await;

    assert_eq!(
        rec.peak_in_flight, 1,
        "max_parts_in_flight=1 must take the serial path"
    );
    assert_eq!(rec.complete_calls, 1);
    let (_offsets, reassembled) = reassemble(&rec);
    assert_eq!(reassembled, payload);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_mid_flight_terminates_without_completing() {
    // Many slow chunks with a small window so the upload is still running when we
    // cancel. The drive loop must drain all in-flight parts and emit exactly one
    // Canceled WITHOUT ever calling complete (a half-finished object must not be
    // committed), and must not hang.
    let payload: Vec<u8> = (0..12_000u32).map(|i| (i % 251) as u8).collect();
    let path = temp_upload_path("cancel");
    fs::write(&path, &payload).expect("write upload fixture");

    let rec = Arc::new(Mutex::new(Recorder::default()));
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(1)
            .build()
            .expect("valid config"),
    );
    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();

    let task = UploadPounceBuilder::new("p.bin", &path, 1000)
        .with_url("https://placeholder/upload")
        .with_breakpoint_upload(Arc::new(RecordingUpload {
            rec: rec.clone(),
            parallel: true,
            total: payload.len() as u64,
            chunk: 1000,
            scramble: false,
            chunk_delay_ms: 60, // each of 12 chunks takes ~60ms
            panic_at_offset: None,
        }))
        .with_max_parts_in_flight(3)
        .build()
        .expect("build upload task");

    let task_id = client
        .try_enqueue(
            task,
            move |record| {
                statuses_cb
                    .lock()
                    .expect("lock statuses")
                    .push(record.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("enqueue upload task");

    // Let a couple of parts start, then cancel while the upload is mid-flight.
    tokio::time::sleep(Duration::from_millis(40)).await;
    client.cancel(task_id).await.expect("cancel task");

    let mut terminal = None;
    for _ in 0..500 {
        let snapshot = statuses.lock().expect("lock statuses").clone();
        terminal = snapshot
            .iter()
            .rev()
            .find(|s| {
                matches!(
                    s,
                    TransferStatus::Complete | TransferStatus::Failed(_) | TransferStatus::Canceled
                )
            })
            .cloned();
        if terminal.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    client.close().await.expect("close client");
    let _ = fs::remove_file(&path);

    let snapshot = rec.lock().expect("rec lock").clone();
    assert!(
        matches!(terminal, Some(TransferStatus::Canceled)),
        "cancel mid-flight must terminate as Canceled, got {terminal:?}"
    );
    assert_eq!(
        snapshot.complete_calls, 0,
        "complete must NOT be called when the upload was cancelled mid-flight"
    );
    assert!(
        snapshot.received.len() < 12,
        "cancel should have stopped the upload before all 12 chunks were sent, sent {}",
        snapshot.received.len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panicked_part_fails_upload_without_completing() {
    // A part task that panics mid-flight must not be silently swallowed: the
    // driver's JoinSet `JoinError` arm (MISSED-F) has to cancel the siblings,
    // drain to quiescence, emit exactly one Failed, and NEVER call complete — a
    // panicked part's bytes are unverified, so finalizing would commit a torn
    // object. It must also not hang.
    let payload: Vec<u8> = (0..6000u32).map(|i| (i % 251) as u8).collect();
    let path = temp_upload_path("panic");
    fs::write(&path, &payload).expect("write upload fixture");

    let rec = Arc::new(Mutex::new(Recorder::default()));
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(1)
            .build()
            .expect("valid config"),
    );
    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();

    let task = UploadPounceBuilder::new("p.bin", &path, 1000)
        .with_url("https://placeholder/upload")
        .with_breakpoint_upload(Arc::new(RecordingUpload {
            rec: rec.clone(),
            parallel: true,
            total: payload.len() as u64,
            chunk: 1000,
            scramble: false,
            chunk_delay_ms: 10,
            // The 3rd part (offset 2000) panics while siblings are in flight.
            panic_at_offset: Some(2000),
        }))
        .with_max_parts_in_flight(3)
        .build()
        .expect("build upload task");

    client
        .try_enqueue(
            task,
            move |record| {
                statuses_cb
                    .lock()
                    .expect("lock statuses")
                    .push(record.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("enqueue upload task");

    let mut terminal = None;
    for _ in 0..500 {
        let snapshot = statuses.lock().expect("lock statuses").clone();
        terminal = snapshot
            .iter()
            .rev()
            .find(|s| {
                matches!(
                    s,
                    TransferStatus::Complete | TransferStatus::Failed(_) | TransferStatus::Canceled
                )
            })
            .cloned();
        if terminal.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    client.close().await.expect("close client");
    let _ = fs::remove_file(&path);

    let snapshot = rec.lock().expect("rec lock").clone();
    assert!(
        matches!(terminal, Some(TransferStatus::Failed(_))),
        "a panicked part must terminate the upload as Failed (not Complete/Canceled/hang), got {terminal:?}"
    );
    assert_eq!(
        snapshot.complete_calls, 0,
        "complete must NOT be called when a part panicked"
    );
    assert_eq!(
        snapshot.abort_calls, 1,
        "a provider session created before a part panic must be aborted exactly once"
    );
}
