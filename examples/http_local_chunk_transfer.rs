mod support;

use std::sync::Arc;

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::Method;
use rusty_cat::api::{
    DownloadPounceBuilder, MeowClient, UploadChunkCtx, UploadPounceBuilder, UploadPrepareCtx,
    UploadResumeInfo,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, Mutex};

use support::{
    config, make_file, remove_if_exists, run_task, temp_path, AnyResult, FIVE_MB, ONE_MB,
};

#[derive(Clone)]
struct BinaryUpload {
    url: String,
}

#[async_trait]
impl rusty_cat::api::BreakpointUpload for BinaryUpload {
    async fn prepare(
        &self,
        _ctx: UploadPrepareCtx<'_>,
    ) -> Result<UploadResumeInfo, rusty_cat::api::MeowError> {
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(0),
            provider_upload_id: None,
        })
    }

    async fn upload_chunk(
        &self,
        ctx: UploadChunkCtx<'_>,
    ) -> Result<UploadResumeInfo, rusty_cat::api::MeowError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-offset",
            HeaderValue::from_str(&ctx.offset.to_string()).unwrap(),
        );
        let resp = ctx
            .client
            .put(&self.url)
            .headers(headers)
            .body(reqwest::Body::from(ctx.chunk.clone()))
            .send()
            .await
            .map_err(|e| {
                rusty_cat::api::MeowError::from_source(
                    rusty_cat::api::InnerErrorCode::HttpError,
                    e.to_string(),
                    e,
                )
            })?;
        if !resp.status().is_success() {
            return Err(rusty_cat::api::MeowError::from_code(
                rusty_cat::api::InnerErrorCode::ResponseStatusError,
                format!("upload status {}", resp.status()),
            ));
        }
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(ctx.offset + ctx.chunk.len() as u64),
            provider_upload_id: None,
        })
    }
}

struct LocalServer {
    addr: String,
    shutdown: Option<oneshot::Sender<()>>,
}

impl Drop for LocalServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

async fn start_server(data: Vec<u8>) -> AnyResult<LocalServer> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let download = Arc::new(data);
    let upload = Arc::new(Mutex::new(vec![0_u8; FIVE_MB]));
    let (tx, mut rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut rx => break,
                accepted = listener.accept() => {
                    if let Ok((stream, _)) = accepted {
                        let download = Arc::clone(&download);
                        let upload = Arc::clone(&upload);
                        tokio::spawn(async move {
                            let _ = handle_connection(stream, download, upload).await;
                        });
                    }
                }
            }
        }
    });
    Ok(LocalServer {
        addr: format!("http://{}", addr),
        shutdown: Some(tx),
    })
}

async fn handle_connection(
    mut stream: TcpStream,
    download: Arc<Vec<u8>>,
    upload: Arc<Mutex<Vec<u8>>>,
) -> AnyResult<()> {
    let mut buf = Vec::new();
    let mut tmp = [0_u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_header_end(&buf) {
            break pos;
        }
    };
    let header = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = header.lines();
    let request = lines.next().unwrap_or_default().to_string();
    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let content_len = header_value(&headers, "content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < content_len {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    let parts: Vec<&str> = request.split_whitespace().collect();
    let method = parts.first().copied().unwrap_or_default();
    let path = parts.get(1).copied().unwrap_or_default();
    match (method, path) {
        ("HEAD", "/file") => {
            write_response(
                &mut stream,
                "200 OK",
                &[("Content-Length", download.len().to_string())],
                &[],
            )
            .await?;
        }
        ("GET", "/file") => {
            let range = header_value(&headers, "range").unwrap_or_else(|| "bytes=0-".to_string());
            let (start, end) = parse_range(&range, download.len() as u64);
            let bytes = &download[start as usize..=end as usize];
            write_response(
                &mut stream,
                "206 Partial Content",
                &[
                    ("Content-Length", bytes.len().to_string()),
                    (
                        "Content-Range",
                        format!("bytes {}-{}/{}", start, end, download.len()),
                    ),
                ],
                bytes,
            )
            .await?;
        }
        ("PUT", "/upload") => {
            let offset = header_value(&headers, "x-offset")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(0);
            let mut target = upload.lock().await;
            let end = offset + body.len();
            if end <= target.len() {
                target[offset..end].copy_from_slice(&body);
                write_response(
                    &mut stream,
                    "200 OK",
                    &[("Content-Length", "0".to_string())],
                    &[],
                )
                .await?;
            } else {
                write_response(
                    &mut stream,
                    "416 Range Not Satisfiable",
                    &[("Content-Length", "0".to_string())],
                    &[],
                )
                .await?;
            }
        }
        _ => {
            write_response(
                &mut stream,
                "404 Not Found",
                &[("Content-Length", "0".to_string())],
                &[],
            )
            .await?;
        }
    }
    Ok(())
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn header_value(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.clone())
}

fn parse_range(value: &str, total: u64) -> (u64, u64) {
    let spec = value.strip_prefix("bytes=").unwrap_or("0-");
    let (start, end) = spec.split_once('-').unwrap_or(("0", ""));
    let start = start
        .parse::<u64>()
        .unwrap_or(0)
        .min(total.saturating_sub(1));
    let end = if end.is_empty() {
        total.saturating_sub(1)
    } else {
        end.parse::<u64>().unwrap_or(total.saturating_sub(1))
    }
    .min(total.saturating_sub(1));
    (start, end)
}

async fn write_response(
    stream: &mut TcpStream,
    status: &str,
    headers: &[(&str, String)],
    body: &[u8],
) -> AnyResult<()> {
    let mut response = format!("HTTP/1.1 {status}\r\nConnection: close\r\n");
    for (k, v) in headers {
        response.push_str(k);
        response.push_str(": ");
        response.push_str(v);
        response.push_str("\r\n");
    }
    response.push_str("\r\n");
    stream.write_all(response.as_bytes()).await?;
    if !body.is_empty() {
        stream.write_all(body).await?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> AnyResult<()> {
    let upload_path = temp_path("rusty_cat_http_upload_5mb.bin");
    let download_path = temp_path("rusty_cat_http_download_5mb.bin");
    remove_if_exists(&upload_path)?;
    remove_if_exists(&download_path)?;
    make_file(&upload_path, FIVE_MB)?;
    let server = start_server(std::fs::read(&upload_path)?).await?;
    let client = MeowClient::new(config()?);
    let upload_protocol = BinaryUpload {
        url: format!("{}/upload", server.addr),
    };
    let upload_task = UploadPounceBuilder::new("http-upload.bin", &upload_path, ONE_MB)
        .with_url(format!("{}/upload", server.addr))
        .with_method(Method::PUT)
        .with_breakpoint_upload(Arc::new(upload_protocol))
        .build()?;
    run_task(&client, upload_task, "http upload").await?;
    let download_task = DownloadPounceBuilder::new(
        "http-download.bin",
        &download_path,
        ONE_MB,
        format!("{}/file", server.addr),
    )
    .build();
    run_task(&client, download_task, "http download").await?;
    client.close().await?;
    drop(server);
    remove_if_exists(&upload_path)?;
    remove_if_exists(&download_path)?;
    Ok(())
}
