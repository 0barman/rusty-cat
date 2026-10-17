use crate::{InnerErrorCode, MeowError};

pub(crate) const MAX_PROVIDER_CONTROL_BODY_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_PROVIDER_ERROR_BODY_BYTES: usize = 64 * 1024;

const ERROR_BODY_TRUNCATED: &str = "[provider error body truncated]";

/// Rejects caller-supplied query keys that the provider implementation owns.
/// The check uses decoded query names, so percent-encoded aliases cannot evade
/// it, while values are never copied into the resulting error.
pub(crate) fn reject_reserved_query_keys(
    url: &reqwest::Url,
    reserved: &[&str],
    provider: &str,
) -> Result<(), MeowError> {
    if let Some(key) = url.query_pairs().find_map(|(key, _)| {
        reserved
            .iter()
            .any(|reserved| key.eq_ignore_ascii_case(reserved))
            .then(|| key.into_owned())
    }) {
        return Err(MeowError::from_code(
            InnerErrorCode::InvalidTaskState,
            format!("{provider} task URL contains reserved query key '{key}'"),
        ));
    }
    Ok(())
}

/// Appends encoded provider-owned pairs without parsing/re-serializing the
/// caller's raw query. This preserves duplicate non-reserved keys, empty/bare
/// values, `+`, and existing percent escapes byte-for-byte.
pub(crate) fn append_query_pairs_preserving_raw(
    url: &mut reqwest::Url,
    pairs: &[(&str, String)],
) -> String {
    let existing = url.query().unwrap_or_default().to_owned();
    url.set_query(None);
    {
        let mut query = url.query_pairs_mut();
        for (key, value) in pairs {
            query.append_pair(key, value);
        }
    }
    // `Url::query_pairs_mut` uses form encoding and spells a space as `+`.
    // Provider signatures use RFC 3986 query encoding, so normalize only the
    // pairs generated here; the caller's original raw query stays byte-exact.
    let generated = url.query().unwrap_or_default().replace('+', "%20");
    let combined = match (existing.is_empty(), generated.is_empty()) {
        (false, false) => format!("{existing}&{generated}"),
        (false, true) => existing,
        (true, false) => generated,
        (true, true) => String::new(),
    };
    url.set_query(Some(&combined));
    combined
}

/// Appends an already-encoded provider query component (for OSS's historical
/// bare `uploads` subresource) while leaving the caller query untouched.
#[cfg(feature = "aliyun-oss-direct")]
pub(crate) fn append_raw_query_component(url: &mut reqwest::Url, component: &str) -> String {
    let combined = match url.query() {
        Some(existing) if !existing.is_empty() => format!("{existing}&{component}"),
        _ => component.to_owned(),
    };
    url.set_query(Some(&combined));
    combined
}

fn checked_body_len(current: usize, incoming: usize, limit: usize) -> Result<usize, MeowError> {
    let next = current.checked_add(incoming).ok_or_else(|| {
        MeowError::from_code_str(
            InnerErrorCode::ResponseParseError,
            "provider control response body length overflow",
        )
    })?;
    if next > limit {
        return Err(MeowError::from_code(
            InnerErrorCode::ResponseParseError,
            format!("provider control response body exceeds {limit} bytes"),
        ));
    }
    Ok(next)
}

fn decode_control_body(body: Vec<u8>, operation: &'static str) -> Result<String, MeowError> {
    String::from_utf8(body).map_err(|_| {
        MeowError::from_code(
            InnerErrorCode::ResponseParseError,
            format!("{operation} response body is not valid UTF-8"),
        )
    })
}

