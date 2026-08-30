/// Internal default transport backend implementations.
pub(crate) mod default_http_transfer;
pub(crate) mod default_http_transfer_chunks;
pub(crate) mod download_progress;

use reqwest::header::{HeaderMap, ETAG};

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
    let etag = headers.get(ETAG)?.to_str().ok()?.trim();
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
    use reqwest::header::{HeaderMap, HeaderValue, ETAG};

    use super::download_generation_validator;

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
}
