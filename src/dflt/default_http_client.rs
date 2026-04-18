use async_trait::async_trait;
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, ETAG};
use reqwest::{Client, Method};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::chunk_outcome::ChunkOutcome;
use crate::direction::Direction;
use crate::error::{InnerErrorCode, MeowError};
use crate::http_breakpoint::{
    BreakpointDownload, BreakpointUpload, DefaultStyleUpload, DownloadHeadCtx, DownloadRangeGetCtx,
    StandardRangeDownload, UploadChunkCtx, UploadPrepareCtx,
};
use crate::prepare_outcome::PrepareOutcome;
use crate::transfer_executor_trait::TransferTrait;
use crate::transfer_task::TransferTask;

/// Creates default breakpoint protocol instances.
pub(crate) fn default_breakpoint_arcs() -> (
    Arc<dyn BreakpointUpload + Send + Sync>,
    Arc<dyn BreakpointDownload + Send + Sync>,
) {
    (
        Arc::new(DefaultStyleUpload::default()),
        Arc::new(StandardRangeDownload::default()),
    )
}

/// Built-in transfer backend based on `reqwest` and async file I/O.
pub struct DefaultHttpClient {
    /// Default shared HTTP client.
    client: reqwest::Client,
    /// Fallback upload protocol when task does not provide one.
    fallback_upload: Arc<dyn BreakpointUpload + Send + Sync>,
    /// Fallback download protocol when task does not provide one.
    fallback_download: Arc<dyn BreakpointDownload + Send + Sync>,
}

impl DefaultHttpClient {
    /// Creates a client with default HTTP timeout and keepalive values.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use rusty_cat::dflt::default_http_client::DefaultHttpClient;
    ///
    /// let client = DefaultHttpClient::new();
    /// let _ = client;
    /// ```
    pub fn new() -> Self {
        Self::with_http_timeouts(Duration::from_secs(5), Duration::from_secs(30))
    }

