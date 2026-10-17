/// Internal default transport backend implementations.
pub(crate) mod default_http_transfer;
pub(crate) mod default_http_transfer_chunks;
pub(crate) mod download_progress;

use reqwest::header::{
    HeaderMap, HeaderName, HeaderValue, ACCEPT_ENCODING, CONTENT_ENCODING, ETAG,
};

use crate::error::{InnerErrorCode, MeowError};

const IDENTITY_ENCODING: HeaderValue = HeaderValue::from_static("identity");

/// Returns the sole ASCII value for a security-sensitive response header.
///
/// HTTP header maps preserve repeated field lines. Picking one with `get()` is
/// unsafe for framing and representation metadata because intermediaries may
/// disagree about which value is authoritative. Absence remains distinct from
/// ambiguity so each protocol call site can retain its existing required vs.
/// optional policy.
pub(crate) fn require_single_header<'a>(
    headers: &'a HeaderMap,
    name: HeaderName,
    context: &str,
) -> Result<Option<&'a str>, MeowError> {
    let mut values = headers.get_all(&name).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!("{context} contains multiple {} headers", name.as_str()),
        ));
    }
    value.to_str().map(Some).map_err(|_| {
        MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!("{context} contains an invalid {} header", name.as_str()),
        )
    })
}

/// Returns the sole ASCII ETag value while rejecting a comma that appears outside
/// the opaque quoted tag. ETag is not a list-valued header, but an intermediary
/// can combine repeated field-lines into `"a", "b"`; treating that as one
/// strong validator would hide an ambiguous representation. A comma inside a
/// quoted opaque tag remains valid. A valid opaque ETag containing `obs-text`
/// remains optional (`None`) for one-part compatibility; it is not converted
/// lossily into the String validator used for multi-part and persisted resume.
pub(crate) fn require_unambiguous_etag<'a>(
    headers: &'a HeaderMap,
    context: &str,
) -> Result<Option<&'a str>, MeowError> {
    let mut values = headers.get_all(ETAG).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!("{context} contains multiple etag headers"),
        ));
    }
    // Inspect original bytes before the ASCII conversion so non-ASCII values
    // cannot conceal repeated field lines or a comma-combined field.
    let mut quoted = false;
    for byte in value.as_bytes() {
        match byte {
            b'"' => quoted = !quoted,
            b',' if !quoted => {
                return Err(MeowError::from_code(
                    InnerErrorCode::InvalidRange,
                    format!("{context} contains an ambiguous comma-combined ETag header"),
                ));
            }
            _ => {}
        }
    }
    if let Ok(ascii) = value.to_str() {
        return Ok(Some(ascii));
    }
    // RFC 9110 entity-tag permits obs-text inside the quoted opaque value.
    // Only that grammar receives compatibility treatment; malformed opaque
    // metadata must still fail before response bytes reach the target file.
    let bytes = value.as_bytes().trim_ascii();
    let bytes = match bytes.strip_prefix(b"W/") {
        Some(strong) => strong,
        None => bytes,
    };
    let opaque = bytes
        .strip_prefix(b"\"")
        .and_then(|tag| tag.strip_suffix(b"\""));
    if opaque.is_some_and(|tag| {
        tag.iter()
            .all(|byte| matches!(byte, b'!' | b'#'..=b'~' | 0x80..=0xff))
    }) {
        Ok(None)
    } else {
        Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!("{context} contains an invalid etag header"),
        ))
    }
}

/// Replaces any caller-provided content-coding preference with the executor's
/// byte-range invariant. `HeaderMap::insert` removes every prior value.
pub(crate) fn set_accept_encoding_identity(headers: &mut HeaderMap) {
    headers.insert(ACCEPT_ENCODING, IDENTITY_ENCODING);
}

