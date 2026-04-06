#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Debug, Default, Clone)]
pub struct UploadInspector {
    pub prepare_calls: usize,
    pub chunk_calls: usize,
    pub max_next_byte: u64,
    pub total_size_seen: u64,
    pub completed: bool,
}

pub struct DevFileServer {
    base_url: String,
    stop: Arc<AtomicBool>,
    #[allow(dead_code)]
    upload_inspector: Arc<Mutex<UploadInspector>>,
    handle: Option<thread::JoinHandle<()>>,
}

pub struct ScriptedServer {
    base_url: String,
    handle: Option<thread::JoinHandle<()>>,
}

pub struct FlakyDownloadServer {
    base_url: String,
    handle: Option<thread::JoinHandle<()>>,
}

impl DevFileServer {
    pub fn spawn(download_payload: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local dev file server");
        let addr = listener
            .local_addr()
            .expect("read local dev file server addr");
        listener
            .set_nonblocking(true)
            .expect("set dev file server nonblocking");

        let payload = Arc::new(download_payload);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = stop.clone();
        let inspector = Arc::new(Mutex::new(UploadInspector::default()));
        let inspector_for_thread = inspector.clone();

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

                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .expect("set dev server read timeout");
                if let Err(e) = handle_one_connection(&mut stream, &payload, &inspector_for_thread)
                {
                    let _ = stream.write_all(
                        format!(
                            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    );
                    let _ = stream.flush();
                    eprintln!("dev file server connection error: {e}");
                }
            }
        });

        Self {
            base_url: format!("http://{addr}"),
            stop,
            upload_inspector: inspector,
            handle: Some(handle),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    #[allow(dead_code)]
    pub fn upload_inspector(&self) -> UploadInspector {
        self.upload_inspector
            .lock()
            .expect("lock upload inspector")
            .clone()
    }

    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            handle.join().expect("join dev file server thread");
        }
    }
}

impl Drop for DevFileServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl ScriptedServer {
    pub fn spawn_download(responses: Vec<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind scripted server");
        let addr = listener.local_addr().expect("read scripted server addr");
        let handle = thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = match listener.accept() {
                    Ok(v) => v,
                    Err(_) => break,
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .expect("set scripted server read timeout");
                let _ = read_http_request(&mut stream);
                if stream.write_all(response.as_bytes()).is_err() {
                    break;
                }
                let _ = stream.flush();
            }
        });
        Self {
            base_url: format!("http://{addr}"),
            handle: Some(handle),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn shutdown(mut self) {
        if let Some(handle) = self.handle.take() {
            handle.join().expect("join scripted server thread");
        }
    }
}