    /// Creates built-in client with explicit timeout and keepalive values.
    ///
    /// # Range guidance
    ///
    /// - `http_timeout`: recommended `1s..=120s`
    /// - `tcp_keepalive`: recommended `10s..=300s`
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use std::time::Duration;
    /// use rusty_cat::dflt::default_http_client::DefaultHttpClient;
    ///
    /// let client = DefaultHttpClient::with_http_timeouts(
    ///     Duration::from_secs(15),
    ///     Duration::from_secs(60),
    /// );
    /// let _ = client;
    /// ```
    pub fn with_http_timeouts(http_timeout: Duration, tcp_keepalive: Duration) -> Self {
        // Keep non-fallible constructor for compatibility.
        // Prefer `try_with_http_timeouts` in new code for explicit errors.
        let client = match Client::builder()
            .timeout(http_timeout)
            .tcp_keepalive(tcp_keepalive)
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                crate::meow_flow_log!(
                    "http_client",
                    "with_http_timeouts build failed, fallback to Client::new(): {}",
                    e
                );
                Client::new()
            }
        };
        Self {
            client,
            fallback_upload: Arc::new(DefaultStyleUpload::default()),
            fallback_download: Arc::new(StandardRangeDownload::default()),
        }
    }

    /// Preferred fallible constructor with explicit error propagation.
    ///
    /// # Errors
    ///
    /// Returns `HttpClientBuildFailed` when `reqwest::Client` cannot be
    /// constructed with the provided timeout/keepalive values.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use std::time::Duration;
    /// use rusty_cat::dflt::default_http_client::DefaultHttpClient;
    ///
    /// let client = DefaultHttpClient::try_with_http_timeouts(
    ///     Duration::from_secs(10),
    ///     Duration::from_secs(30),
    /// )?;
    /// let _ = client;
    /// # Ok::<(), rusty_cat::api::MeowError>(())
    /// ```
    pub fn try_with_http_timeouts(
        http_timeout: Duration,
        tcp_keepalive: Duration,
    ) -> Result<Self, MeowError> {
        let client = Client::builder()
            .timeout(http_timeout)
            .tcp_keepalive(tcp_keepalive)
            .build()
            .map_err(|e| {
                MeowError::from_source(
                    InnerErrorCode::HttpClientBuildFailed,
                    format!(
                        "build reqwest client failed (timeout={:?}, keepalive={:?})",
                        http_timeout, tcp_keepalive
                    ),
                    e,
                )
            })?;
        Ok(Self {
            client,
            fallback_upload: Arc::new(DefaultStyleUpload::default()),
            fallback_download: Arc::new(StandardRangeDownload::default()),
        })
    }

    /// Creates backend with an externally provided `reqwest::Client`.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use rusty_cat::dflt::default_http_client::DefaultHttpClient;
    ///
    /// let reqwest_client = reqwest::Client::new();
    /// let backend = DefaultHttpClient::with_client(reqwest_client);
    /// let _ = backend;
    /// ```
    pub fn with_client(client: reqwest::Client) -> Self {
        Self {
            client,
            fallback_upload: Arc::new(DefaultStyleUpload::default()),
            fallback_download: Arc::new(StandardRangeDownload::default()),
        }
    }

    /// Creates backend with explicit fallback upload/download protocol plugins.
    ///
    /// Task-level protocol instances still take precedence when present.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use std::sync::Arc;
    /// use rusty_cat::api::{DefaultStyleUpload, StandardRangeDownload};
    /// use rusty_cat::dflt::default_http_client::DefaultHttpClient;
    ///
    /// let backend = DefaultHttpClient::with_fallbacks(
    ///     reqwest::Client::new(),
    ///     Arc::new(DefaultStyleUpload::default()),
    ///     Arc::new(StandardRangeDownload),
    /// );
    /// let _ = backend;
    /// ```
    pub fn with_fallbacks(
        client: reqwest::Client,
        upload: Arc<dyn BreakpointUpload + Send + Sync>,
        download: Arc<dyn BreakpointDownload + Send + Sync>,
    ) -> Self {
        Self {
            client,
            fallback_upload: upload,
            fallback_download: download,
        }
    }

    /// Selects HTTP client for a task.
    fn client_for(&self, task: &TransferTask) -> reqwest::Client {
        task.http_client_ref()
            .cloned()
            .unwrap_or_else(|| self.client.clone())
    }

    /// Selects upload protocol implementation for a task.
    fn upload_arc(&self, task: &TransferTask) -> Arc<dyn BreakpointUpload + Send + Sync> {
        match task.breakpoint_upload() {
            Some(a) => a.clone(),
            None => self.fallback_upload.clone(),
        }
    }

    /// Selects download protocol implementation for a task.
    fn download_arc(&self, task: &TransferTask) -> Arc<dyn BreakpointDownload + Send + Sync> {
        match task.breakpoint_download() {
            Some(a) => a.clone(),
            None => self.fallback_download.clone(),
        }
    }
}

impl Default for DefaultHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

async fn upload_prepare(
    client: &reqwest::Client,
    task: &TransferTask,
    upload: Arc<dyn BreakpointUpload + Send + Sync>,
    local_offset: u64,
) -> Result<PrepareOutcome, MeowError> {
    crate::meow_flow_log!(
        "upload_prepare",
        "start: file={} local_offset={} total={}",
        task.file_name(),
        local_offset,
        task.total_size()
    );
    let info = upload
        .prepare(UploadPrepareCtx {
            client,
            task,
            local_offset,
        })
        .await?;
    if info.completed_file_id.is_some() {
        let total = task.total_size();
        crate::meow_flow_log!(
            "upload_prepare",
            "server indicates upload already complete: file={} total={}",
            task.file_name(),
            total
        );
        return Ok(PrepareOutcome {
            next_offset: total,
            total_size: total,
        });
    }
    let server_off = info.next_byte.unwrap_or(0);
    let next = local_offset.max(server_off).min(task.total_size());
    crate::meow_flow_log!(
        "upload_prepare",
        "prepared: server_next={} local_offset={} final_next={}",
        server_off,
        local_offset,
        next
    );
    Ok(PrepareOutcome {
        next_offset: next,
        total_size: task.total_size(),
    })
}

