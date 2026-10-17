use std::sync::Arc;

use reqwest::header::{HeaderMap, HeaderValue, CONTENT_LENGTH, CONTENT_RANGE, IF_MATCH};
use sha2::{Digest, Sha256};
use tokio::fs::File;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use crate::chunk_outcome::ChunkOutcome;
use crate::error::{InnerErrorCode, MeowError};
use crate::http_breakpoint::{
    BreakpointDownload, BreakpointUpload, DownloadRangeGetCtx, UploadChunkCtx,
};
use crate::transfer_task::TransferTask;
use crate::upload_source::UploadSource;

use super::{
    download_generation_validator, ensure_accept_encoding_identity,
    ensure_response_identity_encoding, require_single_header, require_unambiguous_etag,
    set_accept_encoding_identity,
};

const MAX_ERROR_BODY_PREVIEW_BYTES: usize = 4096;

/// Binds every range in one task to a single strong HTTP validator. Local part
/// digests prove persistence only; they cannot prove that disjoint responses
/// came from the same remote representation.
fn validate_or_latch_range_validator(
    task: &TransferTask,
    actual: Option<String>,
    validator_required: bool,
    mode: &str,
) -> Result<(), MeowError> {
    let mut progress = task.download_progress().lock().map_err(|_| {
        MeowError::from_code_str(
            InnerErrorCode::LockPoisoned,
            "download checkpoint lock poisoned",
        )
    })?;
    let progress = progress.as_mut().ok_or_else(|| {
        MeowError::from_code(
            InnerErrorCode::InvalidTaskState,
            format!("{mode} download progress missing while validating generation"),
        )
    })?;
    let expected = progress.expected_validator().map(str::to_owned);
    match (expected.as_deref(), actual.as_deref()) {
        (Some(expected), Some(actual)) if expected == actual => Ok(()),
        (Some(expected), Some(actual)) => Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!(
                "{mode} range response generation mismatch: expected ETag {expected}, got {actual}"
            ),
        )),
        (Some(_), None) => Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!("{mode} range response must include the prepared strong ETag"),
        )),
        (None, Some(actual)) => {
            progress.set_expected_validator(actual.to_owned());
            Ok(())
        }
        (None, None) if validator_required => Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!("{mode} multi-range download requires a strong ETag"),
        )),
        (None, None) => Ok(()),
    }
}

fn expected_if_match_header(task: &TransferTask) -> Result<Option<HeaderValue>, MeowError> {
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

    expected_validator
        .map(|validator| {
            HeaderValue::from_str(&validator).map_err(|e| {
                MeowError::from_code(
                    InnerErrorCode::InvalidRange,
                    format!("invalid stored download validator: {e}"),
                )
            })
        })
        .transpose()
}

fn ensure_expected_if_match_preserved(
    headers: &HeaderMap,
    expected: &HeaderValue,
) -> Result<(), MeowError> {
    let mut values = headers.get_all(IF_MATCH).iter();
    let first = values.next();
    let has_duplicate = values.next().is_some();
    if first == Some(expected) && !has_duplicate {
        return Ok(());
    }

    Err(MeowError::from_code_str(
        InnerErrorCode::InvalidRange,
        "download protocol must preserve the executor-owned If-Match header exactly",
    ))
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
        let expected_if_match = expected_if_match_header(&task)?;
        let mut headers = base;
        set_accept_encoding_identity(&mut headers);
        if let Some(value) = expected_if_match.as_ref() {
            headers.insert(IF_MATCH, value.clone());
        }
        download.merge_range_get_headers(DownloadRangeGetCtx {
            task: &task,
            range_value: &range_value,
            base: &mut headers,
        })?;
        // Resolve the provider URL before validating the final headers. The
        // presigned implementation pairs these operations through thread-local
        // state, which must be consumed even when the contract check fails.
        let range_url = download.range_url(&task);
        if let Some(expected) = expected_if_match.as_ref() {
            ensure_expected_if_match_preserved(&headers, expected)?;
        }
        ensure_accept_encoding_identity(&headers, "download Range request")?;
        Ok((range_url, headers))
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
    let e = e.without_url();
    let message = e.to_string();
    MeowError::from_source(InnerErrorCode::HttpError, message, e)
}

