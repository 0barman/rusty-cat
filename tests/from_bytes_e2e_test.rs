#[path = "dev_server/mod.rs"]
mod dev_server;

use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusty_cat::error::InnerErrorCode;
use rusty_cat::file_transfer_record::FileTransferRecord;
use rusty_cat::meow_config::MeowConfig;
use rusty_cat::transfer_status::TransferStatus;
use rusty_cat::up_pounce_builder::UploadPounceBuilder;
use rusty_cat::MeowClient;

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn read_http_request(
    stream: &mut std::net::TcpStream,
) -> std::io::Result<(String, Vec<(String, String)>, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut temp = [0u8; 4096];
    let mut idle_reads = 0u32;
    loop {
        let n = match stream.read(&mut temp) {
            Ok(n) => n,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                idle_reads += 1;
                if idle_reads > 100 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "timed out while reading request headers",
                    ));
                }
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            Err(e) => return Err(e),
        };
        if n == 0 {
            break;
        }
        idle_reads = 0;
        buf.extend_from_slice(&temp[..n]);
        if find_header_end(&buf).is_some() {
            break;
        }
    }

    let header_end = find_header_end(&buf).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "http header not complete")
    })?;

    let head = &buf[..header_end];
    let head_str = String::from_utf8_lossy(head);
    let mut lines = head_str.split("\r\n");
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }

    let content_len = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("Content-Length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);

    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < content_len {
        let n = match stream.read(&mut temp) {
            Ok(n) => n,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                idle_reads += 1;
                if idle_reads > 100 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "timed out while reading request body",
                    ));
                }
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            Err(e) => return Err(e),
        };
        if n == 0 {
            break;
        }
        idle_reads = 0;
        body.extend_from_slice(&temp[..n]);
    }

    Ok((request_line, headers, body))
}

fn temp_upload_path(case: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    p.push(format!("rusty_cat_from_bytes_e2e_{case}_{ts}.bin"));
    p
}