/// Ensures an untrusted protocol hook preserved the executor-owned request
/// encoding exactly. The check runs after the hook so signed protocols can
/// include the final value in their signature without a post-sign mutation.
pub(crate) fn ensure_accept_encoding_identity(
    headers: &HeaderMap,
    context: &str,
) -> Result<(), MeowError> {
    match require_single_header(headers, ACCEPT_ENCODING, context)? {
        Some(value) if value.eq_ignore_ascii_case("identity") => Ok(()),
        Some(_) => Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!("{context} must preserve Accept-Encoding: identity"),
        )),
        None => Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!("{context} removed Accept-Encoding: identity"),
        )),
    }
}

/// Range bytes are meaningful only for the identity representation. Missing
/// `Content-Encoding` is the normal identity case; an explicit value must be a
/// single `identity` field.
pub(crate) fn ensure_response_identity_encoding(
    headers: &HeaderMap,
    context: &str,
) -> Result<(), MeowError> {
    match require_single_header(headers, CONTENT_ENCODING, context)? {
        None => Ok(()),
        Some(value) if value.eq_ignore_ascii_case("identity") => Ok(()),
        Some(value) => Err(MeowError::from_code(
            InnerErrorCode::InvalidRange,
            format!("{context} used content-encoding {value}"),
        )),
    }
}

/// Returns a representation validator that is safe to replay in `If-Match`.
///
/// Normal HTTP strong ETags are quoted. Azure Blob Storage fronted by Azure
/// Front Door can expose the same Blob ETag as an unquoted hexadecimal token
/// (for example `0x8DEFF4FCC6C92AC`) while still requiring that exact token in
/// `If-Match`. Accept that interoperability form only when the response also
/// carries Azure Blob's own protocol markers; token shape alone must never
/// upgrade an arbitrary origin to checkpoint-safe generation semantics. Weak
/// ETags and other unquoted values remain ineligible.
pub(crate) fn download_generation_validator(headers: &HeaderMap) -> Option<String> {
    // Callers that need to distinguish absence from ambiguity validate first
    // with `require_unambiguous_etag`. Keep this defensive check because this
    // private parser is also exercised independently by provider code/tests.
    let etag = require_unambiguous_etag(headers, "download response")
        .ok()
        .flatten()?
        .trim();
    if etag
        .get(..2)
        .map(|prefix| prefix.eq_ignore_ascii_case("W/"))
        .unwrap_or(false)
    {
        return None;
    }

    let quoted = etag.len() >= 2 && etag.starts_with('"') && etag.ends_with('"');
    let azure_token = etag
        .strip_prefix("0x")
        .or_else(|| etag.strip_prefix("0X"))
        .map(|hex| hex.len() >= 8 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .unwrap_or(false);
    let azure_blob_response = headers
        .get("x-ms-blob-type")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().eq_ignore_ascii_case("BlockBlob"))
        .unwrap_or(false)
        && headers.get("x-ms-version").is_some()
        && headers.get("x-ms-request-id").is_some();

    (quoted || (azure_token && azure_blob_response)).then(|| etag.to_owned())
}

#[cfg(test)]
mod validator_tests {
    use reqwest::header::{
        HeaderMap, HeaderName, HeaderValue, ACCEPT_ENCODING, CONTENT_ENCODING, ETAG,
    };

    use super::{
        download_generation_validator, ensure_accept_encoding_identity,
        ensure_response_identity_encoding, require_single_header, require_unambiguous_etag,
        set_accept_encoding_identity,
    };

    #[test]
    fn valid_opaque_etag_remains_optional_without_becoming_a_string_validator(
    ) -> Result<(), Box<dyn std::error::Error>> {
        for bytes in [
            b"\"v\x80\"".as_slice(),
            b"\"v,\xff\"".as_slice(),
            b"W/\"v\x80\"".as_slice(),
            b" \"v\x80\"\t".as_slice(),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(ETAG, HeaderValue::from_bytes(bytes)?);
            if require_unambiguous_etag(&headers, "opaque response")?.is_some() {
                return Err(
                    std::io::Error::other("opaque ETag cannot become an ASCII header").into(),
                );
            }
            if download_generation_validator(&headers).is_some() {
                return Err(std::io::Error::other(
                    "opaque ETag must not become a String generation validator",
                )
                .into());
            }
        }
        Ok(())
    }

