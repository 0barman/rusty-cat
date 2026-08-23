use std::io;
use std::time::Duration;

use bytes::Bytes;
use reqwest::header::{HeaderValue, CONTENT_TYPE};
use reqwest::redirect::Policy;
use tokio_util::sync::CancellationToken;

use crate::binary::binary_download_error::task_canceled;
use crate::binary::{BinaryDownloadConfig, BinaryDownloadOutput, BinaryTask};
use crate::error::{InnerErrorCode, MeowError};

const CONNECT_TIMEOUT_CAP: Duration = Duration::from_secs(10);
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const INITIAL_BODY_RESERVE_LIMIT: u64 = 1024 * 1024;
const UNKNOWN_BODY_INITIAL_RESERVE: usize = 64 * 1024;

pub(crate) fn build_client(
    timeout: Duration,
    keepalive: Duration,
    redirect_limit: usize,
) -> Result<reqwest::Client, MeowError> {
    let policy = Policy::custom(move |attempt| {
        let next = attempt.url();
        if let Some(message) = reject_redirect(attempt.previous(), next, redirect_limit) {
            return attempt.error(io::Error::other(message));
        }
        attempt.follow()
    });

    reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(timeout.min(CONNECT_TIMEOUT_CAP))
        .tcp_keepalive(keepalive)
        .pool_max_idle_per_host(4)
        .pool_idle_timeout(Some(POOL_IDLE_TIMEOUT))
        .redirect(policy)
        .build()
        .map_err(|_| {
            MeowError::from_code_str(
                InnerErrorCode::HttpClientBuildFailed,
                "build isolated binary HTTP client failed",
            )
        })
}

fn reject_redirect(
    previous: &[reqwest::Url],
    next: &reqwest::Url,
    redirect_limit: usize,
) -> Option<&'static str> {
    // `previous` includes the initial request URL, so the number of redirects
    // already followed is `previous.len() - 1`. Match reqwest's
    // `Policy::limited(max)` boundary: allow exactly `max` redirects and reject
    // the next one.
    if previous.len() > redirect_limit {
        return Some("binary redirect limit exceeded");
    }
    if !matches!(next.scheme(), "http" | "https")
        || !next.username().is_empty()
        || next.password().is_some()
    {
        return Some("unsafe binary redirect target");
    }
    if previous
        .last()
        .is_some_and(|url| url.scheme() == "https" && next.scheme() == "http")
    {
        return Some("HTTPS redirect downgrade rejected");
    }
    None
}