async fn wait_terminal(
    statuses: Arc<Mutex<Vec<TransferStatus>>>,
    max_iters: usize,
) -> TransferStatus {
    for _ in 0..max_iters {
        if let Some(s) = statuses
            .lock()
            .expect("lock statuses")
            .iter()
            .rev()
            .find(|s| {
                matches!(
                    s,
                    TransferStatus::Complete | TransferStatus::Failed(_) | TransferStatus::Canceled
                )
            })
        {
            return s.clone();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("did not observe terminal status in time");
}

struct FlakyUploadServer {
    base_url: String,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl FlakyUploadServer {
    fn spawn_fail_on_second_post_once() -> Self {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind flaky upload server");
        let addr = listener.local_addr().expect("read local addr");
        listener
            .set_nonblocking(true)
            .expect("set server nonblocking");

        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = stop.clone();
        let post_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let post_count_for_thread = post_count.clone();

        let handle = thread::spawn(move || {
            while !stop_for_thread.load(Ordering::Acquire) {
                let accepted = listener.accept();
                let (mut stream, _) = match accepted {
                    Ok(v) => v,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => break,
                };

                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let (request_line, _, _) = match read_http_request(&mut stream) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if !request_line.starts_with("POST ") {
                    let _ = stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    let _ = stream.flush();
                    continue;
                }

                let idx =
                    post_count_for_thread.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                if idx == 2 {
                    let body = b"intentional chunk failure";
                    let resp = format!(
                        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(resp.as_bytes());
                    let _ = stream.write_all(body);
                    let _ = stream.flush();
                    continue;
                }

                let body = if idx == 1 {
                    b"{\"nextByte\":0}".as_slice()
                } else {
                    b"{}".as_slice()
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.write_all(body);
                let _ = stream.flush();
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

    fn shutdown(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            handle.join().expect("join flaky upload server");
        }
    }
}

impl Drop for FlakyUploadServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[tokio::test]
async fn from_bytes_upload_success_and_file_sign_matches_payload_md5() {
    let payload = b"from-bytes-success-payload".repeat(4096);
    let expected_sign = format!("{:x}", md5::compute(&payload));

    let server = dev_server::DevFileServer::spawn(Vec::new());
    let client = MeowClient::new(MeowConfig::new(1, 1));

    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();
    let signs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let signs_cb = signs.clone();

    let task = UploadPounceBuilder::from_bytes("mem.bin", payload.clone(), 4096)
        .with_url(format!("{}/upload/mem.bin", server.base_url()))
        .build()
        .expect("build from_bytes task");

    client
        .try_enqueue(
            task,
            move |record: FileTransferRecord| {
                statuses_cb
                    .lock()
                    .expect("lock statuses")
                    .push(record.status().clone());
                signs_cb
                    .lock()
                    .expect("lock signs")
                    .push(record.file_sign().to_string());
            },
            |_, _| {},
        )
        .await
        .expect("enqueue from_bytes upload");

    let terminal = wait_terminal(statuses, 500).await;
    assert!(matches!(terminal, TransferStatus::Complete));

    let signs = signs.lock().expect("lock signs final").clone();
    assert!(!signs.is_empty(), "should observe callback file_sign");
    assert!(
        signs.iter().all(|s| s == &expected_sign),
        "all file_sign values should equal payload md5"
    );

    client.close().await.expect("close client");
    let inspect = server.upload_inspector();
    server.shutdown();

    assert_eq!(inspect.max_next_byte, payload.len() as u64);
    assert!(inspect.completed, "upload should complete on server side");
}

#[tokio::test]
async fn from_bytes_duplicate_semantics_match_file_source() {
    let payload = b"from-bytes-duplicate-check".repeat(256 * 1024);
    let server = dev_server::DevFileServer::spawn(Vec::new());
    let client = MeowClient::new(MeowConfig::new(1, 1));

    let second_statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let s2 = second_statuses.clone();

    let first = UploadPounceBuilder::from_bytes("dup-a.bin", payload.clone(), 1024)
        .with_url(format!("{}/upload/a.bin", server.base_url()))
        .build()
        .expect("build first bytes task");
    let second = UploadPounceBuilder::from_bytes("dup-b.bin", payload, 1024)
        .with_url(format!("{}/upload/b.bin", server.base_url()))
        .build()
        .expect("build second bytes task");

    client
        .try_enqueue(first, |_r: FileTransferRecord| {}, |_, _| {})
        .await
        .expect("enqueue first bytes task");
    client
        .try_enqueue(
            second,
            move |record: FileTransferRecord| {
                s2.lock()
                    .expect("lock statuses")
                    .push(record.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("enqueue second bytes task");

    let mut seen_duplicate = false;
    for _ in 0..300 {
        seen_duplicate = second_statuses.lock().expect("lock second statuses").iter().any(|s| {
            matches!(
                s,
                TransferStatus::Failed(err) if err.code() == InnerErrorCode::DuplicateTaskError as i32
            )
        });
        if seen_duplicate {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert!(
        seen_duplicate,
        "second from_bytes upload with same payload should be deduped by file_sign"
    );

    client.close().await.expect("close client");
    server.shutdown();
}

#[tokio::test]
async fn from_bytes_and_file_source_match_on_retry_and_callback_semantics() {
    let payload = b"from-bytes-vs-file-retry".repeat(32 * 1024);
    let file_path = temp_upload_path("retry_compare_file");
    fs::write(&file_path, &payload).expect("write upload source");

    let file_case = {
        let server = FlakyUploadServer::spawn_fail_on_second_post_once();
        let task = UploadPounceBuilder::new("retry-file.bin", &file_path, 64 * 1024 * 1024)
            .with_url(format!("{}/upload/retry-file.bin", server.base_url()))
            .build()
            .expect("build file retry task");
        let client = MeowClient::new(MeowConfig::new(1, 1));
        let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
        let statuses_cb = statuses.clone();
        let complete_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let complete_hits_cb = complete_hits.clone();

        client
            .try_enqueue(
                task,
                move |record: FileTransferRecord| {
                    statuses_cb
                        .lock()
                        .expect("lock statuses")
                        .push(record.status().clone());
                },
                move |_, _| {
                    complete_hits_cb.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                },
            )
            .await
            .expect("enqueue file retry case");

        let terminal = wait_terminal(statuses.clone(), 600).await;
        let snapshot = statuses.lock().expect("lock statuses final").clone();
        client.close().await.expect("close file retry client");
        server.shutdown();
        (
            terminal,
            snapshot,
            complete_hits.load(std::sync::atomic::Ordering::Relaxed),
        )
    };

    let bytes_case = {
        let server = FlakyUploadServer::spawn_fail_on_second_post_once();
        let task = UploadPounceBuilder::from_bytes("retry-bytes.bin", payload, 64 * 1024 * 1024)
            .with_url(format!("{}/upload/retry-bytes.bin", server.base_url()))
            .build()
            .expect("build bytes retry task");
        let client = MeowClient::new(MeowConfig::new(1, 1));
        let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
        let statuses_cb = statuses.clone();
        let complete_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let complete_hits_cb = complete_hits.clone();

        client
            .try_enqueue(
                task,
                move |record: FileTransferRecord| {
                    statuses_cb
                        .lock()
                        .expect("lock statuses")
                        .push(record.status().clone());
                },
                move |_, _| {
                    complete_hits_cb.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                },
            )
            .await
            .expect("enqueue bytes retry case");

        let terminal = wait_terminal(statuses.clone(), 600).await;
        let snapshot = statuses.lock().expect("lock statuses final").clone();
        client.close().await.expect("close bytes retry client");
        server.shutdown();
        (
            terminal,
            snapshot,
            complete_hits.load(std::sync::atomic::Ordering::Relaxed),
        )
    };

    assert!(matches!(file_case.0, TransferStatus::Complete));
    assert!(matches!(bytes_case.0, TransferStatus::Complete));
    assert_eq!(
        file_case.2, 1,
        "file-source complete callback should fire once"
    );
    assert_eq!(
        bytes_case.2, 1,
        "from_bytes complete callback should fire once"
    );

    assert!(
        file_case
            .1
            .iter()
            .any(|s| matches!(s, TransferStatus::Transmission)),
        "file-source case should emit Transmission"
    );
    assert!(
        bytes_case
            .1
            .iter()
            .any(|s| matches!(s, TransferStatus::Transmission)),
        "from_bytes case should emit Transmission"
    );

    fs::remove_file(&file_path).ok();
}