    #[test]
    fn opaque_etag_rejects_duplicate_and_combined_values() -> Result<(), Box<dyn std::error::Error>>
    {
        for second in [b"\"v\x80\"".as_slice(), b"\"other\"".as_slice()] {
            let mut headers = HeaderMap::new();
            headers.insert(ETAG, HeaderValue::from_bytes(b"\"v\x80\"")?);
            headers.append(ETAG, HeaderValue::from_bytes(second)?);
            match require_unambiguous_etag(&headers, "opaque response") {
                Err(error) if error.code() == crate::error::InnerErrorCode::InvalidRange as i32 => {
                }
                outcome => {
                    return Err(std::io::Error::other(format!(
                        "duplicate ETag accepted: {outcome:?}"
                    ))
                    .into())
                }
            }
        }
        let mut headers = HeaderMap::new();
        headers.insert(ETAG, HeaderValue::from_bytes(b"\"v,\x80\", \"other\"")?);
        match require_unambiguous_etag(&headers, "opaque response") {
            Err(error) if error.code() == crate::error::InnerErrorCode::InvalidRange as i32 => {}
            outcome => {
                return Err(
                    std::io::Error::other(format!("combined ETag accepted: {outcome:?}")).into(),
                )
            }
        }
        Ok(())
    }

    #[test]
    fn malformed_opaque_etag_does_not_get_single_part_compatibility(
    ) -> Result<(), Box<dyn std::error::Error>> {
        for bytes in [
            b"v\x80".as_slice(),
            b"\"v\x80".as_slice(),
            b"\"v \x80\"".as_slice(),
            b"\"v\t\x80\"".as_slice(),
            b"\"v\"\x80\"".as_slice(),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(ETAG, HeaderValue::from_bytes(bytes)?);
            match require_unambiguous_etag(&headers, "opaque response") {
                Err(error) if error.code() == crate::error::InnerErrorCode::InvalidRange as i32 => {
                }
                outcome => {
                    return Err(std::io::Error::other(format!(
                        "malformed opaque ETag accepted: {outcome:?}"
                    ))
                    .into())
                }
            }
        }
        Ok(())
    }

    fn headers(etag: &'static str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(ETAG, HeaderValue::from_static(etag));
        headers
    }

    fn azure_headers(etag: &'static str) -> HeaderMap {
        let mut headers = headers(etag);
        headers.insert("x-ms-blob-type", HeaderValue::from_static("BlockBlob"));
        headers.insert("x-ms-version", HeaderValue::from_static("2009-09-19"));
        headers.insert(
            "x-ms-request-id",
            HeaderValue::from_static("68c7adf4-b01e-0047-1032-35fc95000000"),
        );
        headers
    }

    #[test]
    fn accepts_standard_strong_and_azure_front_door_etags() {
        assert_eq!(
            download_generation_validator(&headers("\"generation-a\"")),
            Some("\"generation-a\"".to_owned())
        );
        assert_eq!(
            download_generation_validator(&azure_headers("0x8DEFF4FCC6C92AC")),
            Some("0x8DEFF4FCC6C92AC".to_owned())
        );
    }

    #[test]
    fn rejects_weak_and_arbitrary_unquoted_etags() {
        for value in [
            "W/\"generation-a\"",
            "generation-a",
            "0x123",
            "0xnot-hex",
            "0xdeadbeef",
        ] {
            assert_eq!(download_generation_validator(&headers(value)), None);
        }
        assert_eq!(
            download_generation_validator(&azure_headers("W/\"generation-a\"")),
            None
        );
    }

