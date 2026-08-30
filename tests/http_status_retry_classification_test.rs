//! 故意异常用例 — 4xx 快速失败 vs 5xx 重试（commit 41c9244）。
//!
//! 41c9244 让传输层把 HTTP 状态码挂到 `MeowError` 上，并据此在重试层区分：
//!   - 硬客户端错误（4xx，除 408/429）：重发同一请求无意义 -> **快速失败**，只尝试 1 次；
//!   - 服务端瞬时错误（5xx，及 408/429）：保持可重试，最多 `1 + max_chunk_retries` 次。
//!
//! `MeowError::with_http_status` 是 `pub(crate)`，自定义 mock 无法附带状态码，因此这条
//! 端到端语义只能经一个**真实会附带状态码的内置协议**验证。这里用 presigned multipart
//! 的分片 PUT（`send` 非 2xx 时 `.with_http_status(status)`），对着一个会回固定状态码并
//! 统计请求次数的本地服务器，断言尝试次数。
//!
//! 需要 `presigned` feature（`--features presigned` / `aliyun-oss-presigned` /
//! `azure-blob-sas` / `--features all`）；未启用时本文件编译为空。
#![cfg(feature = "presigned")]

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

/// Local HTTP server that answers every PUT with a fixed status code and counts
/// how many requests it received.
struct StatusServer {
    base_url: String,
    hits: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl StatusServer {
    fn spawn(status_line: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind status server");
        let addr = listener.local_addr().expect("status server addr");
        listener.set_nonblocking(true).expect("nonblocking");
        let stop = Arc::new(AtomicBool::new(false));
        let hits = Arc::new(AtomicU64::new(0));
        let stop_t = stop.clone();
        let hits_t = hits.clone();

        let handle = thread::spawn(move || {
            while !stop_t.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let hits_c = hits_t.clone();
                        thread::spawn(move || handle_conn(stream, status_line, hits_c));
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
            hits,
            stop,
            handle: Some(handle),
        }
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn hits(&self) -> u64 {
        self.hits.load(Ordering::Acquire)
    }
}

impl Drop for StatusServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn handle_conn(mut stream: std::net::TcpStream, status_line: &str, hits: Arc<AtomicU64>) {
    stream.set_nonblocking(false).ok();
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(2))).ok();

    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => return,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&buf[..pos]).to_string();
                    let content_len = content_length(&header);
                    // Drain the request body so the client's write completes.
                    let mut remaining = content_len.saturating_sub(buf.len() - (pos + 4));
                    while remaining > 0 {
                        match stream.read(&mut tmp) {
                            Ok(0) => break,
                            Ok(n) => remaining = remaining.saturating_sub(n),
                            Err(_) => break,
                        }
                    }
                    // Count only after we have a full request.
                    hits.fetch_add(1, Ordering::AcqRel);
                    let resp =
                        format!("{status_line}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    let _ = stream.write_all(resp.as_bytes());
                    let _ = stream.flush();
                    return;
                }
            }
            Err(_) => return,
        }
    }
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

fn temp_path(case: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    p.push(format!("rusty_cat_status_retry_{case}_{ts}.bin"));
    p
}

/// Drives a single-part presigned upload against a server that always returns
/// `status_line`, with `max_chunk_retries`, and returns (terminal, server hits).
async fn run_against_status(
    case: &str,
    status_line: &'static str,
    max_chunk_retries: u32,
) -> (Option<TransferStatus>, u64) {
    let total: u64 = 1000;
    let chunk: u64 = 1000; // single part covering the whole file
    let payload: Vec<u8> = (0..total as u32).map(|i| (i % 251) as u8).collect();
    let path = temp_path(case);
    fs::write(&path, &payload).expect("write fixture");

    let server = StatusServer::spawn(status_line);

    let parts = vec![PresignedUploadPart::put(
        1,
        0,
        chunk,
        format!("{}/part/0", server.base_url()),
    )];
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
        .with_breakpoint_upload(proto)
        .with_max_chunk_retries(max_chunk_retries)
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
    let hits = server.hits();
    drop(server);
    let _ = fs::remove_file(&path);
    (terminal, hits)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hard_4xx_response_fast_fails_without_retry() {
    // 403 is a hard client error: re-sending the same request cannot help, so the
    // retry layer must NOT retry — exactly one upload attempt, then Failed, even
    // though up to 5 retries are allowed.
    let (terminal, hits) = run_against_status("forbidden", "HTTP/1.1 403 Forbidden", 5).await;

    assert!(
        matches!(terminal, Some(TransferStatus::Failed(_))),
        "a 403 chunk must terminate as Failed, got {terminal:?}"
    );
    assert_eq!(
        hits, 1,
        "a hard 4xx must fast-fail with exactly one attempt (no retry), saw {hits} requests"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transient_5xx_response_is_retried_until_exhausted() {
    // 503 is a transient server error: the retry layer must keep retrying up to
    // 1 + max_chunk_retries attempts before giving up.
    let (terminal, hits) =
        run_against_status("unavailable", "HTTP/1.1 503 Service Unavailable", 2).await;

    assert!(
        matches!(terminal, Some(TransferStatus::Failed(_))),
        "a persistently 503 chunk must end Failed once retries exhaust, got {terminal:?}"
    );
    assert_eq!(
        hits, 3,
        "a transient 5xx must be retried: 1 initial + 2 retries = 3 attempts, saw {hits}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn throttle_429_response_is_retried() {
    // 429 Too Many Requests is explicitly retryable (alongside 408 and 5xx),
    // unlike other 4xx — guard that classification too.
    let (terminal, hits) =
        run_against_status("throttle", "HTTP/1.1 429 Too Many Requests", 2).await;

    assert!(
        matches!(terminal, Some(TransferStatus::Failed(_))),
        "a persistently 429 chunk must end Failed once retries exhaust, got {terminal:?}"
    );
    assert_eq!(
        hits, 3,
        "429 must be retried like 5xx: 1 initial + 2 retries = 3 attempts, saw {hits}"
    );
}