/// Reads a successful provider control-plane response with a hard byte cap.
///
/// Callers inspect non-success status responses separately so their existing
/// public status/error mapping remains unchanged.
pub(crate) async fn read_control_body(
    mut response: reqwest::Response,
    operation: &'static str,
) -> Result<String, MeowError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PROVIDER_CONTROL_BODY_BYTES as u64)
    {
        return Err(MeowError::from_code(
            InnerErrorCode::ResponseParseError,
            format!("{operation} response body exceeds {MAX_PROVIDER_CONTROL_BODY_BYTES} bytes"),
        ));
    }
    let initial_capacity = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or_default()
        .min(MAX_PROVIDER_CONTROL_BODY_BYTES);
    let mut body = Vec::with_capacity(initial_capacity);
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        let error = error.without_url();
        MeowError::from_source(
            InnerErrorCode::HttpError,
            format!("{operation} response body read failed"),
            error,
        )
    })? {
        let next = checked_body_len(body.len(), chunk.len(), MAX_PROVIDER_CONTROL_BODY_BYTES)?;
        body.extend_from_slice(&chunk);
        debug_assert_eq!(body.len(), next);
    }
    decode_control_body(body, operation)
}

fn append_bounded(body: &mut Vec<u8>, chunk: &[u8], limit: usize) -> bool {
    let remaining = limit.saturating_sub(body.len());
    let take = remaining.min(chunk.len());
    body.extend_from_slice(&chunk[..take]);
    take < chunk.len()
}

fn declared_error_body_is_oversize(declared_length: Option<u64>) -> bool {
    declared_length.is_some_and(|length| length > MAX_PROVIDER_ERROR_BODY_BYTES as u64)
}

fn should_mark_error_body_truncated(
    dropped_chunk_bytes: bool,
    collected_len: usize,
    declared_length: Option<u64>,
) -> bool {
    dropped_chunk_bytes
        || (collected_len == MAX_PROVIDER_ERROR_BODY_BYTES
            && declared_length != Some(MAX_PROVIDER_ERROR_BODY_BYTES as u64))
}

