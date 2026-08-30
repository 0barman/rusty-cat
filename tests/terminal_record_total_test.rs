//! 终态/控制面记录必须携带运行期探测的 total 的端到端回归
//! （对应线上反馈：分片下载终态事件把 DB 进度清零）。
//! 三个场景都刻意不调用 with_total_size，复现"未预设 total"的真实用法。

#[path = "dev_server/mod.rs"]
mod dev_server;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusty_cat::down_pounce_builder::DownloadPounceBuilder;
use rusty_cat::file_transfer_record::FileTransferRecord;
use rusty_cat::meow_config::MeowConfig;
use rusty_cat::transfer_status::TransferStatus;
use rusty_cat::MeowClient;

fn parse_range_header(req: &str) -> Option<(u64, u64)> {
    let line = req
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("range:"))?;
    let value = line.split_once(':')?.1.trim();
    let range = value.strip_prefix("bytes=")?;
    let (start, end) = range.split_once('-')?;
    Some((start.parse().ok()?, end.parse().ok()?))
}

/// 支持 HEAD + Range GET 的本地 server，每个 GET 前 sleep `get_delay_ms`，
/// 便于稳定地在传输中途 cancel/close。与 pause_resume_test.rs 的同名
/// helper 保持一致（集成测试文件间无法共享代码，各自内联）。
fn spawn_range_server(
    payload: Vec<u8>,
    get_delay_ms: u64,
) -> (String, Arc<AtomicBool>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind local test server");
    let addr = listener.local_addr().expect("read local addr");
    listener
        .set_nonblocking(true)
        .expect("set listener nonblocking");
    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_thread = stop.clone();
    let payload_for_thread = payload.clone();
    let handle = thread::spawn(move || loop {
        if stop_for_thread.load(Ordering::Acquire) {
            break;
        }
        let (mut stream, _) = match listener.accept() {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            Err(_) => break,
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let mut buf = [0u8; 4096];
        let n = match stream.read(&mut buf) {
            Ok(n) => n,
            Err(_) => continue,
        };
        let req = String::from_utf8_lossy(&buf[..n]).to_string();
        if req.starts_with("HEAD ") {
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"terminal-total-v1\"\r\nConnection: close\r\n\r\n",
                payload_for_thread.len()
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
            continue;
        }
        let Some((start, end)) = parse_range_header(&req) else {
            let _ = stream.write_all(
                b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            let _ = stream.flush();
            continue;
        };
        thread::sleep(Duration::from_millis(get_delay_ms));
        let start_idx = start as usize;
        let end_idx = end as usize;
        if start_idx >= payload_for_thread.len() || end_idx < start_idx {
            let _ = stream.write_all(
                b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            let _ = stream.flush();
            continue;
        }
        let bounded_end = end_idx.min(payload_for_thread.len() - 1);
        let body = &payload_for_thread[start_idx..=bounded_end];
        let resp = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {}-{}/{}\r\nContent-Length: {}\r\nETag: \"terminal-total-v1\"\r\nConnection: close\r\n\r\n",
            start_idx,
            bounded_end,
            payload_for_thread.len(),
            body.len()
        );
        let _ = stream.write_all(resp.as_bytes());
        let _ = stream.write_all(body);
        let _ = stream.flush();
    });
    (format!("http://{addr}/download"), stop, handle)
}

fn temp_download_path(case: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    p.push(format!("rusty_cat_terminal_total_{case}_{ts}.bin"));
    p
}

fn config() -> MeowConfig {
    MeowConfig::builder()
        .max_upload_concurrency(1)
        .max_download_concurrency(2)
        .build()
        .expect("valid config")
}

async fn wait_until<F>(timeout_ms: u64, mut pred: F)
where
    F: FnMut() -> bool,
{
    let mut waited = 0u64;
    while waited < timeout_ms {
        if pred() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        waited += 20;
    }
    panic!("condition wait timed out after {timeout_ms}ms");
}

fn last_matching(
    records: &Arc<Mutex<Vec<FileTransferRecord>>>,
    pred: impl Fn(&FileTransferRecord) -> bool,
) -> Option<FileTransferRecord> {
    records
        .lock()
        .expect("lock records")
        .iter()
        .rev()
        .find(|r| pred(r))
        .cloned()
}

/// 中途 cancel：终态 Canceled 记录必须保留运行期 total 与真实进度。
#[tokio::test]
async fn canceled_download_terminal_record_carries_runtime_total() {
    // 128KB / 4096 = 32 个分片，每片 150ms → 整体传输约 4.8s。测试在第一片
    // 完成（progress>0）后立即 cancel，此时还剩约 4.6s 的工作量，下载绝不可能
    // 抢先完成，从而消除"下载先于 cancel 结束导致无终态记录"的 flaky（finding #12）。
    let payload = vec![7u8; 128 * 1024];
    let (url, stop, handle) = spawn_range_server(payload.clone(), 150);

    let local_path = temp_download_path("cancel");
    let client = MeowClient::new(config());
    let records: Arc<Mutex<Vec<FileTransferRecord>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = records.clone();

    let task = DownloadPounceBuilder::new("cancel.bin", &local_path, 4096, url).build();
    let task_id = client
        .try_enqueue(task, move |r| sink.lock().expect("lock").push(r), |_, _| {})
        .await
        .expect("enqueue download");

    wait_until(15_000, || {
        records
            .lock()
            .expect("lock")
            .iter()
            .any(|r| matches!(r.status(), TransferStatus::Transmission) && r.progress() > 0.0)
    })
    .await;

    client.cancel(task_id).await.expect("cancel task");

    wait_until(15_000, || {
        records
            .lock()
            .expect("lock")
            .iter()
            .any(|r| matches!(r.status(), TransferStatus::Canceled))
    })
    .await;

    let canceled = last_matching(&records, |r| matches!(r.status(), TransferStatus::Canceled))
        .expect("canceled record");
    assert_eq!(
        canceled.total_size(),
        payload.len() as u64,
        "终态必须保留运行期探测的 total"
    );
    assert!(canceled.progress() > 0.0, "终态必须保留已完成进度");

    client.close().await.expect("close client");
    stop.store(true, Ordering::Release);
    let _ = handle.join();
    let _ = std::fs::remove_file(&local_path);
}

/// 分片失败：终态 Failed 记录必须携带 HEAD 探测到的 total（进度可为 0，
/// 因为首个分片即失败——断言只针对 total）。
#[tokio::test]
async fn failed_download_terminal_record_carries_runtime_total() {
    let chunk = vec![9u8; 4096];
    let server = dev_server::FlakyDownloadServer::spawn_one_chunk(1, chunk.clone());

    let local_path = temp_download_path("failed");
    let client = MeowClient::new(config());
    let records: Arc<Mutex<Vec<FileTransferRecord>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = records.clone();

    let task = DownloadPounceBuilder::new(
        "failed.bin",
        &local_path,
        4096,
        format!("{}/download", server.base_url()),
    )
    .with_max_chunk_retries(0)
    .build();
    client
        .try_enqueue(task, move |r| sink.lock().expect("lock").push(r), |_, _| {})
        .await
        .expect("enqueue download");

    wait_until(15_000, || {
        records
            .lock()
            .expect("lock")
            .iter()
            .any(|r| matches!(r.status(), TransferStatus::Failed(_)))
    })
    .await;

    let failed = last_matching(&records, |r| {
        matches!(r.status(), TransferStatus::Failed(_))
    })
    .expect("failed record");
    assert_eq!(
        failed.total_size(),
        chunk.len() as u64,
        "prepare(HEAD) 探测到的 total 必须保留到终态"
    );

    client.close().await.expect("close client");
    server.shutdown();
    let _ = std::fs::remove_file(&local_path);
}

/// close()：对 in-flight 下载广播的 Paused 记录必须携带运行期 total
/// （修复前每次干净关闭都会把所有进行中下载的 DB 进度清零；
/// close 返回前保证回调已投递完毕，断言无竞态）。
#[tokio::test]
async fn close_emits_paused_with_runtime_total() {
    // 同 cancel 用例：128KB / 32 片保证 close 落地时下载仍在进行（finding #12）。
    let payload = vec![5u8; 128 * 1024];
    let (url, stop, handle) = spawn_range_server(payload.clone(), 150);

    let local_path = temp_download_path("close");
    let client = MeowClient::new(config());
    let records: Arc<Mutex<Vec<FileTransferRecord>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = records.clone();

    let task = DownloadPounceBuilder::new("close.bin", &local_path, 4096, url).build();
    client
        .try_enqueue(task, move |r| sink.lock().expect("lock").push(r), |_, _| {})
        .await
        .expect("enqueue download");

    wait_until(15_000, || {
        records
            .lock()
            .expect("lock")
            .iter()
            .any(|r| matches!(r.status(), TransferStatus::Transmission) && r.progress() > 0.0)
    })
    .await;

    client.close().await.expect("close client");

    let paused = last_matching(&records, |r| matches!(r.status(), TransferStatus::Paused))
        .expect("paused record from close");
    assert_eq!(paused.total_size(), payload.len() as u64);
    assert!(paused.progress() > 0.0);

    stop.store(true, Ordering::Release);
    let _ = handle.join();
    let _ = std::fs::remove_file(&local_path);
}
