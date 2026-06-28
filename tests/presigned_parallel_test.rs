//! GAP-CONCURRENT-SEED — drives the REAL `PresignedMultipartUpload` through the
//! REAL executor's opt-in parallel-parts path against a local server, asserting
//! that concurrent, out-of-order `upload_chunk` calls populate the protocol's
//! `Mutex<Vec<part>>` accounting correctly: every chunk-aligned offset is
//! recorded exactly once (no gap, no duplicate), each with its uploaded ETag,
//! and the upload still reaches Complete.
//!
//! This complements:
//!   - the isomorphic loom model (`rusty-cat-loom-model/tests/parts_mutex_vec.rs`),
//!     which proves the find-or-update/push critical section is race-free under
//!     every interleaving;
//!   - `presigned::tests::presigned_accounting_is_order_independent`, which proves
//!     the resume/completion read side is order-independent.
//!
//! Requires the `presigned` feature (enabled by `aliyun-oss-presigned` /
//! `azure-blob-sas` / `--features all`). Without it the file compiles to no tests.
#![cfg(feature = "presigned")]

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusty_cat::presigned::{
    PresignedMultipartUpload, PresignedMultipartUploadPlan, PresignedUploadPart,
};
use rusty_cat::{MeowClient, MeowConfig, TransferStatus, UploadPounceBuilder};

/// Minimal local HTTP server that accepts presigned PUTs and returns `200 OK`
/// with a unique `ETag`. To stress out-of-order completion it delays each
/// response inversely to the part's offset (encoded in the path `/part/{offset}`)
/// so higher offsets finish first.
struct PutServer {
    base_url: String,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl PutServer {
    fn spawn(total: u64, chunk: u64) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind put server");
        let addr = listener.local_addr().expect("put server addr");
        listener
            .set_nonblocking(true)
            .expect("put server nonblocking");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = stop.clone();
        let etag_seq = Arc::new(AtomicU64::new(0));

        let handle = thread::spawn(move || {
            while !stop_for_thread.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let etag = etag_seq.fetch_add(1, Ordering::Relaxed);
                        // One thread per connection so concurrent in-flight PUTs
                        // are genuinely served in parallel.
                        thread::spawn(move || handle_put(stream, etag, total, chunk));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            base_url: format!("http://{addr}"),
            stop,
            handle: Some(handle),
        }
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }
}

impl Drop for PutServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_put(mut stream: std::net::TcpStream, etag: u64, total: u64, chunk: u64) {
    stream.set_nonblocking(false).ok();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .ok();
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .ok();

    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    // Read until we have the full header block.
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = find_header_end(&buf) {
                    let header = String::from_utf8_lossy(&buf[..pos]).to_string();
                    let content_len = content_length(&header);
                    let body_have = buf.len() - (pos + 4);
                    // Drain the request body so the client's write completes.
                    let mut remaining = content_len.saturating_sub(body_have);
                    while remaining > 0 {
                        match stream.read(&mut tmp) {
                            Ok(0) => break,
                            Ok(n) => remaining = remaining.saturating_sub(n),
                            Err(_) => break,
                        }
                    }
                    // Induce out-of-order completion: higher offsets respond first.
                    let offset = path_offset(&header);
                    let steps = total.saturating_sub(offset) / chunk.max(1);
                    thread::sleep(Duration::from_millis(steps * 8));
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nETag: \"etag-{etag}\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = stream.write_all(resp.as_bytes());
                    let _ = stream.flush();
                    return;
                }
            }
            Err(_) => break,
        }
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn content_length(header: &str) -> usize {
    header
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            if k.trim().eq_ignore_ascii_case("content-length") {
                v.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0)
}

/// Parses the offset out of a `PUT /part/{offset} HTTP/1.1` request line.
fn path_offset(header: &str) -> u64 {
    header
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|path| path.rsplit('/').next())
        .and_then(|seg| seg.parse::<u64>().ok())
        .unwrap_or(0)
}

fn temp_path(case: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    p.push(format!("rusty_cat_presigned_parallel_{case}_{ts}.bin"));
    p
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn presigned_parallel_out_of_order_records_every_part_once_and_completes() {
    let total: u64 = 6000;
    let chunk: u64 = 1000;
    let payload: Vec<u8> = (0..total as u32).map(|i| (i % 251) as u8).collect();
    let path = temp_path("ooo");
    fs::write(&path, &payload).expect("write fixture");

    let server = PutServer::spawn(total, chunk);

    // One presigned part per chunk; the part URL encodes the offset so the server
    // can scramble completion order. No completion request -> `complete` is a
    // no-op `Ok(None)`, so no extra endpoint is needed.
    let parts: Vec<PresignedUploadPart> = (0..total / chunk)
        .map(|i| {
            let offset = i * chunk;
            PresignedUploadPart::put(
                i + 1,
                offset,
                chunk,
                format!("{}/part/{offset}", server.base_url()),
            )
        })
        .collect();
    let plan = PresignedMultipartUploadPlan::new(total, chunk, parts).with_upload_id("u-test");
    let proto = Arc::new(PresignedMultipartUpload::new(plan));

    let client = MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(1)
            .build()
            .expect("config"),
    );
    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();

    let task = UploadPounceBuilder::new("p.bin", &path, chunk)
        .with_url(server.base_url())
        .with_breakpoint_upload(proto.clone())
        .with_max_parts_in_flight(4)
        .build()
        .expect("build task");

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
        .expect("enqueue");

    let mut terminal = None;
    for _ in 0..600 {
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

    client.close().await.expect("close");
    let _ = fs::remove_file(&path);

    assert!(
        matches!(terminal, Some(TransferStatus::Complete)),
        "concurrent presigned upload must reach Complete, got {terminal:?}"
    );

    // The protocol's accounting must hold exactly one record per chunk-aligned
    // offset despite concurrent, out-of-order PUT completion.
    let recorded = proto.uploaded_parts().await;
    let mut by_offset: BTreeMap<u64, u64> = BTreeMap::new();
    for p in &recorded {
        let prev = by_offset.insert(p.offset, p.size);
        assert!(prev.is_none(), "offset {} recorded more than once", p.offset);
        assert!(p.etag.is_some(), "part at offset {} missing etag", p.offset);
    }
    let offsets: Vec<u64> = by_offset.keys().copied().collect();
    assert_eq!(
        offsets,
        vec![0, 1000, 2000, 3000, 4000, 5000],
        "every chunk-aligned offset must be recorded exactly once (no gap, no dup)"
    );
    assert!(
        by_offset.values().all(|&s| s == chunk),
        "every recorded part must carry its full chunk size"
    );
}
