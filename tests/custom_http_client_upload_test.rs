use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{HeaderMap, HeaderValue};
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
    p.push(format!("rusty_cat_custom_upload_client_{case}_{ts}.bin"));
    p
}

async fn wait_terminal(statuses: Arc<Mutex<Vec<TransferStatus>>>) -> TransferStatus {
    for _ in 0..400 {
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
    panic!("did not observe terminal status");
}

#[derive(Default)]
struct HeaderAwareState {
    prepare_calls: usize,
    chunk_calls: usize,
    observed_header_count: usize,
}

struct HeaderAwareUploadServer {
    base_url: String,
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<HeaderAwareState>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl HeaderAwareUploadServer {
    fn spawn(required_header: &'static str, required_value: &'static str) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind header server");
        let addr = listener.local_addr().expect("read local addr");
        listener.set_nonblocking(true).expect("set nonblocking");

        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = stop.clone();
        let state = Arc::new(Mutex::new(HeaderAwareState::default()));
        let state_for_thread = state.clone();

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
                let (request_line, headers, body) = match read_http_request(&mut stream) {
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

                let has_required = headers.iter().any(|(k, v)| {
                    k.eq_ignore_ascii_case(required_header)
                        && v.eq_ignore_ascii_case(required_value)
                });

                if has_required {
                    state_for_thread
                        .lock()
                        .expect("lock state")
                        .observed_header_count += 1;
                }

                let body_str = String::from_utf8_lossy(&body);
                let is_prepare = !body_str.contains("name=\"offset\"")
                    || !body_str.contains("name=\"partSize\"");
                if is_prepare {
                    state_for_thread.lock().expect("lock state").prepare_calls += 1;
                    let body = b"{\"nextByte\":0}";
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(resp.as_bytes());
                    let _ = stream.write_all(body);
                    let _ = stream.flush();
                    continue;
                }

                state_for_thread.lock().expect("lock state").chunk_calls += 1;
                let body = b"{\"nextByte\":999999999}";
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
            state,
            handle: Some(handle),
        }
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn snapshot(&self) -> HeaderAwareState {
        let s = self.state.lock().expect("lock state");
        HeaderAwareState {
            prepare_calls: s.prepare_calls,
            chunk_calls: s.chunk_calls,
            observed_header_count: s.observed_header_count,
        }
    }

    fn shutdown(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            handle.join().expect("join header server");
        }
    }
}

impl Drop for HeaderAwareUploadServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[tokio::test]
async fn custom_reqwest_client_is_used_on_upload_path() {
    let required_header = "x-rusty-custom-upload-client";
    let required_value = "enabled";

    let server = HeaderAwareUploadServer::spawn(required_header, required_value);
    let upload_path = temp_upload_path("header_required");
    fs::write(&upload_path, b"custom-client-upload-payload".repeat(2048))
        .expect("write upload fixture");

    let mut headers = HeaderMap::new();
    headers.insert(required_header, HeaderValue::from_static(required_value));
    let custom_http = reqwest::Client::builder()
        .default_headers(headers)
        .user_agent("rusty-cat-custom-upload-client-test/1.0")
        .timeout(Duration::from_secs(10))
        .build()
        .expect("build custom http client");

    let client = MeowClient::new(
        MeowConfig::new(1, 1)
            .with_http_client(custom_http)
            .with_http_timeout(Duration::from_secs(10)),
    );

    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();

    let task = UploadPounceBuilder::new("custom.bin", &upload_path, 1024 * 1024)
        .with_url(format!("{}/upload/custom.bin", server.base_url()))
        .build()
        .expect("build upload task");

    client
        .try_enqueue(
            task,
            move |record: FileTransferRecord| {
                statuses_cb
                    .lock()
                    .expect("lock statuses")
                    .push(record.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("enqueue custom upload task");

    let terminal = wait_terminal(statuses.clone()).await;
    assert!(
        matches!(terminal, TransferStatus::Complete),
        "upload with custom http client should complete, got terminal={terminal:?}, statuses={:?}",
        statuses.lock().expect("lock statuses for assert")
    );

    client.close().await.expect("close client");

    let inspect = server.snapshot();
    server.shutdown();

    assert!(inspect.prepare_calls >= 1, "prepare should be called");
    assert!(inspect.chunk_calls >= 1, "chunk should be called");
    assert!(
        inspect.observed_header_count >= 1,
        "required custom header should be observed on at least one upload request"
    );

    let _ = fs::remove_file(&upload_path);
}
