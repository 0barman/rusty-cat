use std::sync::Arc;

use reqwest::header::{HeaderMap, HeaderValue, CONTENT_ENCODING, CONTENT_RANGE, ETAG, IF_MATCH};
use sha2::{Digest, Sha256};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use crate::chunk_outcome::ChunkOutcome;
use crate::error::{InnerErrorCode, MeowError};
use crate::http_breakpoint::{
    BreakpointDownload, BreakpointUpload, DownloadRangeGetCtx, UploadChunkCtx,
};
use crate::transfer_task::TransferTask;
use crate::upload_source::UploadSource;

const MAX_ERROR_BODY_PREVIEW_BYTES: usize = 4096;

fn strong_response_etag(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let etag = headers.get(ETAG)?.to_str().ok()?.trim();
    if etag.len() < 2
        || etag
            .get(..2)
            .map(|prefix| prefix.eq_ignore_ascii_case("W/"))
            .unwrap_or(false)
        || !etag.starts_with('"')
        || !etag.ends_with('"')
    {
        return None;
    }
    Some(etag.to_owned())
}

async fn build_download_range_request(
    download: Arc<dyn BreakpointDownload + Send + Sync>,
    task: &TransferTask,
    range_value: &str,
    base: HeaderMap,
) -> Result<(String, HeaderMap), MeowError> {
    let task = task.clone();
    let range_value = range_value.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut headers = base;
        download.merge_range_get_headers(DownloadRangeGetCtx {
            task: &task,
            range_value: &range_value,
            base: &mut headers,
        })?;
        Ok((download.range_url(&task), headers))
    })
    .await
    .map_err(|error| {
        MeowError::from_code(
            InnerErrorCode::InvalidTaskState,
            format!("download range request builder worker failed: {error}"),
        )
    })?
}

/// Maps `reqwest::Error` into SDK error type.
pub(crate) fn map_reqwest(e: reqwest::Error) -> MeowError {
    MeowError::from_source(InnerErrorCode::HttpError, e.to_string(), e)
}

async fn rollback_download_file(file: &mut File, offset: u64, path: &std::path::Path) {
    if let Err(e) = file.set_len(offset).await {
        crate::meow_warn_log!(
            "download_chunk",
            "rollback set_len failed: path={} offset={} err={}",
            path.display(),
            offset,
            e
        );
    }
    if let Err(e) = file.seek(std::io::SeekFrom::Start(offset)).await {
        crate::meow_warn_log!(
            "download_chunk",
            "rollback seek failed: path={} offset={} err={}",
            path.display(),
            offset,
            e
        );
    }
}

/// Verifies the local download file still exists and is consistent with what we
/// just wrote, turning a silent "file deleted/replaced mid-transfer" into an
/// explicit [`InnerErrorCode::LocalFileRemoved`] terminal error.
///
/// On Unix, deleting a file that still has an open handle merely unlinks the
/// directory entry: subsequent writes keep succeeding against the now-orphaned
/// inode and the bytes are lost when the handle closes, with no I/O error ever
/// raised. Re-`stat`ing the path after each chunk catches that case (the entry
/// is gone → `NotFound`) as well as a delete-then-recreate (the visible file is
/// shorter than what we have written → length check).
async fn ensure_download_file_present(
    file: &File,
    path: &std::path::Path,
    expected_min_len: u64,
) -> Result<(), MeowError> {
    let handle_meta = file.metadata().await.map_err(|e| {
        MeowError::from_io(
            format!("stat opened download file failed: {}", path.display()),
            e,
        )
    })?;
    match tokio::fs::metadata(path).await {
        Ok(meta) if meta.len() >= expected_min_len && same_file_generation(&handle_meta, &meta) => {
            Ok(())
        }
        Ok(meta) => Err(MeowError::from_code(
            InnerErrorCode::LocalFileRemoved,
            format!(
                "download file replaced during transfer: path={} expected_len>={} visible_len={} same_generation={}",
                path.display(),
                expected_min_len,
                meta.len(),
                same_file_generation(&handle_meta, &meta)
            ),
        )),
        Err(e) => Err(MeowError::from_io(
            format!(
                "download file missing during transfer: path={}",
                path.display()
            ),
            e,
        )),
    }
}

#[cfg(unix)]
fn same_file_generation(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(windows)]
fn same_file_generation(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    left.volume_serial_number() == right.volume_serial_number()
        && left.file_index() == right.file_index()
}

#[cfg(not(any(unix, windows)))]
fn same_file_generation(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
        && left.created().ok() == right.created().ok()
}