async fn rollback_download_file(file: &mut File, offset: u64, path: &std::path::Path) {
    // Serial targets contain only the committed contiguous prefix. Remove every
    // byte from a failed streaming attempt before retrying so file length never
    // masquerades as verified progress.
    if let Err(e) = file.set_len(offset).await {
        crate::meow_warn_log!(
            "download_chunk",
            "rollback truncate failed: path={} offset={} err={}",
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

/// Verifies that both the locked handle and visible path still have the bytes we
/// just wrote, turning deletion/truncation into an explicit
/// [`InnerErrorCode::LocalFileRemoved`] terminal error.
///
/// On Unix, deleting a file that still has an open handle merely unlinks the
/// directory entry: subsequent writes keep succeeding against the now-orphaned
/// inode and the bytes are lost when the handle closes, with no I/O error ever
/// raised. Re-`stat`ing the path after each chunk catches that case (the entry
/// is gone → `NotFound`) as well as a shorter replacement. Same-length
/// replacement is intentionally decided by the final SHA-256 content check, so
/// an identical replacement remains valid on every platform.
async fn ensure_download_file_present(
    file: &File,
    path: &std::path::Path,
    expected_min_len: u64,
) -> Result<(), MeowError> {
    let handle_len = file
        .metadata()
        .await
        .map(|metadata| metadata.len())
        .map_err(|e| {
            MeowError::from_io(
                format!("stat opened download file failed: {}", path.display()),
                e,
            )
        })?;
    match tokio::fs::metadata(path).await {
        Ok(meta) if meta.len() >= expected_min_len && handle_len >= expected_min_len => Ok(()),
        Ok(meta) => Err(MeowError::from_code(
            InnerErrorCode::LocalFileRemoved,
            format!(
                "download file truncated during transfer: path={} expected_len>={} visible_len={} handle_len={}",
                path.display(),
                expected_min_len,
                meta.len(),
                handle_len
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

fn defer_upload_abort_until_worker_quiescence(task: &TransferTask) {
    task.require_upload_abort();
}

/// Validates the server-suggested cursor for the strict-serial upload path.
///
/// A server may acknowledge only a prefix of the chunk, but it cannot make the
/// cursor stand still, move backwards, or claim bytes that were not in this
/// request. Parallel uploads deliberately do not use this single-cursor rule.
fn validate_serial_upload_ack(
    offset: u64,
    uploaded_end: u64,
    total: u64,
    next_byte: Option<u64>,
) -> Result<u64, MeowError> {
    let next = next_byte.unwrap_or(uploaded_end);
    if next > total {
        return Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!("upload next offset exceeds total: next={next} total={total}"),
        ));
    }
    if next <= offset {
        return Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!("upload next offset did not advance: offset={offset} next={next}"),
        ));
    }
    if next > uploaded_end {
        return Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!(
                "upload next offset exceeds sent chunk: next={next} uploaded_end={uploaded_end}"
            ),
        ));
    }
    Ok(next)
}

async fn validate_serial_upload_before_last_part(
    task: &TransferTask,
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
            defer_upload_abort_until_worker_quiescence(task);
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
            defer_upload_abort_until_worker_quiescence(task);
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
    // Parallel progress is owned by the scheduler's local part grid. Keep the
    // historical outcome for compatibility; the scheduler deliberately ignores
    // this provider cursor when advancing its contiguous watermark.
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
            defer_upload_abort_until_worker_quiescence(task);
            return Err(error);
        }
    };
    let Some((chunk_bytes, uploaded_chunk_len)) = read else {
        if let Some(snapshot) = task.upload_file_snapshot() {
            if let Err(error) = snapshot.validate_generation(true).await {
                defer_upload_abort_until_worker_quiescence(task);
                return Err(error);
            }
        }
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
    validate_serial_upload_before_last_part(task, offset, uploaded_chunk_len).await?;
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
    // `completed_file_id` remains the protocol's authoritative terminal signal
    // (an existing public contract). Validate the cursor only when it controls
    // the next serial source range.
    let next = if info.completed_file_id.is_some() {
        total
    } else {
        validate_serial_upload_ack(offset, uploaded_end, total, info.next_byte).inspect_err(
            |_| {
                defer_upload_abort_until_worker_quiescence(task);
            },
        )?
    };
    if info.completed_file_id.is_some() || next >= total {
        if let Some(snapshot) = task.upload_file_snapshot() {
            // Metadata is never content proof (especially on stable Windows),
            // so every response that can become terminal gets a full digest
            // check after the provider call as well as the pre-last-part check.
            if let Err(error) = snapshot.validate_generation(true).await {
                defer_upload_abort_until_worker_quiescence(task);
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

fn require_content_range(headers: &HeaderMap, mode: &str) -> Result<String, MeowError> {
    require_single_header(headers, CONTENT_RANGE, mode)?
        .map(str::to_owned)
        .ok_or_else(|| {
            MeowError::from_code(
                InnerErrorCode::InvalidRange,
                format!("{mode} response missing content-range"),
            )
        })
}

fn validate_known_content_range_total(
    range_total: Option<u64>,
    expected_total: u64,
    mode: &str,
) -> Result<(), MeowError> {
    match range_total {
        Some(actual) if actual == expected_total => Ok(()),
        Some(actual) => Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!("{mode} content-range total mismatch: expected {expected_total}, got {actual}"),
        )),
        None => Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!(
                "{mode} content-range used wildcard total after total {expected_total} was known"
            ),
        )),
    }
}

fn validate_optional_content_length(
    headers: &HeaderMap,
    expected_len: u64,
    mode: &str,
) -> Result<(), MeowError> {
    let Some(value) = require_single_header(headers, CONTENT_LENGTH, mode)? else {
        // A correctly framed chunked response has no Content-Length and is
        // validated byte-for-byte by the streaming length guard below.
        return Ok(());
    };
    let actual = value.parse::<u64>().map_err(|_| {
        MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!("{mode} response contains an invalid content-length"),
        )
    })?;
    if actual != expected_len {
        return Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!("{mode} content-length mismatch: expected {expected_len}, got {actual}"),
        ));
    }
    Ok(())
}