/// Runs download prepare stage and computes resume offset/total size.
async fn download_prepare(
    client: &reqwest::Client,
    task: &TransferTask,
    download: Arc<dyn BreakpointDownload + Send + Sync>,
    _local_offset: u64,
) -> Result<PrepareOutcome, MeowError> {
    crate::meow_flow_log!(
        "download_prepare",
        "start: file={} path={}",
        task.file_name(),
        task.file_path().display()
    );
    let path = task.file_path();
    let local_len = match tokio::fs::metadata(path).await {
        Ok(meta) => meta.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0u64,
        Err(e) => {
            return Err(MeowError::from_source(
                InnerErrorCode::IoError,
                format!("download_prepare stat failed: {}", path.display()),
                e,
            ));
        }
    };

    // Use local persisted length as resume start to avoid sparse gaps.
    let start = local_len;
    let head_url = download.head_url(task);
    let mut head_headers = task.headers().clone();
    download.merge_head_headers(DownloadHeadCtx {
        task,
        base: &mut head_headers,
    })?;
    let head_resp = client
        .request(Method::HEAD, &head_url)
        .headers(head_headers)
        .send()
        .await
        .map_err(map_reqwest)?;
    if !head_resp.status().is_success() {
        crate::meow_flow_log!(
            "download_prepare",
            "head failed: status={}",
            head_resp.status()
        );
        return Err(MeowError::from_code(
            InnerErrorCode::ResponseStatusError,
            format!("download_prepare HEAD failed: {}", head_resp.status()),
        ));
    }
    let head_content_length = head_resp
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("<missing>");
    let head_etag = head_resp
        .headers()
        .get(ETAG)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("<missing>");
    println!(
        "[download-head] url={} content-length={} etag={}",
        head_url, head_content_length, head_etag
    );
    crate::meow_flow_log!(
        "download_prepare",
        "head metadata: url={} content_length={} etag={}",
        head_url,
        head_content_length,
        head_etag
    );
    let total = download.total_size_from_head(head_resp.headers())?;
    if start > total {
        crate::meow_flow_log!(
            "download_prepare",
            "invalid local length larger than remote: local={} remote={}",
            start,
            total
        );
        return Err(MeowError::from_code_str(
            InnerErrorCode::InvalidRange,
            "local file larger than remote content-length",
        ));
    }
    if start >= total {
        crate::meow_flow_log!(
            "download_prepare",
            "already complete by local length: local={} remote={}",
            start,
            total
        );
        return Ok(PrepareOutcome {
            next_offset: total,
            total_size: total,
        });
    }
    crate::meow_flow_log!(
        "download_prepare",
        "prepared resume offset: start={} remote_total={}",
        start,
        total
    );
    Ok(PrepareOutcome {
        next_offset: start,
        total_size: total,
    })
}

