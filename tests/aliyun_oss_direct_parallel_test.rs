//! 单文件内多分片并发 — 真实 `AliOssDirectUpload`（commit 2776a94 + 本轮启用）。
//!
//! 本轮给 OSS 直传开启了并发分片（`supports_parallel_parts -> true`，因其分片身份
//! `part_number = offset/chunk + 1` 为 offset 派生、幂等，complete 由 `x-oss-complete-all`
//! 按序合并，乱序安全）。本用例驱动真实 OSS 直传协议经执行器并发窗口
//! （`with_max_parts_in_flight(4)`）对着本地 mock OSS 服务器上传，验证：
//!   - Initiate Multipart 恰好一次（拿到 UploadId）；
//!   - 多个 Upload Part 并发在飞（peak >= 2），窗口上限被尊重；
//!   - 每个 partNumber 恰好上传一次，无重无漏；
//!   - Complete Multipart 恰好一次，且**在全部 part 到齐之后**才发出（completion 被门控）；
//!   - 整体到达 Complete。
//!
//! 需要 `aliyun-oss-direct` feature；未启用时本文件编译为空。
#![cfg(feature = "aliyun-oss-direct")]

use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusty_cat::aliyun_oss_direct::AliOssDirectUpload;
use rusty_cat::{MeowClient, MeowConfig, TransferStatus, UploadPounceBuilder};

#[derive(Default)]
struct ServerState {
    /// Distinct part numbers that received an Upload Part.
    parts: BTreeSet<u64>,
    in_flight: usize,
    peak_in_flight: usize,
    initiate_calls: usize,
    complete_calls: usize,
    /// Distinct parts already present when the FIRST complete arrived.
    parts_at_complete: usize,
}

struct OssMock {
    base_url: String,
    state: Arc<Mutex<ServerState>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl OssMock {
    fn spawn(part_delay_ms: u64) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        listener.set_nonblocking(true).expect("nonblocking");
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(ServerState::default()));
        let etag_seq = Arc::new(AtomicU64::new(0));
        let stop_t = stop.clone();
        let state_t = state.clone();
        let etag_t = etag_seq.clone();

        let handle = thread::spawn(move || {
            while !stop_t.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let st = state_t.clone();
                        let et = etag_t.clone();
                        thread::spawn(move || handle_conn(stream, st, et, part_delay_ms));
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
            state,
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for OssMock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn handle_conn(
    mut stream: std::net::TcpStream,
    state: Arc<Mutex<ServerState>>,
    etag_seq: Arc<AtomicU64>,
    part_delay_ms: u64,
) {
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(2))).ok();

    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        match stream.read(&mut tmp) {
            Ok(0) => return,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos;
                }
            }
            Err(_) => return,
        }
    };
    let header = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut remaining = content_length(&header).saturating_sub(buf.len() - (header_end + 4));
    while remaining > 0 {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => remaining = remaining.saturating_sub(n),
            Err(_) => break,
        }
    }

    let request_line = header.lines().next().unwrap_or_default();
    let mut rl = request_line.split_whitespace();
    let method = rl.next().unwrap_or_default();
    let target = rl.next().unwrap_or_default();
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");

    // Default response.
    let mut status = "HTTP/1.1 200 OK".to_string();
    let mut extra_header = String::new();
    let mut body = String::new();

    if method == "POST" && query.contains("uploadId") {
        // Complete Multipart Upload.
        let mut s = state.lock().expect("state");
        s.complete_calls += 1;
        if s.complete_calls == 1 {
            s.parts_at_complete = s.parts.len();
        }
        body = "<CompleteMultipartUploadResult></CompleteMultipartUploadResult>".to_string();
    } else if method == "POST" {
        // Initiate Multipart Upload (?uploads).
        state.lock().expect("state").initiate_calls += 1;
        body = "<?xml version=\"1.0\"?><InitiateMultipartUploadResult>\
                <UploadId>test-upload-id</UploadId></InitiateMultipartUploadResult>"
            .to_string();
    } else if method == "PUT" && query.contains("partNumber") {
        // Upload Part: dedup by partNumber, track concurrency, return an ETag.
        let pn = query
            .split('&')
            .find_map(|kv| kv.strip_prefix("partNumber="))
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        {
            let mut s = state.lock().expect("state");
            s.in_flight += 1;
            s.peak_in_flight = s.peak_in_flight.max(s.in_flight);
        }
        thread::sleep(Duration::from_millis(part_delay_ms));
        {
            let mut s = state.lock().expect("state");
            s.parts.insert(pn);
            s.in_flight -= 1;
        }
        let etag = etag_seq.fetch_add(1, Ordering::Relaxed);
        extra_header = format!("ETag: \"etag-{etag}\"\r\n");
    } else if method == "GET" {
        // List multipart uploads / parts (only on resume; not hit fresh).
        status = "HTTP/1.1 404 Not Found".to_string();
    }

    let resp = format!(
        "{status}\r\n{extra_header}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
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

fn temp_path() -> PathBuf {
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    p.push(format!("rusty_cat_oss_direct_parallel_{ts}.bin"));
    p
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oss_direct_parallel_parts_upload_concurrently_and_complete_once() {
    let chunk: u64 = 1000;
    let total: u64 = 6000; // 6 parts
    let n_parts = (total / chunk) as usize;
    let payload: Vec<u8> = (0..total as u32).map(|i| (i % 251) as u8).collect();
    let path = temp_path();
    fs::write(&path, &payload).expect("write fixture");

    // ~25ms per Upload Part ensures several overlap inside a 4-wide window.
    let server = OssMock::spawn(25);

    let proto = Arc::new(AliOssDirectUpload::new("bucket", "ak", "sk", "cn-hangzhou"));

    let client = MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(1)
            .build()
            .expect("config"),
    );
    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();

    let task = UploadPounceBuilder::new("object.bin", &path, chunk)
        .with_url(format!("{}/object.bin", server.base_url))
        .with_breakpoint_upload(proto)
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
    let s = {
        let g = server.state.lock().expect("state");
        (
            g.parts.iter().copied().collect::<Vec<_>>(),
            g.initiate_calls,
            g.complete_calls,
            g.parts_at_complete,
            g.peak_in_flight,
        )
    };
    drop(server);
    let _ = fs::remove_file(&path);
    let (parts, initiate_calls, complete_calls, parts_at_complete, peak) = s;

    assert!(
        matches!(terminal, Some(TransferStatus::Complete)),
        "concurrent OSS direct upload must reach Complete, got {terminal:?}"
    );
    assert_eq!(
        initiate_calls, 1,
        "Initiate Multipart must fire exactly once"
    );
    assert_eq!(
        parts,
        (1..=n_parts as u64).collect::<Vec<_>>(),
        "every partNumber 1..=N must be uploaded exactly once (no gap, no dup)"
    );
    assert_eq!(
        complete_calls, 1,
        "Complete Multipart must fire exactly once"
    );
    assert_eq!(
        parts_at_complete, n_parts,
        "complete must be gated: all parts present before Complete Multipart"
    );
    assert!(
        peak >= 2,
        "expected genuine concurrency among Upload Part calls, peak = {peak}"
    );
    assert!(
        peak <= 4,
        "in-flight parts must never exceed max_parts_in_flight=4, observed {peak}"
    );
}
