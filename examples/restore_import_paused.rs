//! Example: restart/restore with paused import + selective start.
//!
//! Demonstrates [`rusty_cat::api::MeowClient::try_enqueue_paused`]. After an app
//! restart, rebuild your tasks from your own persistence, import them all in the
//! paused state (which performs **no network or file I/O**), then `resume()`
//! only the tasks the user chose to start. The rest stay paused until you
//! resume them later.
//!
//! Run with: `cargo run --example restore_import_paused`

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rusty_cat::api::{DownloadPounceBuilder, FileTransferRecord, MeowClient, MeowConfig};

type AnyResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn temp_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(name)
}

#[tokio::main]
async fn main() -> AnyResult<()> {
    // A small fixed payload the local server will serve for every download.
    let payload: Vec<u8> = (0..256 * 1024).map(|i| (i % 251) as u8).collect();
    let server = LocalServer::spawn(payload.clone())?;
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_download_concurrency(2)
            .build()?,
    );

    // Imagine these three tasks were rebuilt from your own database after a
    // restart. We import them ALL in the paused state first.
    let names = ["alpha", "beta", "gamma"];
    let mut ids = Vec::new();
    let mut paths = Vec::new();
    let mut done_flags = Vec::new();

    for name in names {
        let path = temp_path(&format!("rusty_cat_restore_{name}.bin"));
        let _ = std::fs::remove_file(&path);
        let done = Arc::new(AtomicBool::new(false));
        let done_cb = done.clone();

        let task = DownloadPounceBuilder::new(
            format!("{name}.bin"),
            &path,
            64 * 1024,
            format!("{}/download/{name}.bin", server.base_url()),
        )
        .build();

        // try_enqueue_paused registers the task as Paused without scheduling it:
        // no HEAD/GET request is sent and no file is created here.
        let id = client
            .try_enqueue_paused(
                task,
                |_r: FileTransferRecord| {},
                move |_id, _payload| done_cb.store(true, Ordering::SeqCst),
            )
            .await?;
        println!("imported '{name}' as paused (task id {id})");
        ids.push(id);
        paths.push(path);
        done_flags.push(done);
    }

    // Nothing is queued or active yet: the imports did no I/O.
    let snap = client.snapshot().await?;
    println!(
        "after import: queued={}, active={} (all paused, no I/O performed)",
        snap.queued_groups, snap.active_groups
    );

    // The user chose to start only "alpha" and "gamma"; "beta" stays paused.
    let selected = [0usize, 2usize];
    for &i in &selected {
        println!("resuming '{}' ...", names[i]);
        client.resume(ids[i]).await?;
    }

    // Wait for the selected tasks to finish (poll the completion flags).
    for &i in &selected {
        let mut finished = false;
        for _ in 0..600 {
            if done_flags[i].load(Ordering::SeqCst) {
                finished = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let size = std::fs::metadata(&paths[i]).map(|m| m.len()).unwrap_or(0);
        println!("'{}' completed={} ({} bytes on disk)", names[i], finished, size);
    }

    // "beta" was never resumed, so it is still paused and wrote no file.
    println!(
        "'{}' is still paused; file exists on disk: {}",
        names[1],
        paths[1].exists()
    );

    client.close().await?;
    drop(server);
    for p in &paths {
        let _ = std::fs::remove_file(p);
    }
    Ok(())
}

// ----------------- minimal local HEAD + Range GET server -----------------

struct LocalServer {
    base_url: String,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl LocalServer {
    fn spawn(payload: Vec<u8>) -> AnyResult<Self> {
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
                        let _ = handle_connection(&mut stream, &payload);
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

fn handle_connection(stream: &mut TcpStream, payload: &[u8]) -> std::io::Result<()> {
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