/// Uploads exactly one chunk and returns next transfer outcome.
async fn upload_one_chunk(
    client: &reqwest::Client,
    task: &TransferTask,
    upload: Arc<dyn BreakpointUpload + Send + Sync>,
    offset: u64,
    chunk_size: u64,
) -> Result<ChunkOutcome, MeowError> {
    let total = task.total_size();
    if offset >= total {
        return Ok(ChunkOutcome {
            next_offset: offset,
            total_size: total,
            done: true,
            completion_payload: None,
        });
    }
    let read_len = chunk_size.min(total - offset);
    if read_len == 0 {
        return Ok(ChunkOutcome {
            next_offset: offset,
            total_size: total,
            done: true,
            completion_payload: None,
        });
    }

    let path = task.file_path();
    let mut slot = task.upload_file_slot().lock().await;
    if slot.is_none() {
        let opened = File::open(path).await.map_err(|e| {
            MeowError::from_source(
                InnerErrorCode::IoError,
                format!("open upload source failed: {}", path.display()),
                e,
            )
        })?;
        *slot = Some(opened);
    }
    let file = slot.as_mut().ok_or_else(|| {
        MeowError::from_code_str(
            InnerErrorCode::IoError,
            "upload file slot unexpectedly empty after open",
        )
    })?;
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(|e| {
            MeowError::from_source(
                InnerErrorCode::IoError,
                format!(
                    "seek upload source failed: offset={offset} path={}",
                    path.display()
                ),
                e,
            )
        })?;
    let mut buf = vec![0u8; read_len as usize];
    file.read_exact(&mut buf).await.map_err(|e| {
        MeowError::from_source(
            InnerErrorCode::IoError,
            format!("read upload source failed: path={}", path.display()),
            e,
        )
    })?;
    drop(slot);

    let info = upload
        .upload_chunk(UploadChunkCtx {
            client,
            task,
            chunk: &buf,
            offset,
        })
        .await?;
    if info.completed_file_id.is_some() {
        return Ok(ChunkOutcome {
            next_offset: total,
            total_size: total,
            done: true,
            completion_payload: info.completed_file_id,
        });
    }
    let next = info.next_byte.unwrap_or(offset + buf.len() as u64).min(total);
    let mut completion_payload = None;
    if next >= total {
        completion_payload = upload.complete_upload(client, task).await?;
    }
    Ok(ChunkOutcome {
        next_offset: next,
        total_size: total,
        done: next >= total,
        completion_payload,
    })
}

/// Builds `Range` header value from start/chunk-size/total-size.
fn range_spec(start: u64, chunk_size: u64, total: u64) -> String {
    if total == 0 {
        return format!("bytes={start}-");
    }
    let end = (start + chunk_size - 1).min(total.saturating_sub(1));
    format!("bytes={start}-{end}")
}

/// Parses `Content-Range` header into `(start, end, total)`.
fn parse_content_range(value: &str) -> Result<(u64, u64, Option<u64>), MeowError> {
    let s = value.trim();
    let mut parts = s.splitn(2, ' ');
    let unit = parts.next().unwrap_or_default().trim();
    let range_and_total = parts.next().unwrap_or_default().trim();
    if unit != "bytes" || range_and_total.is_empty() {
        crate::meow_flow_log!(
            "content_range",
            "invalid content-range unit/format: value={}",
            value
        );
        return Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!("invalid content-range: {value}"),
        ));
    }

    let (range_part, total_part) = range_and_total.split_once('/').ok_or_else(|| {
        MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!("invalid content-range: {value}"),
        )
    })?;
    let (start_s, end_s) = range_part.trim().split_once('-').ok_or_else(|| {
        MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!("invalid content-range range: {value}"),
        )
    })?;
    let start = start_s.trim().parse::<u64>().map_err(|_| {
        MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!("invalid content-range start: {value}"),
        )
    })?;
    let end = end_s.trim().parse::<u64>().map_err(|_| {
        MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!("invalid content-range end: {value}"),
        )
    })?;
    if end < start {
        crate::meow_flow_log!(
            "content_range",
            "invalid content-range order: start={} end={} value={}",
            start,
            end,
            value
        );
        return Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!("invalid content-range order: {value}"),
        ));
    }

    let total = if total_part.trim() == "*" {
        None
    } else {
        Some(total_part.trim().parse::<u64>().map_err(|_| {
            MeowError::from_code(
                InnerErrorCode::InvalidRange,
                format!("invalid content-range total: {value}"),
            )
        })?)
    };
    Ok((start, end, total))
}