fn validate_range_safety_headers(headers: &HeaderMap, mode: &str) -> Result<(), MeowError> {
    ensure_response_identity_encoding(headers, mode)?;
    // ETag remains optional for one-part compatibility, but it must never be
    // selected from an ambiguous set. Multi-part requirements are enforced by
    // `validate_or_latch_range_validator` after this cardinality check.
    let _ = require_unambiguous_etag(headers, mode)?;
    Ok(())
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
        task.finalize_download_content(total).await?;
        return Ok(ChunkOutcome {
            next_offset: offset,
            total_size: total,
            done: true,
            completion_payload: None,
        });
    }

    // A serial run may inherit non-contiguous durable parts from an earlier
    // parallel run. Once its preceding hole is filled, reuse the already
    // verified part instead of downloading and then comparing it again.
    let already_done = task
        .download_progress()
        .lock()
        .map_err(|_| {
            MeowError::from_code_str(
                InnerErrorCode::LockPoisoned,
                "download checkpoint lock poisoned",
            )
        })?
        .as_ref()
        .map(|progress| progress.is_done(offset))
        .unwrap_or(false);
    if already_done {
        let next = offset.saturating_add(chunk_size).min(total);
        if next >= total {
            task.finalize_download_content(total).await?;
        }
        return Ok(ChunkOutcome {
            next_offset: next,
            total_size: total,
            done: next >= total,
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
        let safe_body = crate::log::redact_secrets(&body);
        crate::log::emit_lazy(|| {
            crate::log::Log::error(
                "range_get",
                format!(
                    "invalid status for range GET: status={} offset={} spec={} body={}",
                    status, offset, spec, safe_body
                ),
            )
            .with_key(task.file_name())
            .with_offset(offset)
            .with_http_status(status.as_u16())
            .with_url(sanitized_range_url.as_str())
        });
        return Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!("download GET requires 206 Partial Content, got {status}: {safe_body}"),
        ));
    }
    validate_range_safety_headers(resp.headers(), "serial range")?;
    let content_range =
        require_content_range(resp.headers(), "serial range").inspect_err(|_| {
            crate::log::emit_lazy(|| {
                crate::log::Log::error(
                    "range_get",
                    format!(
                        "missing or ambiguous content-range header: offset={} spec={}",
                        offset, spec
                    ),
                )
                .with_key(task.file_name())
                .with_offset(offset)
                .with_url(sanitized_range_url.as_str())
            });
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
    validate_known_content_range_total(range_total, total, "serial range")?;
    let expected_len = range_end
        .checked_sub(range_start)
        .and_then(|len| len.checked_add(1))
        .ok_or_else(|| {
            MeowError::from_code_str(
                InnerErrorCode::InvalidRange,
                "content-range length overflow",
            )
        })?;
    validate_optional_content_length(resp.headers(), expected_len, "serial range")?;
    validate_or_latch_range_validator(
        task,
        download_generation_validator(resp.headers()),
        total > chunk_size,
        "serial",
    )?;

    let path = task.file_path();
    let mut slot = task.download_file_slot().lock().await;
    let f = slot.as_mut().ok_or_else(|| {
        MeowError::from_code_str(
            InnerErrorCode::InvalidTaskState,
            "locked download target missing during serial write",
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
    let mut digest = Sha256::new();
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
        digest.update(&chunk);
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
    let digest = digest.finalize().into();
    drop(slot);
    task.stage_serial_download_part(offset, digest).await?;
    if next >= total {
        task.finalize_download_content(total).await?;
    }
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

/// Downloads exactly one range chunk into memory, then serializes the short
/// positioned disk write through the task's one locked target handle. Network
/// receives remain concurrent while Windows never needs a conflicting handle.
pub(crate) async fn download_one_chunk_part_positioned(
    client: &reqwest::Client,
    task: &TransferTask,
    download: Arc<dyn BreakpointDownload + Send + Sync>,
    offset: u64,
    chunk_size: u64,
    remote_total_size: u64,
) -> Result<ChunkOutcome, MeowError> {
    let total = remote_total_size;
    if offset >= total {
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
        let safe_body = crate::log::redact_secrets(&body);
        return Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!("range GET requires 206, got {status}: {safe_body}"),
        ));
    }
    validate_range_safety_headers(resp.headers(), "parallel range")?;
    let content_range = require_content_range(resp.headers(), "parallel range")?;
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
    validate_known_content_range_total(range_total, total, "parallel range")?;
    let expected_len = range_end
        .checked_sub(range_start)
        .and_then(|len| len.checked_add(1))
        .ok_or_else(|| {
            MeowError::from_code_str(
                InnerErrorCode::InvalidRange,
                "content-range length overflow",
            )
        })?;
    validate_optional_content_length(resp.headers(), expected_len, "parallel range")?;
    // Every range in one positioned download must prove that it belongs to one
    // immutable remote generation. Run the latch only after every immutable
    // response header has passed validation, so malformed metadata cannot
    // mutate task progress even transiently.
    validate_or_latch_range_validator(
        task,
        download_generation_validator(resp.headers()),
        total > chunk_size,
        "parallel",
    )?;
    let part_end = offset.checked_add(expected_len).ok_or_else(|| {
        MeowError::from_code_str(InnerErrorCode::InvalidRange, "download part end overflow")
    })?;

    let capacity = usize::try_from(expected_len).map_err(|_| {
        MeowError::from_code_str(
            InnerErrorCode::InvalidRange,
            "parallel download part does not fit this architecture",
        )
    })?;
    let mut body = Vec::new();
    body.try_reserve_exact(capacity).map_err(|error| {
        MeowError::from_code(
            InnerErrorCode::IoError,
            format!("cannot allocate parallel download part: {error}"),
        )
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
        body.extend_from_slice(&chunk);
        digest.update(&chunk);
        written_len = next_written;
    }
    if written_len != expected_len {
        return Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!("range body short: expected {expected_len}, got {written_len}"),
        ));
    }
    task.write_and_stage_download_part(offset, &body, digest.finalize().into())
        .await?;
    Ok(ChunkOutcome {
        next_offset: part_end.min(total),
        total_size: total,
        done: part_end >= total,
        completion_payload: None,
    })
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use reqwest::header::{HeaderMap, HeaderValue, CONTENT_LENGTH, CONTENT_RANGE};

    use super::ensure_download_file_present;
    use super::map_reqwest;
    use super::parse_content_range;
    use super::range_spec;
    use super::require_content_range;
    use super::rollback_download_file;
    use super::validate_known_content_range_total;
    use super::validate_optional_content_length;
    use tokio::io::{AsyncSeekExt, AsyncWriteExt};

    #[tokio::test]
    async fn mapped_reqwest_error_preserves_type_without_secret_url() {
        const SECRET: &str = "RC_SECRET_REQWEST_CANARY";
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve port");
        let address = listener.local_addr().expect("reserved port address");
        drop(listener);
        let url = format!("http://{address}/object?sig={SECRET}&mode=range");
        let source = reqwest::Client::new()
            .get(url)
            .send()
            .await
            .expect_err("closed loopback port must fail");

        let error = map_reqwest(source);
        let concrete = error
            .source()
            .and_then(|source| source.downcast_ref::<reqwest::Error>())
            .expect("mapping must preserve reqwest::Error as the concrete source");
        let rendered = format!("{error}\n{error:?}\n{concrete}\n{concrete:?}");

        assert!(!rendered.contains(SECRET), "secret URL leaked: {rendered}");
        assert_eq!(error.code(), crate::error::InnerErrorCode::HttpError as i32);
        assert!(rendered.contains("error sending request"));
    }

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

    #[tokio::test]
    async fn serial_retry_truncates_uncommitted_tail() {
        let path = std::env::temp_dir().join(format!(
            "rusty_cat_serial_retry_truncate_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let mut bytes = vec![0_u8; 30];
        bytes[20..30].copy_from_slice(b"later-part");
        tokio::fs::write(&path, &bytes).await.expect("fixture");
        let mut file = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .await
            .expect("target");
        file.seek(std::io::SeekFrom::Start(10))
            .await
            .expect("seek failed part");
        file.write_all(b"partial")
            .await
            .expect("partial failed write");

        rollback_download_file(&mut file, 10, &path).await;

        assert_eq!(file.metadata().await.expect("metadata").len(), 10);
        let _ = tokio::fs::remove_file(path).await;
    }

    #[test]
    fn parse_content_range_ok() -> Result<(), crate::error::MeowError> {
        let (start, end, total) = parse_content_range("bytes 10-99/1000")?;
        assert_eq!(start, 10);
        assert_eq!(end, 99);
        assert_eq!(total, Some(1000));
        Ok(())
    }

    #[tokio::test]
    async fn same_length_visible_path_replacement_is_deferred_to_content_validation() {
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

        ensure_download_file_present(&handle, &path, 128)
            .await
            .expect("length guard is identity-neutral; final SHA-256 decides");
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

    #[test]
    fn content_range_header_requires_exactly_one_value() {
        let mut headers = HeaderMap::new();
        assert!(require_content_range(&headers, "unit range").is_err());

        headers.insert(CONTENT_RANGE, HeaderValue::from_static("bytes 0-3/8"));
        assert_eq!(
            require_content_range(&headers, "unit range").expect("single range"),
            "bytes 0-3/8"
        );

        for second in ["bytes 0-3/8", "bytes 1-4/8"] {
            let mut duplicated = headers.clone();
            duplicated.append(CONTENT_RANGE, HeaderValue::from_static(second));
            let error = require_content_range(&duplicated, "unit range")
                .expect_err("duplicate Content-Range must fail");
            assert_eq!(
                error.code(),
                crate::error::InnerErrorCode::InvalidRange as i32
            );
        }
    }

    #[test]
    fn known_total_requires_exact_matching_content_range_total() {
        validate_known_content_range_total(Some(8), 8, "unit range").expect("exact total is valid");
        for total in [None, Some(7), Some(9), Some(u64::MAX)] {
            let error = validate_known_content_range_total(total, 8, "unit range")
                .expect_err("wildcard or mismatching total must fail");
            assert_eq!(
                error.code(),
                crate::error::InnerErrorCode::InvalidRange as i32
            );
        }
    }

    #[test]
    fn optional_content_length_must_be_single_numeric_and_exact() {
        let mut headers = HeaderMap::new();
        validate_optional_content_length(&headers, 4, "unit range")
            .expect("missing Content-Length permits chunked framing");

        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("4"));
        validate_optional_content_length(&headers, 4, "unit range")
            .expect("exact Content-Length is valid");

        for value in ["3", "5", "not-a-number", "18446744073709551616"] {
            headers.insert(
                CONTENT_LENGTH,
                HeaderValue::from_str(value).expect("ASCII Content-Length fixture"),
            );
            let error = validate_optional_content_length(&headers, 4, "unit range")
                .expect_err("invalid or inconsistent Content-Length must fail");
            assert_eq!(
                error.code(),
                crate::error::InnerErrorCode::InvalidRange as i32
            );
        }

        for second in ["4", "5"] {
            let mut duplicated = HeaderMap::new();
            duplicated.insert(CONTENT_LENGTH, HeaderValue::from_static("4"));
            duplicated.append(CONTENT_LENGTH, HeaderValue::from_static(second));
            validate_optional_content_length(&duplicated, 4, "unit range")
                .expect_err("duplicate Content-Length must fail");
        }
    }

    mod request_headers {
        use std::sync::Arc;

        use reqwest::header::{HeaderValue, ACCEPT_ENCODING, RANGE};

        use super::super::build_download_range_request;
        use crate::download_trait::{BreakpointDownload, DownloadRangeGetCtx};
        use crate::error::{InnerErrorCode, MeowError};
        use crate::http_breakpoint::{
            BreakpointDownloadHttpConfig, DefaultStyleUpload, StandardRangeDownload,
        };
        use crate::inner::inner_task::InnerTask;
        use crate::transfer_task::TransferTask;
        use crate::DownloadPounceBuilder;

        async fn task_with(download: Arc<dyn BreakpointDownload + Send + Sync>) -> TransferTask {
            let pounce = DownloadPounceBuilder::new(
                "range-request-unit.bin",
                "./range-request-unit.bin",
                4,
                "https://protocol.invalid/range-request-unit",
            )
            .with_total_size(8)
            .with_breakpoint_download(Arc::clone(&download))
            .build();
            let inner = InnerTask::from_pounce(
                pounce,
                BreakpointDownloadHttpConfig::default(),
                None,
                Arc::new(DefaultStyleUpload::default()),
                download,
            )
            .await
            .expect("normalize range request task");
            TransferTask::from_inner(&inner)
        }

        struct AppendsGzip;

        impl BreakpointDownload for AppendsGzip {
            fn merge_range_get_headers(
                &self,
                ctx: DownloadRangeGetCtx<'_>,
            ) -> Result<(), MeowError> {
                ctx.base.insert(
                    RANGE,
                    HeaderValue::from_str(ctx.range_value).expect("range fixture"),
                );
                ctx.base
                    .append(ACCEPT_ENCODING, HeaderValue::from_static("gzip"));
                Ok(())
            }
        }

        #[tokio::test]
        async fn final_range_request_has_one_identity_encoding() {
            let download: Arc<dyn BreakpointDownload + Send + Sync> =
                Arc::new(StandardRangeDownload);
            let task = task_with(Arc::clone(&download)).await;
            let (_, headers) =
                build_download_range_request(download, &task, "bytes=0-3", task.headers().clone())
                    .await
                    .expect("standard request builder");

            assert_eq!(headers.get_all(ACCEPT_ENCODING).iter().count(), 1);
            assert_eq!(
                headers.get(ACCEPT_ENCODING),
                Some(&HeaderValue::from_static("identity"))
            );
        }

        #[tokio::test]
        async fn range_hook_cannot_append_an_ambiguous_encoding() {
            let download: Arc<dyn BreakpointDownload + Send + Sync> = Arc::new(AppendsGzip);
            let task = task_with(Arc::clone(&download)).await;
            let error =
                build_download_range_request(download, &task, "bytes=0-3", task.headers().clone())
                    .await
                    .expect_err("hook must preserve executor-owned encoding");

            assert_eq!(error.code(), InnerErrorCode::InvalidRange as i32);
        }
    }

    mod upload_ack {
        use std::sync::Arc;

        use async_trait::async_trait;

        use super::super::{upload_one_chunk, validate_serial_upload_ack};
        use crate::download_trait::BreakpointDownload;
        use crate::error::{InnerErrorCode, MeowError};
        use crate::http_breakpoint::{
            BreakpointDownloadHttpConfig, BreakpointUpload, DefaultStyleUpload,
            StandardRangeDownload, UploadResumeInfo,
        };
        use crate::inner::inner_task::InnerTask;
        use crate::transfer_task::TransferTask;
        use crate::up_pounce_builder::UploadPounceBuilder;
        use crate::upload_trait::{UploadChunkCtx, UploadPrepareCtx};

        struct FixedAck {
            next_byte: Option<u64>,
        }

        #[async_trait]
        impl BreakpointUpload for FixedAck {
            async fn prepare(
                &self,
                _ctx: UploadPrepareCtx<'_>,
            ) -> Result<UploadResumeInfo, MeowError> {
                Ok(UploadResumeInfo::default())
            }

            async fn upload_chunk(
                &self,
                _ctx: UploadChunkCtx<'_>,
            ) -> Result<UploadResumeInfo, MeowError> {
                Ok(UploadResumeInfo {
                    completed_file_id: None,
                    next_byte: self.next_byte,
                    provider_upload_id: None,
                })
            }
        }

        async fn task_with(upload: Arc<dyn BreakpointUpload + Send + Sync>) -> TransferTask {
            let pounce = UploadPounceBuilder::from_bytes("ack-unit.bin", vec![0; 8], 4)
                .with_url("https://protocol.invalid/ack-unit")
                .with_breakpoint_upload(upload)
                .with_max_upload_prepare_retries(0)
                .with_max_chunk_retries(0)
                .build()
                .expect("build ack task");
            let inner = InnerTask::from_pounce(
                pounce,
                BreakpointDownloadHttpConfig::default(),
                None,
                Arc::new(DefaultStyleUpload::default()),
                Arc::new(StandardRangeDownload) as Arc<dyn BreakpointDownload + Send + Sync>,
            )
            .await
            .expect("normalize ack task");
            TransferTask::from_inner(&inner)
        }

        #[tokio::test]
        async fn serial_ack_rejects_nonprogress_backward_gap_and_above_total() {
            for (offset, next, label) in [
                (0, 0, "non-progress"),
                (4, 2, "backward"),
                (0, 5, "forward gap"),
                (0, 9, "above total"),
            ] {
                let upload: Arc<dyn BreakpointUpload + Send + Sync> = Arc::new(FixedAck {
                    next_byte: Some(next),
                });
                let task = task_with(Arc::clone(&upload)).await;

                let error = upload_one_chunk(&reqwest::Client::new(), &task, upload, offset, 4)
                    .await
                    .expect_err("unsafe ack must fail");

                assert_eq!(error.code(), InnerErrorCode::InvalidRange as i32, "{label}");
                assert!(task.upload_abort_required(), "{label} must require abort");
            }
        }

        #[tokio::test]
        async fn serial_ack_accepts_none_partial_progress_and_exact_end() {
            for (next, expected, label) in [
                (None, 4, "none"),
                (Some(2), 2, "partial"),
                (Some(4), 4, "exact end"),
            ] {
                let upload: Arc<dyn BreakpointUpload + Send + Sync> =
                    Arc::new(FixedAck { next_byte: next });
                let task = task_with(Arc::clone(&upload)).await;

                let outcome = upload_one_chunk(&reqwest::Client::new(), &task, upload, 0, 4)
                    .await
                    .unwrap_or_else(|error| panic!("{label} ack failed: {error}"));

                assert_eq!(outcome.next_offset, expected, "{label}");
                assert!(!outcome.done, "{label}");
                assert!(!task.upload_abort_required(), "{label}");
            }
        }

        #[test]
        fn serial_ack_validator_handles_u64_boundary_without_clamping() {
            let offset = u64::MAX - 4;
            let uploaded_end = u64::MAX - 1;

            assert_eq!(
                validate_serial_upload_ack(offset, uploaded_end, u64::MAX, None)
                    .expect("implicit acknowledgement remains bounded"),
                uploaded_end
            );
            assert_eq!(
                validate_serial_upload_ack(offset, uploaded_end, u64::MAX, Some(uploaded_end))
                    .expect("exact acknowledgement remains bounded"),
                uploaded_end
            );
            assert!(
                validate_serial_upload_ack(offset, uploaded_end, u64::MAX, Some(offset)).is_err()
            );
            assert!(
                validate_serial_upload_ack(offset, uploaded_end, u64::MAX, Some(u64::MAX)).is_err()
            );
        }

        use proptest::prelude::*;

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(256))]

            #[test]
            fn serial_ack_only_accepts_the_strictly_advancing_sent_interval(
                offset in 0u64..u64::MAX / 2,
                sent_len in 1u64..1_048_576,
                extra_total in 0u64..1_048_576,
            ) {
                let uploaded_end = offset + sent_len;
                let total = uploaded_end + extra_total;

                prop_assert_eq!(
                    validate_serial_upload_ack(offset, uploaded_end, total, None)
                        .map_err(|error| error.code()),
                    Ok(uploaded_end)
                );

                let partial = offset + 1 + sent_len / 2;
                let partial = partial.min(uploaded_end);
                prop_assert_eq!(
                    validate_serial_upload_ack(offset, uploaded_end, total, Some(partial))
                        .map_err(|error| error.code()),
                    Ok(partial)
                );

                prop_assert!(
                    validate_serial_upload_ack(offset, uploaded_end, total, Some(offset)).is_err()
                );
                prop_assert!(validate_serial_upload_ack(
                    offset,
                    uploaded_end,
                    total.saturating_add(1),
                    Some(uploaded_end + 1),
                )
                .is_err());
            }
        }
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
