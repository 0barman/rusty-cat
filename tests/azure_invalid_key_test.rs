//! 故意异常用例 — Azure 账号密钥解码缓存（commit e160b6d）。
//!
//! e160b6d 把 Azure 账号密钥的 base64 解码改为「首次签名时惰性解码并缓存」，并让公开
//! `AzureBlobDirectUpload::new` 保持 infallible。本用例验证**非法 base64 密钥**的 fail-closed
//! 行为：解码错误在**任何网络请求发出之前**冒泡为 `Failed`（错误码 `ParameterEmpty`），
//! 既不 panic，也不会污染缓存、不会发出未签名/错误签名的请求。
//!
//! 需要 `azure-blob-direct` feature；未启用时本文件编译为空。
#![cfg(feature = "azure-blob-direct")]

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusty_cat::azure_blob_direct::AzureBlobDirectUpload;
use rusty_cat::error::InnerErrorCode;
use rusty_cat::{MeowClient, MeowConfig, TransferStatus, UploadPounceBuilder};

/// Local server that answers any request with `200 OK` and counts how many it
/// received — used to prove that NO request is ever sent.
struct CountingServer {
    base_url: String,
    hits: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl CountingServer {
    fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        listener.set_nonblocking(true).expect("nonblocking");
        let stop = Arc::new(AtomicBool::new(false));
        let hits = Arc::new(AtomicU64::new(0));
        let stop_t = stop.clone();
        let hits_t = hits.clone();

        let handle = thread::spawn(move || {
            while !stop_t.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        hits_t.fetch_add(1, Ordering::AcqRel);
                        stream
                            .set_read_timeout(Some(Duration::from_millis(200)))
                            .ok();
                        let mut tmp = [0u8; 1024];
                        let _ = stream.read(&mut tmp);
                        let _ = stream.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                        let _ = stream.flush();
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
}

impl Drop for CountingServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn temp_path() -> PathBuf {
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    p.push(format!("rusty_cat_azure_badkey_{ts}.bin"));
    p
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn azure_invalid_base64_key_fails_closed_before_any_request() {
    let payload = vec![9u8; 1000];
    let path = temp_path();
    fs::write(&path, &payload).expect("write fixture");

    let server = CountingServer::spawn();

    // `new` is infallible even with a structurally invalid key; the error must
    // only surface when signing first decodes it.
    let upload = Arc::new(AzureBlobDirectUpload::new("acct", "@@@ not base64 @@@"));

    let client = MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(1)
            .build()
            .expect("config"),
    );
    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();

    let task = UploadPounceBuilder::new("blob.bin", &path, 1000)
        .with_url(format!("{}/container/blob", server.base_url))
        .with_breakpoint_upload(upload)
        // Even with retries available, a decode error is non-retryable and must
        // not loop or hang.
        .with_max_chunk_retries(3)
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
    for _ in 0..300 {
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
    let hits = server.hits.load(Ordering::Acquire);
    drop(server);
    let _ = fs::remove_file(&path);

    match terminal {
        Some(TransferStatus::Failed(err)) => {
            assert_eq!(
                err.code(),
                InnerErrorCode::ParameterEmpty as i32,
                "an invalid base64 account key must fail as ParameterEmpty, got code {}",
                err.code()
            );
        }
        other => panic!("expected Failed on invalid account key, got {other:?}"),
    }
    assert_eq!(
        hits, 0,
        "signing must fail BEFORE any HTTP request is sent, but the server saw {hits} requests"
    );
}
