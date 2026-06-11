use std::sync::Arc;

use bytes::Bytes;
use reqwest::header::CONTENT_RANGE;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::chunk_outcome::ChunkOutcome;
use crate::error::{InnerErrorCode, MeowError};
use crate::http_breakpoint::{
    BreakpointDownload, BreakpointUpload, DownloadRangeGetCtx, UploadChunkCtx,
};
use crate::transfer_task::TransferTask;
use crate::upload_source::UploadSource;

const MAX_ERROR_BODY_PREVIEW_BYTES: usize = 4096;

/// Maps `reqwest::Error` into SDK error type.
pub(crate) fn map_reqwest(e: reqwest::Error) -> MeowError {
    MeowError::from_source(InnerErrorCode::HttpError, e.to_string(), e)
}

async fn rollback_download_file(file: &mut File, offset: u64, path: &std::path::Path) {
    if let Err(e) = file.set_len(offset).await {
        crate::meow_flow_log!(
            "download_chunk",
            "rollback set_len failed: path={} offset={} err={}",
            path.display(),
            offset,
            e
        );
    }
    if let Err(e) = file.seek(std::io::SeekFrom::Start(offset)).await {
        crate::meow_flow_log!(
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
    path: &std::path::Path,
    expected_min_len: u64,
) -> Result<(), MeowError> {
    match tokio::fs::metadata(path).await {
        Ok(meta) if meta.len() >= expected_min_len => Ok(()),
        Ok(meta) => Err(MeowError::from_code(
            InnerErrorCode::LocalFileRemoved,
            format!(
                "download file replaced during transfer: path={} expected_len>={} visible_len={}",
                path.display(),
                expected_min_len,
                meta.len()
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

/// Uploads exactly one chunk and returns next transfer outcome.
///
/// Reuses [`TransferTask::upload_chunk_buf`] for the read buffer so consecutive
/// chunks on the same task avoid per-chunk `Vec` allocations.
pub(crate) async fn upload_one_chunk(
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

    let read_len_usize = read_len as usize;

    let (info, uploaded_chunk_len) = match task.upload_source() {
        Some(UploadSource::File(path)) => {
            // Reuse the per-task scratch Vec<u8> for the disk read, then build a
            // single `Bytes` handle: the later reqwest `Body::from(Bytes)` and the
            // `Bytes::clone` on retries are all zero-copy, fully avoiding the
            // protocol layer's `to_vec` allocation.
            let chunk_bytes = {
                let mut buf_guard = task.upload_chunk_buf().lock().await;
                if buf_guard.len() < read_len_usize {
                    buf_guard.resize(read_len_usize, 0);
                } else {
                    buf_guard.truncate(read_len_usize);
                }

                let mut slot = task.upload_file_slot().lock().await;
                if slot.is_none() {
                    let opened = File::open(path).await.map_err(|e| {
                        MeowError::from_io(
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
                        MeowError::from_io(
                            format!(
                                "seek upload source failed: offset={offset} path={}",
                                path.display()
                            ),
                            e,
                        )
                    })?;
                file.read_exact(&mut buf_guard).await.map_err(|e| {
                    MeowError::from_io(
                        format!("read upload source failed: path={}", path.display()),
                        e,
                    )
                })?;
                drop(slot);
                // Copy once into `Bytes` to release the reusable Vec<u8> buffer;
                // compared with the old implementation (protocol-layer `to_vec`)
                // the total allocation count is unchanged, but clone and retry
                // both become O(1).
                Bytes::copy_from_slice(&buf_guard[..])
            };

            let chunk_len = chunk_bytes.len() as u64;
            let info = upload
                .upload_chunk(UploadChunkCtx {
                    client,
                    task,
                    chunk: chunk_bytes,
                    offset,
                })
                .await?;
            (info, chunk_len)
        }
        Some(UploadSource::Bytes(bytes)) => {
            let start = offset as usize;
            let end = start + read_len_usize;
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
            let info = upload
                .upload_chunk(UploadChunkCtx {
                    client,
                    task,
                    chunk,
                    offset,
                })
                .await?;
            (info, chunk_len)
        }
        None => {
            return Err(MeowError::from_code_str(
                InnerErrorCode::ParameterEmpty,
                "upload task missing upload source",
            ));
        }
    };
    if info.completed_file_id.is_some() {
        return Ok(ChunkOutcome {
            next_offset: total,
            total_size: total,
            done: true,
            completion_payload: info.completed_file_id,
        });
    }
    let next = info
        .next_byte
        .unwrap_or(offset + uploaded_chunk_len)
        .min(total);
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
    let mut headers = task.headers().clone();
    download.merge_range_get_headers(DownloadRangeGetCtx {
        task,
        range_value: &spec,
        base: &mut headers,
    })?;

    let range_url = download.range_url(task);
    let mut resp = client
        .get(range_url)
        .headers(headers)
        .send()
        .await
        .map_err(map_reqwest)?;
    let status = resp.status();
    if status != reqwest::StatusCode::PARTIAL_CONTENT {
        let body = read_error_body_preview(&mut resp, MAX_ERROR_BODY_PREVIEW_BYTES).await;
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
    let expected_len = range_end - range_start + 1;

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
                return Err(map_reqwest(e));
            }
        };
        let Some(chunk) = chunk else {
            break;
        };
        if chunk.is_empty() {
            continue;
        }
        let next_written = written_len + chunk.len() as u64;
        if next_written > expected_len {
            rollback_download_file(f, offset, path).await;
            crate::meow_flow_log!(
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
    if written_len != expected_len {
        rollback_download_file(f, offset, path).await;
        crate::meow_flow_log!(
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

    let next = offset + written_len;
    // Detect a file deleted/replaced while this chunk was being written. On Unix
    // such a deletion does not fail `write_all` (the handle keeps an orphaned
    // inode alive), so without this check the failure would be silent.
    ensure_download_file_present(path, next).await?;
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

#[cfg(test)]
mod tests {
    use super::parse_content_range;

    #[test]
    fn parse_content_range_ok() -> Result<(), crate::error::MeowError> {
        let (start, end, total) = parse_content_range("bytes 10-99/1000")?;
        assert_eq!(start, 10);
        assert_eq!(end, 99);
        assert_eq!(total, Some(1000));
        Ok(())
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

            // Expected length equals the actual length: should pass.
            let result = ensure_download_file_present(&path, 1024).await;
            assert!(result.is_ok(), "expected Ok, got {result:?}");

            let _ = tokio::fs::remove_file(&path).await;
        }

        #[tokio::test]
        async fn replaced_shorter_file_is_local_file_removed() {
            let path = unique_temp_path("shrank");
            // Only 10 bytes on disk while we claim 4096 bytes "written" → the path
            // no longer points to the inode we are writing (deleted then recreated).
            tokio::fs::write(&path, vec![0u8; 10])
                .await
                .expect("write temp file");

            let err = ensure_download_file_present(&path, 4096)
                .await
                .expect_err("expected LocalFileRemoved error");
            assert_eq!(err.code(), InnerErrorCode::LocalFileRemoved as i32);

            let _ = tokio::fs::remove_file(&path).await;
        }

        #[tokio::test]
        async fn missing_file_is_local_file_removed() {
            let path = unique_temp_path("missing");
            // Intentionally skip creating the file, simulating a mid-transfer delete.
            let err = ensure_download_file_present(&path, 1)
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