    #[test]
    fn rejects_duplicate_or_conflicting_etag_values() {
        for second in ["\"generation-a\"", "\"generation-b\""] {
            let mut headers = headers("\"generation-a\"");
            headers.append(ETAG, HeaderValue::from_static(second));

            assert_eq!(
                download_generation_validator(&headers),
                None,
                "multiple ETag fields are ambiguous even when byte-identical"
            );
        }
    }

    #[test]
    fn rejects_comma_combined_etag_values() {
        let combined = headers("\"generation-a\", \"generation-b\"");
        assert_eq!(
            download_generation_validator(&combined),
            None,
            "ETag is not list-valued; a comma-combined field is ambiguous"
        );

        let valid = headers("\"generation,a\"");
        assert_eq!(
            require_unambiguous_etag(&valid, "unit response")
                .expect("comma inside an opaque tag is valid"),
            Some("\"generation,a\"")
        );
    }

    #[test]
    fn singleton_header_distinguishes_absent_single_invalid_and_multiple() {
        let name = HeaderName::from_static("x-range-metadata");
        let mut headers = HeaderMap::new();
        assert_eq!(
            require_single_header(&headers, name.clone(), "unit").expect("absent is valid"),
            None
        );

        headers.insert(name.clone(), HeaderValue::from_static("one"));
        assert_eq!(
            require_single_header(&headers, name.clone(), "unit").expect("single is valid"),
            Some("one")
        );

        headers.append(name.clone(), HeaderValue::from_static("two"));
        let duplicate =
            require_single_header(&headers, name.clone(), "unit").expect_err("duplicate fails");
        assert_eq!(
            duplicate.code(),
            crate::error::InnerErrorCode::InvalidRange as i32
        );

        let mut invalid = HeaderMap::new();
        invalid.insert(
            name.clone(),
            HeaderValue::from_bytes(b"\x80").expect("opaque header fixture"),
        );
        let invalid = require_single_header(&invalid, name, "unit")
            .expect_err("non-ASCII safety metadata fails");
        assert_eq!(
            invalid.code(),
            crate::error::InnerErrorCode::InvalidRange as i32
        );
    }

    #[test]
    fn executor_owned_accept_encoding_is_one_identity_value() {
        let mut headers = HeaderMap::new();
        headers.append(ACCEPT_ENCODING, HeaderValue::from_static("gzip"));
        headers.append(ACCEPT_ENCODING, HeaderValue::from_static("br"));
        set_accept_encoding_identity(&mut headers);

        ensure_accept_encoding_identity(&headers, "unit request").expect("identity is preserved");
        assert_eq!(headers.get_all(ACCEPT_ENCODING).iter().count(), 1);
        assert_eq!(
            headers.get(ACCEPT_ENCODING),
            Some(&HeaderValue::from_static("identity"))
        );

        headers.append(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
        ensure_accept_encoding_identity(&headers, "unit request")
            .expect_err("even an identical duplicate is ambiguous");
    }

    #[test]
    fn response_content_encoding_accepts_only_absent_or_single_identity() {
        let mut headers = HeaderMap::new();
        ensure_response_identity_encoding(&headers, "unit response")
            .expect("absent means identity");
        headers.insert(CONTENT_ENCODING, HeaderValue::from_static("identity"));
        ensure_response_identity_encoding(&headers, "unit response")
            .expect("explicit identity is valid");

        for value in ["gzip", "br"] {
            headers.insert(CONTENT_ENCODING, HeaderValue::from_static(value));
            ensure_response_identity_encoding(&headers, "unit response")
                .expect_err("encoded range response fails");
        }
        headers.insert(CONTENT_ENCODING, HeaderValue::from_static("identity"));
        headers.append(CONTENT_ENCODING, HeaderValue::from_static("identity"));
        ensure_response_identity_encoding(&headers, "unit response")
            .expect_err("duplicate identity is ambiguous");
    }
}