/// Downloads exactly one range chunk and appends/writes it to local file.
async fn download_one_chunk(
    client: &reqwest::Client,
    task: &TransferTask,
    download: Arc<dyn BreakpointDownload + Send + Sync>,
    offset: u64,
    chunk_size: u64,
    remote_total_size: u64,
) -> Result<ChunkOutcome, MeowError> {
    let total = remote_total_size;
    if offset >= total {
        crate::meow_flow_log!(
            "download_chunk",
            "offset already reached total: offset={} total={}",
            offset,
            total
        );
        return Ok(ChunkOutcome {
            next_offset: offset,
            total_size: total,
            done: true,
            completion_payload: None,
        });
    }

    let spec = range_spec(offset, chunk_size, total);
    let mut headers = task.headers().clone();
    download.merge_range_get_headers(DownloadRangeGetCtx {
        task,
        range_value: &spec,
        base: &mut headers,
    })?;

    let resp = client
        .get(task.url())
        .headers(headers)
        .send()
        .await
        .map_err(map_reqwest)?;
    let status = resp.status();
    if status != reqwest::StatusCode::PARTIAL_CONTENT {
        let body = resp.text().await.unwrap_or_default();
        crate::meow_flow_log!(
            "download_chunk",
            "invalid status for range GET: status={} offset={} spec={}",
            status,
            offset,
            spec
        );
        return Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!("download GET requires 206 Partial Content, got {status}: {body}"),
        ));
    }
    let content_range = resp
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            crate::meow_flow_log!(
                "download_chunk",
                "missing content-range header: offset={} spec={}",
                offset,
                spec
            );
            MeowError::from_code_str(
                InnerErrorCode::InvalidRange,
                "download response missing content-range for ranged GET",
            )
        })?;
    let (range_start, range_end, range_total) = parse_content_range(&content_range)?;
    if range_start != offset {
        crate::meow_flow_log!(
            "download_chunk",
            "content-range start mismatch: expected={} got={} header={}",
            offset,
            range_start,
            content_range
        );
        return Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!("download content-range start mismatch: expected {offset}, got {range_start}"),
        ));
    }
    if let Some(rt) = range_total {
        if rt != total {
            crate::meow_flow_log!(
                "download_chunk",
                "content-range total mismatch: head_total={} range_total={}",
                total,
                rt
            );
            return Err(MeowError::from_code(
                InnerErrorCode::InvalidRange,
                format!("download total size changed: HEAD={total}, Content-Range={rt}"),
            ));
        }
    }
    let data = resp.bytes().await.map_err(map_reqwest)?;
    if data.is_empty() {
        crate::meow_flow_log!(
            "download_chunk",
            "empty body for ranged chunk: offset={} spec={}",
            offset,
            spec
        );
        return Err(MeowError::from_code_str(
            InnerErrorCode::InvalidRange,
            "download chunk empty body",
        ));
    }
    let expected_len = range_end - range_start + 1;
    if data.len() as u64 != expected_len {
        crate::meow_flow_log!(
            "download_chunk",
            "body length mismatch: expected={} actual={} header={}",
            expected_len,
            data.len(),
            content_range
        );
        return Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!(
                "download body length mismatch: expected {expected_len}, got {}",
                data.len()
            ),
        ));
    }

    let path = task.file_path();
    if offset == 0 {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    MeowError::from_source(
                        InnerErrorCode::IoError,
                        format!("create download parent dir failed: {}", parent.display()),
                        e,
                    )
                })?;
            }
        }
    }
    let mut slot = task.download_file_slot().lock().await;
    if offset == 0 {
        let created = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .await
            .map_err(|e| {
                MeowError::from_source(
                    InnerErrorCode::IoError,
                    format!("create download file failed: {}", path.display()),
                    e,
                )
            })?;
        *slot = Some(created);
    } else if slot.is_none() {
        let opened = OpenOptions::new()
            .write(true)
            .create(true)
            .open(path)
            .await
            .map_err(|e| {
                MeowError::from_source(
                    InnerErrorCode::IoError,
                    format!("open download file failed: {}", path.display()),
                    e,
                )
            })?;
        let local_len = opened
            .metadata()
            .await
            .map_err(|e| {
                MeowError::from_source(
                    InnerErrorCode::IoError,
                    format!("read download metadata failed: {}", path.display()),
                    e,
                )
            })?
            .len();
        if local_len != offset {
            crate::meow_flow_log!(
                "download_chunk",
                "local length mismatch before resume write: expected={} got={}",
                offset,
                local_len
            );
            return Err(MeowError::from_code(
                InnerErrorCode::InvalidRange,
                format!("local file size mismatch: expected {offset}, got {local_len}"),
            ));
        }
        *slot = Some(opened);
    }
    let f = slot.as_mut().ok_or_else(|| {
        MeowError::from_code_str(
            InnerErrorCode::IoError,
            "download file slot unexpectedly empty after open/create",
        )
    })?;
    f.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(|e| {
            MeowError::from_source(
                InnerErrorCode::IoError,
                format!(
                    "seek download file failed: offset={offset} path={}",
                    path.display()
                ),
                e,
            )
        })?;
    f.write_all(&data).await.map_err(|e| {
        MeowError::from_source(
            InnerErrorCode::IoError,
            format!("write download file failed: path={}", path.display()),
            e,
        )
    })?;

    let next = offset + data.len() as u64;
    crate::meow_flow_log!(
        "download_chunk",
        "chunk write success: file={} offset={} next={} total={}",
        task.file_name(),
        offset,
        next,
        total
    );
    Ok(ChunkOutcome {
        next_offset: next,
        total_size: total,
        done: next >= total,
        completion_payload: None,
    })
}