impl Drop for ScriptedServer {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl FlakyDownloadServer {
    pub fn spawn_one_chunk(fail_get_times: usize, chunk: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind flaky download server");
        let addr = listener.local_addr().expect("read flaky server addr");
        listener
            .set_nonblocking(true)
            .expect("set flaky server nonblocking");
        let handle = thread::spawn(move || {
            let mut get_count = 0usize;
            let mut head_seen = false;
            let mut idle_ticks = 0u32;
            let total = chunk.len() as u64;
            loop {
                let accepted = listener.accept();
                let (mut stream, _) = match accepted {
                    Ok(v) => {
                        idle_ticks = 0;
                        v
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        idle_ticks += 1;
                        if head_seen && get_count >= fail_get_times && idle_ticks > 200 {
                            break;
                        }
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => break,
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .expect("set flaky server read timeout");
                let (request_line, _, _) = match read_http_request(&mut stream) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if request_line.starts_with("HEAD ") {
                    head_seen = true;
                    let _ = write_http_response(
                        &mut stream,
                        "200 OK",
                        vec![("Content-Length".to_string(), total.to_string())],
                        &[],
                    );
                    continue;
                }
                if get_count < fail_get_times {
                    get_count += 1;
                    continue;
                }
                let end = total.saturating_sub(1);
                let _ = write_http_response(
                    &mut stream,
                    "206 Partial Content",
                    vec![
                        (
                            "Content-Range".to_string(),
                            format!("bytes 0-{end}/{total}"),
                        ),
                        ("Content-Length".to_string(), chunk.len().to_string()),
                    ],
                    &chunk,
                );
                break;
            }
        });
        Self {
            base_url: format!("http://{addr}"),
            handle: Some(handle),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn shutdown(mut self) {
        if let Some(handle) = self.handle.take() {
            handle.join().expect("join flaky server thread");
        }
    }
}

impl Drop for FlakyDownloadServer {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_one_connection(
    stream: &mut std::net::TcpStream,
    payload: &Arc<Vec<u8>>,
    inspector: &Arc<Mutex<UploadInspector>>,
) -> Result<(), String> {
    let (request_line, headers, body) = read_http_request(stream).map_err(|e| e.to_string())?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let path = parts.next().unwrap_or_default();

    match (method, path) {
        ("HEAD", p) if p.starts_with("/download") => {
            write_http_response(
                stream,
                "200 OK",
                vec![("Content-Length".to_string(), payload.len().to_string())],
                &[],
            )
            .map_err(|e| e.to_string())?;
        }
        ("GET", p) if p.starts_with("/download") => {
            let range_header = headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("Range"))
                .map(|(_, v)| v.clone())
                .ok_or_else(|| "missing Range header".to_string())?;
            let (start, end) = parse_range_header(&range_header, payload.len() as u64)?;
            if end < start || end as usize >= payload.len() {
                write_http_response(
                    stream,
                    "416 Range Not Satisfiable",
                    vec![("Content-Length".to_string(), "0".to_string())],
                    &[],
                )
                .map_err(|e| e.to_string())?;
                return Ok(());
            }
            let chunk = &payload[start as usize..=end as usize];
            write_http_response(
                stream,
                "206 Partial Content",
                vec![
                    (
                        "Content-Range".to_string(),
                        format!("bytes {}-{}/{}", start, end, payload.len()),
                    ),
                    ("Content-Length".to_string(), chunk.len().to_string()),
                ],
                chunk,
            )
            .map_err(|e| e.to_string())?;
        }
        ("POST", p) if p.starts_with("/upload") => {
            let mut guard = inspector.lock().expect("lock upload inspector in server");
            let offset = parse_multipart_u64_field(&body, "offset");
            let part_size = parse_multipart_u64_field(&body, "partSize");
            let total_size = parse_multipart_u64_field(&body, "totalSize").unwrap_or(0);

            guard.total_size_seen = guard.total_size_seen.max(total_size);
            if offset.is_none() || part_size.is_none() {
                guard.prepare_calls += 1;
                write_http_response(
                    stream,
                    "200 OK",
                    vec![("Content-Type".to_string(), "application/json".to_string())],
                    br#"{"nextByte":0}"#,
                )
                .map_err(|e| e.to_string())?;
                return Ok(());
            }

            guard.chunk_calls += 1;
            let offset = offset.unwrap_or(0);
            let part_size = part_size.unwrap_or(0);
            let mut next = offset.saturating_add(part_size);
            if total_size > 0 {
                next = next.min(total_size);
            }
            guard.max_next_byte = guard.max_next_byte.max(next);
            let done = total_size > 0 && next >= total_size;
            guard.completed = guard.completed || done;

            let body = if done {
                format!(r#"{{"fileId":"dev-file-id","nextByte":{next}}}"#)
            } else {
                format!(r#"{{"nextByte":{next}}}"#)
            };
            write_http_response(
                stream,
                "200 OK",
                vec![("Content-Type".to_string(), "application/json".to_string())],
                body.as_bytes(),
            )
            .map_err(|e| e.to_string())?;
        }
        _ => {
            write_http_response(
                stream,
                "404 Not Found",
                vec![("Content-Length".to_string(), "0".to_string())],
                &[],
            )
            .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
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

fn write_http_response(
    stream: &mut std::net::TcpStream,
    status: &str,
    mut headers: Vec<(String, String)>,
    body: &[u8],
) -> std::io::Result<()> {
    if !headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("Content-Length"))
    {
        headers.push(("Content-Length".to_string(), body.len().to_string()));
    }
    if !headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("Connection"))
    {
        headers.push(("Connection".to_string(), "close".to_string()));
    }
    let mut resp = format!("HTTP/1.1 {status}\r\n");
    for (k, v) in headers {
        resp.push_str(&format!("{k}: {v}\r\n"));
    }
    resp.push_str("\r\n");
    stream.write_all(resp.as_bytes())?;
    if !body.is_empty() {
        stream.write_all(body)?;
    }
    stream.flush()?;
    Ok(())
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_range_header(value: &str, total: u64) -> Result<(u64, u64), String> {
    let spec = value
        .strip_prefix("bytes=")
        .ok_or_else(|| format!("invalid range prefix: {value}"))?;
    let (start_s, end_s) = spec
        .split_once('-')
        .ok_or_else(|| format!("invalid range format: {value}"))?;
    let start = start_s
        .trim()
        .parse::<u64>()
        .map_err(|_| format!("invalid range start: {value}"))?;
    let end = if end_s.trim().is_empty() {
        total.saturating_sub(1)
    } else {
        end_s
            .trim()
            .parse::<u64>()
            .map_err(|_| format!("invalid range end: {value}"))?
    };
    Ok((start, end))
}

fn parse_multipart_u64_field(body: &[u8], field_name: &str) -> Option<u64> {
    let raw = String::from_utf8_lossy(body);
    let marker = format!("name=\"{field_name}\"");
    let start = raw.find(&marker)?;
    let after_marker = &raw[start + marker.len()..];
    let value_block = after_marker.split("\r\n\r\n").nth(1)?;
    let value = value_block.split("\r\n").next()?.trim();
    value.parse::<u64>().ok()
}
