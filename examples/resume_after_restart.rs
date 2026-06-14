//! Example: resume a download after a simulated process kill.
//!
//! This shows the easiest resume case: for downloads, the partially written file
//! on disk *is* the checkpoint. After a crash you simply rebuild the same
//! [`rusty_cat::api::DownloadPounceBuilder`] (pointing at the same `file_path`)
//! and call `try_enqueue` again; the SDK reads the on-disk length and continues
//! with a `Range` request from there.
//!
//! To keep the demo deterministic, instead of racing a real kill we *simulate*
//! the leftover of a previous run by writing the first 100 KiB of the object to
//! the target path, then resume it. The local server records the first byte
//! offset it is asked to serve, which proves the resume started above zero.
//!
//! Run with: `cargo run --example resume_after_restart`
//!
//! See `docs/resume-after-restart.md` for uploads and the full mental model.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rusty_cat::api::{DownloadPounceBuilder, FileTransferRecord, MeowClient, MeowConfig};

type AnyResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const TOTAL: usize = 256 * 1024;
const CHUNK: u64 = 64 * 1024;
const PRESEED: usize = 100 * 1024;

fn make_payload() -> Vec<u8> {
    (0..TOTAL).map(|i| (i % 251) as u8).collect()
}

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(name)
}

#[tokio::main]
async fn main() -> AnyResult<()> {
    let payload = make_payload();

    // The server records the smallest byte offset it is asked to serve. After a
    // resume this is the partial-file length, not zero.
    let resume_start = Arc::new(AtomicU64::new(u64::MAX));
    let server = LocalServer::spawn(payload.clone(), resume_start.clone())?;

    let path = temp_path("rusty_cat_resume_after_restart.bin");
    let _ = std::fs::remove_file(&path);

    // ---- Simulate a previous run that was killed after writing PRESEED bytes ----
    // In a real application the SDK already wrote these bytes to disk before the
    // process died. They MUST be the correct leading bytes of the object.
    std::fs::write(&path, &payload[..PRESEED])?;
    println!("simulated crashed run: {PRESEED} of {TOTAL} bytes already on disk",);

    // ---- "New process after restart": rebuild the SAME task and resume it ----
    let client = MeowClient::new(MeowConfig::builder().max_download_concurrency(1).build()?);

    let done = Arc::new(AtomicBool::new(false));
    let done_cb = done.clone();

    // Identical builder values to the original task: same file_name, file_path,
    // chunk_size and url. The file_path is what ties this run to the partial file.
    let task = DownloadPounceBuilder::new(
        "report.bin",
        &path,
        CHUNK,
        format!("{}/download/report.bin", server.base_url()),
    )
    .build();

    let _task_id = client
        .try_enqueue(
            task,
            |record: FileTransferRecord| {
                println!("resuming... {:.1}%", record.progress() * 100.0);
            },
            move |_id, _payload| done_cb.store(true, Ordering::SeqCst),
        )
        .await?;

    // Poll the completion flag (see the end-to-end README example for a channel
    // based alternative).
    let mut finished = false;
    for _ in 0..600 {
        if done.load(Ordering::SeqCst) {
            finished = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let on_disk = std::fs::read(&path)?;
    let observed_resume = resume_start.load(Ordering::SeqCst);

    println!("completed = {finished}");
    println!("server first served from byte offset = {observed_resume} (0 would mean no resume)");
    println!(
        "final size on disk = {} bytes (expected {TOTAL})",
        on_disk.len()
    );
    println!("byte-exact match = {}", on_disk == payload);

    assert!(finished, "download did not finish in time");
    assert_eq!(
        on_disk, payload,
        "resumed file does not match the original object"
    );
    assert_eq!(
        observed_resume, PRESEED as u64,
        "expected the resume to start from the partial-file length",
    );

    client.close().await?;
    drop(server);
    let _ = std::fs::remove_file(&path);
    println!("OK: the download resumed from the partial file and finished correctly.");
    Ok(())
}

// ----------------- minimal local HEAD + Range GET server -----------------

struct LocalServer {
    base_url: String,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl LocalServer {
    fn spawn(payload: Vec<u8>, resume_start: Arc<AtomicU64>) -> AnyResult<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let payload = Arc::new(payload);
        let handle = thread::spawn(move || {
            while !stop_thread.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        let _ = handle_connection(&mut stream, &payload, &resume_start);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            base_url: format!("http://{addr}"),
            stop,
            handle: Some(handle),
        })
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }
}

impl Drop for LocalServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_connection(
    stream: &mut TcpStream,
    payload: &[u8],
    resume_start: &AtomicU64,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let request = read_request(stream);
    let first_line = request.lines().next().unwrap_or_default();

    if first_line.starts_with("HEAD") {
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len()
        );
        stream.write_all(resp.as_bytes())?;
    } else if first_line.starts_with("GET") {
        let (start, end) = parse_range(&request, payload.len());
        // Track the lowest offset requested: after a resume this is the
        // partial-file length, never zero.
        resume_start.fetch_min(start as u64, Ordering::SeqCst);
        let body = &payload[start..=end];
        let resp = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {}-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            start,
            end,
            payload.len(),
            body.len()
        );
        stream.write_all(resp.as_bytes())?;
        stream.write_all(body)?;
    } else {
        stream.write_all(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )?;
    }
    stream.flush()
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                // Requests here carry no body, so the blank line ends them.
                if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 16 * 1024 {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn parse_range(request: &str, total: usize) -> (usize, usize) {
    let last = total.saturating_sub(1);
    for line in request.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("range:") {
            if let Some(spec) = rest.trim().strip_prefix("bytes=") {
                if let Some((start_s, end_s)) = spec.split_once('-') {
                    let start = start_s.trim().parse::<usize>().unwrap_or(0).min(last);
                    let end = if end_s.trim().is_empty() {
                        last
                    } else {
                        end_s.trim().parse::<usize>().unwrap_or(last).min(last)
                    };
                    return (start, end.max(start));
                }
            }
        }
    }
    (0, last)
}