/// Maps `reqwest::Error` into SDK error type.
fn map_reqwest(e: reqwest::Error) -> MeowError {
    MeowError::from_source(InnerErrorCode::HttpError, e.to_string(), e)
}

#[async_trait]
impl TransferTrait for DefaultHttpClient {
    /// Prepares transfer execution according to task direction.
    async fn prepare(
        &self,
        task: &TransferTask,
        local_offset: u64,
    ) -> Result<PrepareOutcome, MeowError> {
        let client = self.client_for(task);
        match task.direction() {
            Direction::Upload => {
                upload_prepare(&client, task, self.upload_arc(task), local_offset).await
            }
            Direction::Download => {
                download_prepare(&client, task, self.download_arc(task), local_offset).await
            }
        }
    }

    /// Transfers one chunk according to task direction.
    async fn transfer_chunk(
        &self,
        task: &TransferTask,
        offset: u64,
        chunk_size: u64,
        remote_total_size: u64,
    ) -> Result<ChunkOutcome, MeowError> {
        let client = self.client_for(task);
        match task.direction() {
            Direction::Upload => {
                upload_one_chunk(&client, task, self.upload_arc(task), offset, chunk_size).await
            }
            Direction::Download => {
                download_one_chunk(
                    &client,
                    task,
                    self.download_arc(task),
                    offset,
                    chunk_size,
                    remote_total_size,
                )
                .await
            }
        }
    }

    /// Handles task cancel; upload direction may trigger protocol abort.
    async fn cancel(&self, task: &TransferTask) -> Result<(), MeowError> {
        if task.direction() != Direction::Upload {
            return Ok(());
        }
        let client = self.client_for(task);
        self.upload_arc(task).abort_upload(&client, task).await
    }
}

#[cfg(test)]
mod tests {
    use super::parse_content_range;

    #[test]
    fn parse_content_range_ok() {
        let (start, end, total) = parse_content_range("bytes 10-99/1000").unwrap();
        assert_eq!(start, 10);
        assert_eq!(end, 99);
        assert_eq!(total, Some(1000));
    }

    #[test]
    fn parse_content_range_unknown_total_ok() {
        let (start, end, total) = parse_content_range("bytes 0-1023/*").unwrap();
        assert_eq!(start, 0);
        assert_eq!(end, 1023);
        assert_eq!(total, None);
    }

    #[test]
    fn parse_content_range_invalid_order_fail() {
        let err = parse_content_range("bytes 100-1/1000").unwrap_err();
        assert!(err.msg().contains("invalid content-range order"));
    }
}