pub(crate) async fn download_with_retry(
    client: &reqwest::Client,
    task: &BinaryTask,
    parsed_url: &reqwest::Url,
    config: &BinaryDownloadConfig,
    cancel: &CancellationToken,
) -> Result<BinaryDownloadOutput, MeowError> {
    let mut attempt_index = 0usize;
    loop {
        let result = download_once(client, task, parsed_url, config.max_body_bytes(), cancel).await;
        match result {
            Ok(output) => return Ok(output),
            Err(AttemptFailure::Terminal(error)) => return Err(error),
            Err(AttemptFailure::Retryable(error)) => {
                let Some(delay) = config.retry_delays().get(attempt_index).copied() else {
                    return Err(error);
                };
                attempt_index += 1;
                tokio::select! {
                    _ = cancel.cancelled() => return Err(task_canceled()),
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
}

async fn download_once(
    client: &reqwest::Client,
    task: &BinaryTask,
    parsed_url: &reqwest::Url,
    global_max: u64,
    cancel: &CancellationToken,
) -> Result<BinaryDownloadOutput, AttemptFailure> {
    let max_body_bytes = task.effective_max_body_bytes(global_max);
    let request = client
        .get(parsed_url.clone())
        .headers(task.headers().clone());
    let response_result = tokio::select! {
        _ = cancel.cancelled() => return Err(AttemptFailure::Terminal(task_canceled())),
        response = request.send() => response,
    };
    let mut response = response_result.map_err(|error| {
        let mapped = MeowError::from_code_str(
            InnerErrorCode::HttpError,
            if error.is_timeout() {
                "binary HTTP attempt timed out"
            } else if error.is_redirect() {
                "binary HTTP redirect was rejected"
            } else {
                "binary HTTP request failed"
            },
        );
        if error.is_redirect() {
            AttemptFailure::Terminal(mapped)
        } else {
            AttemptFailure::Retryable(mapped)
        }
    })?;

    let status = response.status();
    if !status.is_success() {
        return Err(AttemptFailure::Terminal(
            MeowError::from_code(
                InnerErrorCode::ResponseStatusError,
                format!("binary HTTP response status was {}", status.as_u16()),
            )
            .with_http_status(status.as_u16()),
        ));
    }

    if response
        .content_length()
        .is_some_and(|length| length > max_body_bytes)
    {
        return Err(AttemptFailure::Terminal(body_too_large(max_body_bytes)));
    }
    let content_type: Option<HeaderValue> = response.headers().get(CONTENT_TYPE).cloned();
    let initial_capacity = bounded_initial_capacity(response.content_length(), max_body_bytes)
        .map_err(AttemptFailure::Terminal)?;
    let mut body = Vec::new();
    body.try_reserve_exact(initial_capacity)
        .map_err(|error| AttemptFailure::Terminal(allocation_failed(error)))?;
    let mut total = 0u64;

    loop {
        let chunk_result = tokio::select! {
            _ = cancel.cancelled() => return Err(AttemptFailure::Terminal(task_canceled())),
            chunk = response.chunk() => chunk,
        };
        let Some(chunk) = chunk_result.map_err(|_| {
            AttemptFailure::Retryable(MeowError::from_code_str(
                InnerErrorCode::HttpError,
                "binary response body read failed",
            ))
        })?
        else {
            break;
        };
        let chunk_len = u64::try_from(chunk.len())
            .map_err(|_| AttemptFailure::Terminal(body_too_large(max_body_bytes)))?;
        total = total
            .checked_add(chunk_len)
            .ok_or_else(|| AttemptFailure::Terminal(body_too_large(max_body_bytes)))?;
        if total > max_body_bytes {
            return Err(AttemptFailure::Terminal(body_too_large(max_body_bytes)));
        }
        let required = usize::try_from(total)
            .map_err(|_| AttemptFailure::Terminal(body_too_large(max_body_bytes)))?;
        if required > body.capacity() {
            let max_capacity = usize::try_from(max_body_bytes).unwrap_or(usize::MAX);
            let doubled = body
                .capacity()
                .max(UNKNOWN_BODY_INITIAL_RESERVE)
                .saturating_mul(2)
                .min(max_capacity);
            let target = required.max(doubled).min(max_capacity);
            body.try_reserve_exact(target.saturating_sub(body.len()))
                .map_err(|error| AttemptFailure::Terminal(allocation_failed(error)))?;
        }
        body.extend_from_slice(&chunk);
    }

    Ok(BinaryDownloadOutput::new(Bytes::from(body), content_type))
}

fn bounded_initial_capacity(
    content_length: Option<u64>,
    max_body_bytes: u64,
) -> Result<usize, MeowError> {
    let wanted = content_length
        .unwrap_or(0)
        .min(max_body_bytes)
        .min(INITIAL_BODY_RESERVE_LIMIT);
    usize::try_from(wanted).map_err(|_| body_too_large(max_body_bytes))
}

fn allocation_failed(error: std::collections::TryReserveError) -> MeowError {
    MeowError::from_code(
        InnerErrorCode::IoError,
        format!("binary response buffer allocation failed: {error}"),
    )
}

fn body_too_large(max_body_bytes: u64) -> MeowError {
    MeowError::from_code(
        InnerErrorCode::BinaryBodyTooLarge,
        format!("binary response exceeded {max_body_bytes} bytes"),
    )
}

enum AttemptFailure {
    Retryable(MeowError),
    Terminal(MeowError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_rejects_an_impossible_redirect_limit_only_through_config() {
        let config = BinaryDownloadConfig::builder().redirect_limit(11).build();
        assert!(config.is_err());
    }

    #[test]
    fn empty_bytes_remain_a_valid_output() {
        let output = BinaryDownloadOutput::new(bytes::Bytes::new(), None);
        assert!(output.bytes().is_empty());
    }

    #[test]
    fn redirect_safety_rejects_downgrade_credentials_and_unsupported_scheme() {
        let https = reqwest::Url::parse("https://example.com/start").unwrap();
        let http = reqwest::Url::parse("http://example.com/next").unwrap();
        assert_eq!(
            reject_redirect(std::slice::from_ref(&https), &http, 1),
            Some("HTTPS redirect downgrade rejected")
        );

        let credentials = reqwest::Url::parse("https://user:secret@example.com/next").unwrap();
        assert_eq!(
            reject_redirect(std::slice::from_ref(&https), &credentials, 1),
            Some("unsafe binary redirect target")
        );

        let file = reqwest::Url::parse("file:///tmp/data").unwrap();
        assert_eq!(
            reject_redirect(std::slice::from_ref(&https), &file, 1),
            Some("unsafe binary redirect target")
        );
    }

    #[test]
    fn redirect_safety_allows_exact_limit_and_rejects_the_next_redirect() {
        let initial = reqwest::Url::parse("https://example.com/start").unwrap();
        let first = reqwest::Url::parse("https://example.com/first").unwrap();
        let second = reqwest::Url::parse("https://example.com/second").unwrap();
        assert_eq!(
            reject_redirect(std::slice::from_ref(&initial), &first, 1),
            None
        );
        assert_eq!(
            reject_redirect(&[initial, first], &second, 1),
            Some("binary redirect limit exceeded")
        );
    }

    #[test]
    fn bounded_capacity_uses_known_medium_lengths_without_reserving_full_large_limit() {
        assert_eq!(
            bounded_initial_capacity(Some(0), 16 * 1024 * 1024).unwrap(),
            0
        );
        assert_eq!(
            bounded_initial_capacity(Some(63 * 1024), 16 * 1024 * 1024).unwrap(),
            63 * 1024
        );
        assert_eq!(
            bounded_initial_capacity(Some(65 * 1024), 16 * 1024 * 1024).unwrap(),
            65 * 1024
        );
        assert_eq!(
            bounded_initial_capacity(Some(16 * 1024 * 1024), 16 * 1024 * 1024).unwrap(),
            1024 * 1024
        );
        assert_eq!(bounded_initial_capacity(None, 16 * 1024 * 1024).unwrap(), 0);
    }

    #[test]
    fn u64_max_length_still_uses_architecture_safe_initial_reserve() {
        // Conversion failure is intentionally unreachable: clamp to the 1 MiB
        // initial reserve before converting to usize. This assertion exercises
        // the actual 32-bit-safe contract instead of conditionally expecting an
        // error from a value that can never reach `usize::try_from` unchanged.
        assert_eq!(
            bounded_initial_capacity(Some(u64::MAX), u64::MAX).unwrap(),
            1024 * 1024
        );
    }
}