/// Reads enough of a provider error response for bounded diagnostics. A
/// declared oversize body is not read, and streaming bodies are dropped once
/// the diagnostic byte cap is reached.
pub(crate) async fn read_error_body(
    mut response: reqwest::Response,
) -> Result<String, reqwest::Error> {
    let declared_length = response.content_length();
    if declared_error_body_is_oversize(declared_length) {
        return Ok(ERROR_BODY_TRUNCATED.to_owned());
    }

    let initial_capacity = declared_length
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or_default()
        .min(MAX_PROVIDER_ERROR_BODY_BYTES);
    let mut body = Vec::with_capacity(initial_capacity);
    while let Some(chunk) = response.chunk().await? {
        let truncated = append_bounded(&mut body, &chunk, MAX_PROVIDER_ERROR_BODY_BYTES);
        if truncated || body.len() == MAX_PROVIDER_ERROR_BODY_BYTES {
            let mark_truncated =
                should_mark_error_body_truncated(truncated, body.len(), declared_length);
            let mut body = String::from_utf8_lossy(&body).into_owned();
            if mark_truncated {
                body.push_str(ERROR_BODY_TRUNCATED);
            }
            return Ok(body);
        }
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "aliyun-oss-direct")]
    use super::append_raw_query_component;
    use super::{
        append_bounded, append_query_pairs_preserving_raw, checked_body_len,
        declared_error_body_is_oversize, decode_control_body, reject_reserved_query_keys,
        should_mark_error_body_truncated, MAX_PROVIDER_CONTROL_BODY_BYTES,
        MAX_PROVIDER_ERROR_BODY_BYTES,
    };
    use crate::InnerErrorCode;

    #[test]
    fn exact_control_body_limit_is_accepted() {
        assert_eq!(
            checked_body_len(
                MAX_PROVIDER_CONTROL_BODY_BYTES - 1,
                1,
                MAX_PROVIDER_CONTROL_BODY_BYTES
            )
            .unwrap(),
            MAX_PROVIDER_CONTROL_BODY_BYTES
        );
    }

    #[test]
    fn provider_query_append_preserves_caller_raw_spelling() {
        let mut url = reqwest::Url::parse(
            "https://example.test/o?dup=one&dup=two&empty=&bare&plus=a+b&slash=%2F&amp=%26",
        )
        .unwrap();
        let query = append_query_pairs_preserving_raw(
            &mut url,
            &[
                ("partNumber", "1".to_owned()),
                ("uploadId", "a/b&c".to_owned()),
                ("prefix", "objects/a b+c".to_owned()),
            ],
        );
        assert_eq!(
            query,
            "dup=one&dup=two&empty=&bare&plus=a+b&slash=%2F&amp=%26&partNumber=1&uploadId=a%2Fb%26c&prefix=objects%2Fa%20b%2Bc"
        );

        #[cfg(feature = "aliyun-oss-direct")]
        {
            let mut bare = reqwest::Url::parse("https://example.test/o?tenant=a+b").unwrap();
            assert_eq!(
                append_raw_query_component(&mut bare, "uploads"),
                "tenant=a+b&uploads"
            );
        }
    }

    #[test]
    fn provider_reserved_query_detection_covers_encoded_duplicate_and_empty_forms() {
        for query in [
            "uploadId=",
            "uploadId=one&uploadId=two",
            "%75ploadId=a+b",
            "partNumber=%2F",
            "COMP=%26",
            "blockid=",
        ] {
            let url = reqwest::Url::parse(&format!("https://example.test/o?{query}")).unwrap();
            let error = reject_reserved_query_keys(
                &url,
                &["uploadId", "partNumber", "comp", "blockid"],
                "provider",
            )
            .expect_err("reserved key must be rejected");
            assert_eq!(error.code(), InnerErrorCode::InvalidTaskState as i32);
        }
    }

    #[test]
    fn control_body_limit_and_arithmetic_overflow_fail_closed() {
        assert!(checked_body_len(
            MAX_PROVIDER_CONTROL_BODY_BYTES,
            1,
            MAX_PROVIDER_CONTROL_BODY_BYTES
        )
        .is_err());
        assert!(checked_body_len(usize::MAX, 1, usize::MAX).is_err());
    }

    #[test]
    fn control_body_requires_strict_utf8() {
        assert_eq!(
            decode_control_body(b"<Root>ok</Root>".to_vec(), "provider")
                .unwrap()
                .as_str(),
            "<Root>ok</Root>"
        );
        let error = decode_control_body(vec![b'<', 0xff, b'>'], "provider")
            .expect_err("invalid UTF-8 must not be repaired before XML parsing");
        assert_eq!(error.code(), InnerErrorCode::ResponseParseError as i32);
    }

    #[test]
    fn error_body_never_grows_past_its_limit() {
        let mut body = vec![b'a'; MAX_PROVIDER_ERROR_BODY_BYTES - 1];
        assert!(!append_bounded(
            &mut body,
            b"b",
            MAX_PROVIDER_ERROR_BODY_BYTES
        ));
        assert_eq!(body.len(), MAX_PROVIDER_ERROR_BODY_BYTES);

        assert!(append_bounded(
            &mut body,
            b"overflow",
            MAX_PROVIDER_ERROR_BODY_BYTES
        ));
        assert_eq!(body.len(), MAX_PROVIDER_ERROR_BODY_BYTES);
    }

    #[test]
    fn known_exact_error_body_limit_is_complete_but_limit_plus_one_is_truncated() {
        let limit = MAX_PROVIDER_ERROR_BODY_BYTES as u64;
        assert!(!declared_error_body_is_oversize(Some(limit)));
        assert!(!should_mark_error_body_truncated(
            false,
            MAX_PROVIDER_ERROR_BODY_BYTES,
            Some(limit)
        ));

        assert!(declared_error_body_is_oversize(Some(limit + 1)));
        assert!(should_mark_error_body_truncated(
            false,
            MAX_PROVIDER_ERROR_BODY_BYTES,
            None
        ));
    }
}