async fn read_error_body_preview(resp: &mut reqwest::Response, max_bytes: usize) -> String {
    let mut out = Vec::new();
    while out.len() < max_bytes {
        let next = match resp.chunk().await {
            Ok(v) => v,
            Err(_) => break,
        };
        let Some(chunk) = next else {
            break;
        };
        if chunk.is_empty() {
            continue;
        }
        let remain = max_bytes - out.len();
        if chunk.len() <= remain {
            out.extend_from_slice(&chunk);
        } else {
            out.extend_from_slice(&chunk[..remain]);
            break;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Reads the bytes for the chunk at `offset` from the task's upload source.
///
/// Returns `Some((chunk_bytes, uploaded_len))`, or `None` when `offset` is at or
/// past `total` (nothing left to upload at this offset). For the file source it
/// reads from the stable task-owned handle with positioned I/O, so concurrent
/// parts never share a mutable cursor. The resulting `Bytes` owns the allocation
/// and retry clones are O(1). For the in-memory source it slices the shared
/// `Bytes` (a refcount bump, no copy).
///
/// Shared by the serial [`upload_one_chunk`] and the parallel
/// [`upload_one_chunk_part`] paths so the read logic cannot drift between them.
async fn read_upload_chunk(
    task: &TransferTask,
    offset: u64,
    chunk_size: u64,
) -> Result<Option<(bytes::Bytes, u64)>, MeowError> {
    let total = task.total_size();
    if offset >= total {
        return Ok(None);
    }
    let read_len = chunk_size.min(total - offset);
    if read_len == 0 {
        return Ok(None);
    }
    let read_len_usize = usize::try_from(read_len).map_err(|_| {
        MeowError::from_code_str(
            InnerErrorCode::IoError,
            "upload chunk length does not fit this target architecture",
        )
    })?;

    match task.upload_source() {
        Some(UploadSource::File(_path)) => {
            let snapshot = task.upload_file_snapshot().ok_or_else(|| {
                MeowError::from_code_str(
                    InnerErrorCode::InvalidTaskState,
                    "upload file snapshot missing for file source",
                )
            })?;
            snapshot.validate_generation(false).await?;
            let chunk = snapshot.read_exact_at(offset, read_len).await?;
            let chunk_len = chunk.len() as u64;
            Ok(Some((chunk, chunk_len)))
        }
        Some(UploadSource::Bytes(bytes)) => {
            let start = usize::try_from(offset).map_err(|_| {
                MeowError::from_code_str(
                    InnerErrorCode::InvalidRange,
                    "upload bytes offset does not fit this target architecture",
                )
            })?;
            let end = start.checked_add(read_len_usize).ok_or_else(|| {
                MeowError::from_code_str(
                    InnerErrorCode::InvalidRange,
                    "upload bytes range overflow",
                )
            })?;
            if end > bytes.len() {
                return Err(MeowError::from_code(
                    InnerErrorCode::InvalidRange,
                    format!(
                        "upload bytes source out of range: start={start} end={end} len={}",
                        bytes.len()
                    ),
                ));
            }
            // The in-memory source is the zero-copy hot path: `Bytes::slice` only
            // bumps the refcount, copying no data and allocating no new buffer.
            let chunk = bytes.slice(start..end);
            let chunk_len = chunk.len() as u64;
            Ok(Some((chunk, chunk_len)))
        }
        None => Err(MeowError::from_code_str(
            InnerErrorCode::ParameterEmpty,
            "upload task missing upload source",
        )),
    }
}

async fn abort_upload_once(
    client: &reqwest::Client,
    task: &TransferTask,
    upload: &Arc<dyn BreakpointUpload + Send + Sync>,
) {
    if task.begin_upload_abort() {
        if let Err(abort_error) = upload.abort_upload(client, task).await {
            crate::meow_warn_log!(
                "upload_source",
                "abort after source validation failure also failed: {}",
                crate::log::redact_secrets(&abort_error.to_string())
            );
        }
    }
}

async fn validate_serial_upload_before_last_part(
    client: &reqwest::Client,
    task: &TransferTask,
    upload: &Arc<dyn BreakpointUpload + Send + Sync>,
    offset: u64,
    len: u64,
) -> Result<(), MeowError> {
    let is_last = offset
        .checked_add(len)
        .map(|end| end >= task.total_size())
        .ok_or_else(|| {
            MeowError::from_code_str(InnerErrorCode::InvalidRange, "upload part end overflow")
        })?;
    if !is_last {
        return Ok(());
    }
    if let Some(snapshot) = task.upload_file_snapshot() {
        if let Err(error) = snapshot.validate_generation(true).await {
            abort_upload_once(client, task, upload).await;
            return Err(error);
        }
    }
    Ok(())
}

/// Uploads exactly one chunk WITHOUT finalizing the upload, returning this part's
/// next offset.
///
/// This is the parallel-parts path: the upload is finalized exactly once by the
/// scheduler (via `TransferTrait::complete`) after every part has been uploaded
/// as a contiguous prefix, so an individual part must never call
/// `complete_upload` nor treat a server `completed_file_id` as terminal — doing
/// so from whichever part reaches the end first would commit a torn object while
/// lower parts are still in flight. The returned `next_offset`/`done` describe
/// only THIS part; the scheduler's window keys completion on dispatched offsets,
/// not on these fields.
pub(crate) async fn upload_one_chunk_part(
    client: &reqwest::Client,
    task: &TransferTask,
    upload: Arc<dyn BreakpointUpload + Send + Sync>,
    offset: u64,
    chunk_size: u64,
) -> Result<ChunkOutcome, MeowError> {
    let total = task.total_size();
    let read = match read_upload_chunk(task, offset, chunk_size).await {
        Ok(read) => read,
        Err(error) => {
            abort_upload_once(client, task, &upload).await;
            return Err(error);
        }
    };
    let Some((chunk_bytes, uploaded_chunk_len)) = read else {
        return Ok(ChunkOutcome {
            next_offset: offset,
            total_size: total,
            done: true,
            completion_payload: None,
        });
    };
    let uploaded_end = offset.checked_add(uploaded_chunk_len).ok_or_else(|| {
        MeowError::from_code_str(InnerErrorCode::InvalidRange, "upload part end overflow")
    })?;
    let info = upload
        .upload_chunk(UploadChunkCtx {
            client,
            task,
            chunk: chunk_bytes,
            offset,
        })
        .await
        .map_err(|e| {
            crate::log::emit_lazy(|| {
                let mut log = crate::log::Log::error(
                    "upload_part",
                    format!("part chunk upload failed (orphaned-parts risk): {}", e),
                )
                .with_key(task.file_name())
                .with_offset(offset)
                .with_byte_len(uploaded_chunk_len)
                .with_url(task.url());
                if let Some(status) = e.http_status() {
                    log = log.with_http_status(status);
                }
                log = log.with_error_code(e.code());
                log
            });
            e
        })?;
    let next = info.next_byte.unwrap_or(uploaded_end).min(total);
    Ok(ChunkOutcome {
        next_offset: next,
        total_size: total,
        done: next >= total,
        // Never finalize from a part: completion is hoisted to the scheduler.
        completion_payload: None,
    })
}

/// Uploads exactly one chunk and returns next transfer outcome.
///
/// Reads the chunk straight into a per-chunk [`bytes::BytesMut`] and `freeze()`s
/// it into the request `Bytes` body with no extra copy: `freeze` reuses the same
/// allocation, and retries clone that `Bytes` (refcount only).
pub(crate) async fn upload_one_chunk(
    client: &reqwest::Client,
    task: &TransferTask,
    upload: Arc<dyn BreakpointUpload + Send + Sync>,
    offset: u64,
    chunk_size: u64,
) -> Result<ChunkOutcome, MeowError> {
    let total = task.total_size();
    let read = match read_upload_chunk(task, offset, chunk_size).await {
        Ok(read) => read,
        Err(error) => {
            abort_upload_once(client, task, &upload).await;
            return Err(error);
        }
    };
    let Some((chunk_bytes, uploaded_chunk_len)) = read else {
        return Ok(ChunkOutcome {
            next_offset: offset,
            total_size: total,
            done: true,
            completion_payload: None,
        });
    };
    // A serial protocol may finalize inside its last `upload_chunk`, so verify
    // the source exactly once before sending that last buffered part. Parallel
    // uploads intentionally skip this checkpoint and perform their sole full
    // re-hash after every part has settled, immediately before multipart
    // completion.
    validate_serial_upload_before_last_part(client, task, &upload, offset, uploaded_chunk_len)
        .await?;
    let uploaded_end = offset.checked_add(uploaded_chunk_len).ok_or_else(|| {
        MeowError::from_code_str(InnerErrorCode::InvalidRange, "upload chunk end overflow")
    })?;
    let info = upload
        .upload_chunk(UploadChunkCtx {
            client,
            task,
            chunk: chunk_bytes,
            offset,
        })
        .await
        .map_err(|e| {
            crate::log::emit_lazy(|| {
                let mut log = crate::log::Log::error(
                    "upload_chunk",
                    format!("serial chunk upload failed: {}", e),
                )
                .with_key(task.file_name())
                .with_offset(offset)
                .with_byte_len(uploaded_chunk_len)
                .with_url(task.url());
                if let Some(status) = e.http_status() {
                    log = log.with_http_status(status);
                }
                log = log.with_error_code(e.code());
                log
            });
            e
        })?;
    if offset
        .checked_add(uploaded_chunk_len)
        .map(|end| end >= total)
        .unwrap_or(true)
    {
        if let Some(snapshot) = task.upload_file_snapshot() {
            if let Err(error) = snapshot.validate_generation(false).await {
                abort_upload_once(client, task, &upload).await;
                return Err(error);
            }
        }
    }
    if info.completed_file_id.is_some() {
        return Ok(ChunkOutcome {
            next_offset: total,
            total_size: total,
            done: true,
            completion_payload: info.completed_file_id,
        });
    }
    let next = info.next_byte.unwrap_or(uploaded_end).min(total);
    let mut completion_payload = None;
    if next >= total {
        completion_payload = upload.complete_upload(client, task).await.map_err(|e| {
            crate::log::emit_lazy(|| {
                let mut log = crate::log::Log::error(
                    "complete",
                    format!("complete_upload finalize failed: {}", e),
                )
                .with_key(task.file_name())
                .with_offset(offset)
                .with_byte_len(total)
                .with_url(task.url());
                if let Some(status) = e.http_status() {
                    log = log.with_http_status(status);
                }
                log = log.with_error_code(e.code());
                log
            });
            e
        })?;
        crate::log::emit_lazy(|| {
            crate::log::Log::key("complete", "complete_upload finalized")
                .with_key(task.file_name())
                .with_byte_len(total)
                .with_url(task.url())
        });
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
    let end = start
        .saturating_add(chunk_size.saturating_sub(1))
        .min(total.saturating_sub(1));
    format!("bytes={start}-{end}")
}

/// Parses `Content-Range` header into `(start, end, total)`.
fn parse_content_range(value: &str) -> Result<(u64, u64, Option<u64>), MeowError> {
    let s = value.trim();
    let mut parts = s.splitn(2, ' ');
    let unit = parts.next().unwrap_or_default().trim();
    let range_and_total = parts.next().unwrap_or_default().trim();
    if unit != "bytes" || range_and_total.is_empty() {
        crate::meow_error_log!(
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
        crate::meow_error_log!(
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
pub(crate) async fn download_one_chunk(
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
    let (range_url, headers) =
        build_download_range_request(download, task, &spec, task.headers().clone()).await?;
    // Sanitized snapshot for logging only: the raw `range_url` is a presigned/SAS
    // URL and is moved into `client.get(..)` below, so capture a redacted copy now.
    let sanitized_range_url = crate::log::sanitize_url(&range_url);
    let mut resp = client
        .get(range_url)
        .headers(headers)
        .send()
        .await
        .map_err(|e| {
            let meow_err = map_reqwest(e);
            crate::log::emit_lazy(|| {
                crate::log::Log::error(
                    "range_get",
                    format!(
                        "download GET send failed: {}",
                        crate::log::redact_secrets(&meow_err.to_string())
                    ),
                )
                .with_key(task.file_name())
                .with_offset(offset)
                .with_url(sanitized_range_url.as_str())
            });
            meow_err
        })?;
    let status = resp.status();
    if status != reqwest::StatusCode::PARTIAL_CONTENT {
        let body = read_error_body_preview(&mut resp, MAX_ERROR_BODY_PREVIEW_BYTES).await;
        crate::log::emit_lazy(|| {
            crate::log::Log::error(
                "range_get",
                format!(
                    "invalid status for range GET: status={} offset={} spec={} body={}",
                    status,
                    offset,
                    spec,
                    crate::log::redact_secrets(&body)
                ),
            )
            .with_key(task.file_name())
            .with_offset(offset)
            .with_http_status(status.as_u16())
            .with_url(sanitized_range_url.as_str())
        });
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
            crate::log::emit_lazy(|| {
                crate::log::Log::error(
                    "range_get",
                    format!(
                        "missing content-range header: offset={} spec={}",
                        offset, spec
                    ),
                )
                .with_key(task.file_name())
                .with_offset(offset)
                .with_url(sanitized_range_url.as_str())
            });
            MeowError::from_code_str(
                InnerErrorCode::InvalidRange,
                "download response missing content-range for ranged GET",
            )
        })?;
    let (range_start, range_end, range_total) = parse_content_range(&content_range)?;
    if range_start != offset {
        crate::log::emit_lazy(|| {
            crate::log::Log::error(
                "range_get",
                format!(
                    "content-range start mismatch: expected={} got={} header={}",
                    offset, range_start, content_range
                ),
            )
            .with_key(task.file_name())
            .with_offset(offset)
            .with_url(sanitized_range_url.as_str())
        });
        return Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!("download content-range start mismatch: expected {offset}, got {range_start}"),
        ));
    }
    let requested_end = offset
        .saturating_add(chunk_size.saturating_sub(1))
        .min(total.saturating_sub(1));
    if range_end != requested_end {
        return Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!(
                "download content-range end mismatch: expected {requested_end}, got {range_end}"
            ),
        ));
    }
    if let Some(rt) = range_total {
        if rt != total {
            crate::log::emit_lazy(|| {
                crate::log::Log::error(
                    "range_get",
                    format!(
                        "content-range total mismatch: head_total={} range_total={}",
                        total, rt
                    ),
                )
                .with_key(task.file_name())
                .with_offset(offset)
                .with_url(sanitized_range_url.as_str())
            });
            return Err(MeowError::from_code(
                InnerErrorCode::InvalidRange,
                format!("download total size changed: HEAD={total}, Content-Range={rt}"),
            ));
        }
    }
    let expected_len = range_end
        .checked_sub(range_start)
        .and_then(|len| len.checked_add(1))
        .ok_or_else(|| {
            MeowError::from_code_str(
                InnerErrorCode::InvalidRange,
                "content-range length overflow",
            )
        })?;

    let path = task.file_path();
    if offset == 0 {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    MeowError::from_io(
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
                MeowError::from_io(
                    format!("create download file failed: {}", path.display()),
                    e,
                )
            })?;
        *slot = Some(created);
    } else if slot.is_none() {
        let opened = OpenOptions::new()
            .write(true)
            .create(true)
            // Resume must preserve already-written bytes beyond the current
            // offset; spell this out so platform defaults cannot drift.
            .truncate(false)
            .open(path)
            .await
            .map_err(|e| {
                MeowError::from_io(format!("open download file failed: {}", path.display()), e)
            })?;
        let local_len = opened
            .metadata()
            .await
            .map_err(|e| {
                MeowError::from_io(
                    format!("read download metadata failed: {}", path.display()),
                    e,
                )
            })?
            .len();
        if local_len != offset {
            crate::log::emit_lazy(|| {
                crate::log::Log::error(
                    "range_get",
                    format!(
                        "local length mismatch before resume write: expected={} got={}",
                        offset, local_len
                    ),
                )
                .with_key(task.file_name())
                .with_offset(offset)
            });
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
            MeowError::from_io(
                format!(
                    "seek download file failed: offset={offset} path={}",
                    path.display()
                ),
                e,
            )
        })?;
    let mut written_len = 0_u64;
    loop {
        let chunk = match resp.chunk().await {
            Ok(v) => v,
            Err(e) => {
                if written_len > 0 {
                    rollback_download_file(f, offset, path).await;
                }
                crate::log::emit_lazy(|| {
                    crate::log::Log::error(
                        "range_get",
                        format!(
                            "streaming body read failed: {}",
                            crate::log::redact_secrets(&e.to_string())
                        ),
                    )
                    .with_key(task.file_name())
                    .with_offset(offset)
                    .with_url(sanitized_range_url.as_str())
                });
                return Err(map_reqwest(e));
            }
        };
        let Some(chunk) = chunk else {
            break;
        };
        if chunk.is_empty() {
            continue;
        }
        let next_written = written_len.checked_add(chunk.len() as u64).ok_or_else(|| {
            MeowError::from_code_str(InnerErrorCode::InvalidRange, "range body length overflow")
        })?;
        if next_written > expected_len {
            rollback_download_file(f, offset, path).await;
            crate::meow_error_log!(
                "download_chunk",
                "body length exceeded expected range: expected={} next_written={} header={}",
                expected_len,
                next_written,
                content_range
            );
            return Err(MeowError::from_code(
                InnerErrorCode::InvalidRange,
                format!(
                    "download body length mismatch: expected {expected_len}, got at least {next_written}"
                ),
            ));
        }
        if let Err(e) = f.write_all(&chunk).await {
            rollback_download_file(f, offset, path).await;
            crate::log::emit_lazy(|| {
                crate::log::Log::error(
                    "download_chunk",
                    format!("write download file failed: path={}", path.display()),
                )
                .with_key(task.file_name())
                .with_offset(offset)
            });
            // `from_io` classifies out-of-space (`ENOSPC` / `ERROR_DISK_FULL`)
            // into `DiskFull` and a vanished file into `LocalFileRemoved`.
            return Err(MeowError::from_io(
                format!("write download file failed: path={}", path.display()),
                e,
            ));
        }
        written_len = next_written;
    }
    if written_len == 0 {
        crate::meow_error_log!(
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
    if written_len != expected_len {
        rollback_download_file(f, offset, path).await;
        crate::meow_error_log!(
            "download_chunk",
            "body length mismatch: expected={} actual={} header={}",
            expected_len,
            written_len,
            content_range
        );
        return Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!("download body length mismatch: expected {expected_len}, got {written_len}"),
        ));
    }

    let next = offset.checked_add(written_len).ok_or_else(|| {
        MeowError::from_code_str(InnerErrorCode::InvalidRange, "download offset overflow")
    })?;
    if let Err(e) = f.flush().await {
        rollback_download_file(f, offset, path).await;
        return Err(MeowError::from_io(
            format!("flush download file failed: path={}", path.display()),
            e,
        ));
    }
    // Detect a file deleted/replaced while this chunk was being written. On Unix
    // such a deletion does not fail `write_all` (the handle keeps an orphaned
    // inode alive), so without this check the failure would be silent.
    ensure_download_file_present(f, path, next).await?;
    crate::meow_trace_log!(
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

/// Downloads exactly one range chunk into `task.file_path()` at its absolute
/// offset using a dedicated file handle. Concurrency-safe: no shared cursor, no
/// truncating rollback. The file MUST already be pre-sized to at least
/// `offset + expected_len` (the parallel `download_prepare` pre-sizes to total).
pub(crate) async fn download_one_chunk_part_positioned(
    client: &reqwest::Client,
    task: &TransferTask,
    download: Arc<dyn BreakpointDownload + Send + Sync>,
    offset: u64,
    chunk_size: u64,
    remote_total_size: u64,
) -> Result<(ChunkOutcome, [u8; 32]), MeowError> {
    let total = remote_total_size;
    if offset >= total {
        return Ok((
            ChunkOutcome {
                next_offset: offset,
                total_size: total,
                done: true,
                completion_payload: None,
            },
            Sha256::digest([]).into(),
        ));
    }
    let spec = range_spec(offset, chunk_size, total);
    let (range_url, mut headers) =
        build_download_range_request(download, task, &spec, task.headers().clone()).await?;
    let expected_validator = {
        let progress = task.download_progress().lock().map_err(|_| {
            MeowError::from_code_str(
                InnerErrorCode::LockPoisoned,
                "download checkpoint lock poisoned",
            )
        })?;
        progress
            .as_ref()
            .and_then(|progress| progress.expected_validator())
            .map(str::to_owned)
    };
    if let Some(validator) = expected_validator.as_deref() {
        let value = HeaderValue::from_str(validator).map_err(|e| {
            MeowError::from_code(
                InnerErrorCode::InvalidRange,
                format!("invalid stored download validator: {e}"),
            )
        })?;
        headers.insert(IF_MATCH, value);
    }
    let mut resp = client
        .get(range_url)
        .headers(headers)
        .send()
        .await
        .map_err(map_reqwest)?;
    let status = resp.status();
    if status == reqwest::StatusCode::OK {
        // Server ignored Range and is streaming the whole body. Positioned
        // parallel writes require 206; surface a clear, terminal error so the
        // caller (app) can retry serially. Do NOT try to slice a 200 body here.
        let _ = read_error_body_preview(&mut resp, MAX_ERROR_BODY_PREVIEW_BYTES).await;
        return Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            "server returned 200 OK to a Range request (Range not honored); \
             concurrent download requires 206 Partial Content"
                .to_string(),
        ));
    }
    if status != reqwest::StatusCode::PARTIAL_CONTENT {
        let body = read_error_body_preview(&mut resp, MAX_ERROR_BODY_PREVIEW_BYTES).await;
        return Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!("range GET requires 206, got {status}: {body}"),
        ));
    }
    if let Some(encoding) = resp.headers().get(CONTENT_ENCODING) {
        let encoding = encoding.to_str().unwrap_or("<invalid>");
        if !encoding.eq_ignore_ascii_case("identity") {
            return Err(MeowError::from_code(
                InnerErrorCode::InvalidRange,
                format!("parallel range response used content-encoding {encoding}"),
            ));
        }
    }
    // Every range in one positioned download must prove that it belongs to one
    // immutable remote generation. HEAD may have supplied the validator; when
    // callers intentionally skip HEAD, the first 206 latches it under the
    // progress lock and every concurrent response is compared with that value.
    // Missing/weak ETags fail closed because byte digests only validate local
    // persistence, not that disjoint ranges came from the same object version.
    let actual_validator = strong_response_etag(resp.headers()).ok_or_else(|| {
        MeowError::from_code_str(
            InnerErrorCode::InvalidRange,
            "parallel range response must include a strong ETag",
        )
    })?;
    {
        let mut progress = task.download_progress().lock().map_err(|_| {
            MeowError::from_code_str(
                InnerErrorCode::LockPoisoned,
                "download checkpoint lock poisoned",
            )
        })?;
        let progress = progress.as_mut().ok_or_else(|| {
            MeowError::from_code_str(
                InnerErrorCode::InvalidTaskState,
                "parallel download progress missing while validating generation",
            )
        })?;
        match progress.expected_validator() {
            Some(expected) if expected != actual_validator => {
                return Err(MeowError::from_code(
                    InnerErrorCode::InvalidRange,
                    format!(
                        "parallel range response generation mismatch: expected ETag {expected}, got {actual_validator}"
                    ),
                ));
            }
            Some(_) => {}
            None => progress.set_expected_validator(actual_validator),
        }
    }
    let content_range = resp
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            MeowError::from_code_str(
                InnerErrorCode::InvalidRange,
                "range response missing content-range",
            )
        })?;
    let (range_start, range_end, range_total) = parse_content_range(&content_range)?;
    if range_start != offset {
        return Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!("content-range start mismatch: expected {offset}, got {range_start}"),
        ));
    }
    let requested_end = offset
        .saturating_add(chunk_size.saturating_sub(1))
        .min(total.saturating_sub(1));
    if range_end != requested_end {
        return Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!("content-range end mismatch: expected {requested_end}, got {range_end}"),
        ));
    }
    if let Some(rt) = range_total {
        if rt != total {
            return Err(MeowError::from_code(
                InnerErrorCode::InvalidRange,
                format!(
                    "content-range total mismatch (remote file may have been resized \
                     mid-download): expected total={total}, server reports {rt}; \
                     verify the URL is stable and retry"
                ),
            ));
        }
    }
    let expected_len = range_end
        .checked_sub(range_start)
        .and_then(|len| len.checked_add(1))
        .ok_or_else(|| {
            MeowError::from_code_str(
                InnerErrorCode::InvalidRange,
                "content-range length overflow",
            )
        })?;
    let part_end = offset.checked_add(expected_len).ok_or_else(|| {
        MeowError::from_code_str(InnerErrorCode::InvalidRange, "download part end overflow")
    })?;

    // Own handle: independent OS cursor; disjoint ranges never conflict.
    let path = task.file_path();
    let mut f = OpenOptions::new()
        .write(true)
        .create(false)
        .open(path)
        .await
        .map_err(|e| {
            MeowError::from_io(format!("open download file failed: {}", path.display()), e)
        })?;
    // Defensive precondition: the file MUST already be pre-sized to at least
    // `offset + expected_len` (download_prepare `set_len(total)`). Seeking past
    // EOF and writing would otherwise silently create a sparse hole on POSIX.
    let flen = f.metadata().await.map(|m| m.len()).map_err(|e| {
        MeowError::from_io(format!("stat download file failed: {}", path.display()), e)
    })?;
    if flen < part_end {
        return Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!(
                "download file not pre-sized for positioned write: len={flen}, need>={}",
                part_end
            ),
        ));
    }
    f.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(|e| {
            MeowError::from_io(format!("seek download file failed: offset={offset}"), e)
        })?;
    let mut written_len = 0_u64;
    let mut digest = Sha256::new();
    loop {
        let chunk = match resp.chunk().await {
            Ok(v) => v,
            Err(e) => return Err(map_reqwest(e)), // no rollback: retry overwrites
        };
        let Some(chunk) = chunk else { break };
        if chunk.is_empty() {
            continue;
        }
        let next_written = written_len.checked_add(chunk.len() as u64).ok_or_else(|| {
            MeowError::from_code_str(InnerErrorCode::InvalidRange, "range body length overflow")
        })?;
        if next_written > expected_len {
            return Err(MeowError::from_code(
                InnerErrorCode::InvalidRange,
                format!("range body too long: expected {expected_len}, got >= {next_written}"),
            ));
        }
        f.write_all(&chunk).await.map_err(|e| {
            MeowError::from_io(format!("write download file failed at {offset}"), e)
        })?;
        digest.update(&chunk);
        written_len = next_written;
    }
    if written_len != expected_len {
        return Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!("range body short: expected {expected_len}, got {written_len}"),
        ));
    }
    // D1 durability is coordinated after this handle is dropped: completed
    // writes enter an epoch and one target `sync_data` covers the frozen batch
    // before any of its sidecar bits become visible.
    Ok((
        ChunkOutcome {
            next_offset: part_end.min(total),
            total_size: total,
            done: part_end >= total,
            completion_payload: None,
        },
        digest.finalize().into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::ensure_download_file_present;
    use super::parse_content_range;
    use super::range_spec;

    #[test]
    fn range_spec_clamps_last_part() {
        // total=25, chunk=10 => last part at 20 spans bytes 20-24 (5 bytes)
        assert_eq!(range_spec(20, 10, 25), "bytes=20-24");
        assert_eq!(range_spec(0, 10, 25), "bytes=0-9");
    }

    #[test]
    fn range_spec_near_u64_max_does_not_wrap() {
        assert_eq!(
            range_spec(u64::MAX - 2, u64::MAX, u64::MAX),
            format!("bytes={}-{}", u64::MAX - 2, u64::MAX - 1)
        );
    }

    #[test]
    fn parse_content_range_ok() -> Result<(), crate::error::MeowError> {
        let (start, end, total) = parse_content_range("bytes 10-99/1000")?;
        assert_eq!(start, 10);
        assert_eq!(end, 99);
        assert_eq!(total, Some(1000));
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn same_length_visible_path_replacement_is_not_mistaken_for_open_handle() {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "rusty_cat_serial_download_generation_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let replacement = path.with_extension("replacement");
        std::fs::write(&path, vec![b'A'; 128]).expect("original");
        let handle = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .await
            .expect("handle");
        std::fs::write(&replacement, vec![b'B'; 128]).expect("replacement");
        std::fs::rename(&replacement, &path).expect("replace");

        let error = ensure_download_file_present(&handle, &path, 128)
            .await
            .expect_err("same-length replacement must be detected");
        assert_eq!(
            error.code(),
            crate::error::InnerErrorCode::LocalFileRemoved as i32
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn parse_content_range_unknown_total_ok() -> Result<(), crate::error::MeowError> {
        let (start, end, total) = parse_content_range("bytes 0-1023/*")?;
        assert_eq!(start, 0);
        assert_eq!(end, 1023);
        assert_eq!(total, None);
        Ok(())
    }

    #[test]
    fn parse_content_range_invalid_order_fail() {
        let err = parse_content_range("bytes 100-1/1000").unwrap_err();
        assert!(err.msg().contains("invalid content-range order"));
    }

    // ------------------------------------------------------------------
    // `ensure_download_file_present`: detection of a local file deleted or
    // replaced mid-transfer. Covers three cases: file present and intact,
    // file truncated/replaced, and file removed.
    // ------------------------------------------------------------------
    mod file_presence {
        use super::super::ensure_download_file_present;
        use crate::error::InnerErrorCode;

        fn unique_temp_path(case: &str) -> std::path::PathBuf {
            let mut p = std::env::temp_dir();
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            p.push(format!("rusty_cat_presence_{case}_{ts}.bin"));
            p
        }

        #[tokio::test]
        async fn present_file_with_enough_bytes_is_ok() {
            let path = unique_temp_path("ok");
            tokio::fs::write(&path, vec![0u8; 1024])
                .await
                .expect("write temp file");
            let handle = tokio::fs::File::open(&path).await.expect("open handle");

            // Expected length equals the actual length: should pass.
            let result = ensure_download_file_present(&handle, &path, 1024).await;
            assert!(result.is_ok(), "expected Ok, got {result:?}");

            let _ = tokio::fs::remove_file(&path).await;
        }

        #[tokio::test]
        async fn replaced_shorter_file_is_local_file_removed() {
            let path = unique_temp_path("shrank");
            tokio::fs::write(&path, vec![0u8; 4096])
                .await
                .expect("write temp file");
            let handle = tokio::fs::File::open(&path).await.expect("open handle");
            let replacement = path.with_extension("replacement");
            tokio::fs::write(&replacement, vec![0u8; 10])
                .await
                .expect("write replacement");
            tokio::fs::rename(&replacement, &path)
                .await
                .expect("replace path");

            let err = ensure_download_file_present(&handle, &path, 4096)
                .await
                .expect_err("expected LocalFileRemoved error");
            assert_eq!(err.code(), InnerErrorCode::LocalFileRemoved as i32);

            let _ = tokio::fs::remove_file(&path).await;
        }

        #[tokio::test]
        async fn missing_file_is_local_file_removed() {
            let path = unique_temp_path("missing");
            tokio::fs::write(&path, vec![0u8; 1])
                .await
                .expect("write temp file");
            let handle = tokio::fs::File::open(&path).await.expect("open handle");
            tokio::fs::remove_file(&path).await.expect("remove path");
            let err = ensure_download_file_present(&handle, &path, 1)
                .await
                .expect_err("expected LocalFileRemoved error");
            assert_eq!(err.code(), InnerErrorCode::LocalFileRemoved as i32);
        }
    }

    // ------------------------------------------------------------------
    // Property-based tests guarding the robustness of `parse_content_range`.
    // Key points:
    //   1) valid inputs (including `*` for unknown total) must round-trip;
    //   2) various invalid inputs (reversed order, missing slash, non-`bytes`
    //      unit, non-numeric) must not panic and must return an `InvalidRange`
    //      error.
    // ------------------------------------------------------------------
    mod prop {
        use super::super::parse_content_range;
        use proptest::prelude::*;

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(256))]

            /// Valid inputs should round-trip into the same triple.
            #[test]
            fn parse_content_range_roundtrip_ok(
                start in 0u64..u64::MAX / 2,
                len in 1u64..1024 * 1024,
                total in 1u64..u64::MAX / 2,
            ) {
                let end = start.saturating_add(len - 1);
                // Skip end >= total combinations; those are handled by upper-layer
                // semantics and are not this function's responsibility.
                prop_assume!(end < total);

                let header = format!("bytes {start}-{end}/{total}");
                let parsed = parse_content_range(&header);
                prop_assert!(parsed.is_ok());
                let (ps, pe, pt) = match parsed {
                    Ok(v) => v,
                    Err(_) => return Ok(()),
                };
                prop_assert_eq!(ps, start);
                prop_assert_eq!(pe, end);
                prop_assert_eq!(pt, Some(total));
            }

            /// The unknown-total (`*`) branch should also correctly return `None`.
            #[test]
            fn parse_content_range_unknown_total(
                start in 0u64..u64::MAX / 2,
                len in 1u64..1024 * 1024,
            ) {
                let end = start.saturating_add(len - 1);
                let header = format!("bytes {start}-{end}/*");
                let parsed = parse_content_range(&header);
                prop_assert!(parsed.is_ok());
                let (ps, pe, pt) = match parsed {
                    Ok(v) => v,
                    Err(_) => return Ok(()),
                };
                prop_assert_eq!(ps, start);
                prop_assert_eq!(pe, end);
                prop_assert!(pt.is_none());
            }

            /// end < start must return an error, never panic.
            #[test]
            fn parse_content_range_reversed_range_fails(
                start in 1u64..1_000_000,
                delta in 1u64..1_000_000,
                total in 1u64..u64::MAX / 2,
            ) {
                let end = start.saturating_sub(delta);
                prop_assume!(end < start);
                let header = format!("bytes {start}-{end}/{total}");
                prop_assert!(parse_content_range(&header).is_err());
            }

            /// Any arbitrary string must not cause a panic; either Ok or Err.
            #[test]
            fn parse_content_range_never_panics(s in ".{0,128}") {
                let _ = parse_content_range(&s);
            }

            /// A unit other than `bytes` should fail.
            #[test]
            fn parse_content_range_wrong_unit_fails(
                unit in "[a-z]{1,8}",
                start in 0u64..1_000_000,
                end in 0u64..1_000_000,
                total in 1u64..1_000_000,
            ) {
                prop_assume!(unit != "bytes");
                let header = format!("{unit} {start}-{end}/{total}");
                prop_assert!(parse_content_range(&header).is_err());
            }
        }
    }
}
