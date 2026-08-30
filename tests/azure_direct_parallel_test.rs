//! 单文件内多分片并发 — 真实 `AzureBlobDirectUpload`（commit 2776a94 + 本轮启用）。
//!
//! 驱动真实的 Azure Block Blob 直传协议经执行器的并发窗口（`with_max_parts_in_flight(4)`）
//! 对着一个本地 mock Azure Blob 服务器上传，验证：
//!   - 多个 Put Block 并发在飞（peak >= 2），窗口上限被尊重；
//!   - 每个 block（按 blockid）恰好上传一次，无重无漏；
//!   - Put Block List（收尾）恰好一次，且**在全部 block 到齐之后**才发出（completion 被门控）；
//!   - 整体到达 Complete。
//!
//! 需要 `azure-blob-direct` feature；未启用时本文件编译为空。
#![cfg(feature = "azure-blob-direct")]

use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusty_cat::azure_blob_direct::AzureBlobDirectUpload;
use rusty_cat::{MeowClient, MeowConfig, TransferStatus, UploadPounceBuilder};

#[derive(Default)]
struct ServerState {
    /// Distinct blockids that received a Put Block.
    blocks: BTreeSet<String>,
    in_flight: usize,
    peak_in_flight: usize,
    /// Number of Put Block List (complete) calls.
    complete_calls: usize,
    /// Distinct blocks already present when the FIRST complete arrived.
    blocks_at_complete: usize,
}

struct AzureMock {
    base_url: String,
    state: Arc<Mutex<ServerState>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl AzureMock {
    fn spawn(block_delay_ms: u64) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        listener.set_nonblocking(true).expect("nonblocking");
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(ServerState::default()));
        let stop_t = stop.clone();
        let state_t = state.clone();

        let handle = thread::spawn(move || {
            while !stop_t.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let st = state_t.clone();
                        thread::spawn(move || handle_conn(stream, st, block_delay_ms));
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

impl Drop for AzureMock {
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
    block_delay_ms: u64,
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
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");

    let (status, body): (&str, &str) = if method == "PUT" && query.contains("blockid") {
        // Put Block: dedup by blockid, track real concurrency, induce overlap.
        let blockid = query
            .split('&')
            .find_map(|kv| kv.strip_prefix("blockid="))
            .unwrap_or("")
            .to_string();
        {
            let mut s = state.lock().expect("state");
            s.in_flight += 1;
            s.peak_in_flight = s.peak_in_flight.max(s.in_flight);
        }
        thread::sleep(Duration::from_millis(block_delay_ms));
        {
            let mut s = state.lock().expect("state");
            s.blocks.insert(blockid);
            s.in_flight -= 1;
        }
        ("HTTP/1.1 201 Created", "")
    } else if method == "PUT" && query.contains("blocklist") {
        // Put Block List (complete): snapshot how many blocks were already present.
        let mut s = state.lock().expect("state");
        s.complete_calls += 1;
        if s.complete_calls == 1 {
            s.blocks_at_complete = s.blocks.len();
        }
        ("HTTP/1.1 201 Created", "")
    } else if method == "GET" {
        // List uncommitted blocks (only hit on resume; fresh prepare is local).
        (
            "HTTP/1.1 200 OK",
            "<?xml version=\"1.0\"?><BlockList></BlockList>",
        )
    } else {
        ("HTTP/1.1 200 OK", "")
    };

    let resp = format!(
        "{status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
    // A plain drop can emit TCP RST on some platforms when bytes are still in
    // the receive queue, making reqwest intermittently report IncompleteMessage
    // or ConnectionReset even though the complete response was written. Close
    // the response side first so the client observes a deterministic EOF, then
    // briefly drain until it closes its request side.
    let _ = stream.shutdown(std::net::Shutdown::Write);
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .ok();
    loop {
        match stream.read(&mut tmp) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
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

fn temp_path() -> PathBuf {
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    p.push(format!("rusty_cat_azure_direct_parallel_{ts}.bin"));
    p
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn azure_direct_parallel_blocks_upload_concurrently_and_complete_once() {
    let chunk: u64 = 1000;
    let total: u64 = 6000; // 6 blocks
    let n_blocks = (total / chunk) as usize;
    let payload: Vec<u8> = (0..total as u32).map(|i| (i % 251) as u8).collect();
    let path = temp_path();
    fs::write(&path, &payload).expect("write fixture");

    // ~25ms per Put Block ensures several overlap inside a 4-wide window.
    let server = AzureMock::spawn(25);

    // "a2V5" is valid base64 ("key"); the mock ignores the SharedKey signature.
    let proto = Arc::new(AzureBlobDirectUpload::new("acct", "a2V5"));

    let client = MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(1)
            .build()
            .expect("config"),
    );
    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();

    let task = UploadPounceBuilder::new("blob.bin", &path, chunk)
        .with_url(format!("{}/container/blob", server.base_url))
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
            g.blocks.len(),
            g.complete_calls,
            g.blocks_at_complete,
            g.peak_in_flight,
        )
    };
    drop(server);
    let _ = fs::remove_file(&path);
    let (blocks_len, complete_calls, blocks_at_complete, peak) = s;

    assert!(
        matches!(terminal, Some(TransferStatus::Complete)),
        "concurrent Azure direct upload must reach Complete, got {terminal:?}"
    );
    assert_eq!(
        blocks_len, n_blocks,
        "every block must be Put exactly once (no gap, no dup)"
    );
    assert_eq!(
        complete_calls, 1,
        "Put Block List (complete) must fire exactly once"
    );
    assert_eq!(
        blocks_at_complete, n_blocks,
        "complete must be gated: all blocks present before Put Block List"
    );
    assert!(
        peak >= 2,
        "expected genuine concurrency among Put Block calls, peak = {peak}"
    );
    assert!(
        peak <= 4,
        "in-flight blocks must never exceed max_parts_in_flight=4, observed {peak}"
    );
}
