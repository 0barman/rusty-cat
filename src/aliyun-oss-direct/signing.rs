use hmac::{Hmac, Mac};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use sha2::{Digest, Sha256};

use super::constants::OSS_UNSIGNED_PAYLOAD;
use crate::{InnerErrorCode, MeowError};

type HmacSha256 = Hmac<Sha256>;

/// Derived OSS v4 signing-key material.
///
/// Wrapped in a newtype with a **redacted** `Debug` so the secret-derived bytes
/// can never be accidentally logged or printed.
#[derive(Clone)]
pub(crate) struct SecretKeyBytes(Vec<u8>);

impl SecretKeyBytes {
    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for SecretKeyBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretKeyBytes(<redacted>)")
    }
}

/// Formats the UTC date (`YYYYMMDD`) used in the OSS v4 credential scope.
pub(crate) fn oss_date(now: time::OffsetDateTime) -> String {
    let now = now.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}{:02}{:02}",
        now.year(),
        u8::from(now.month()),
        now.day()
    )
}

/// Returns the current UTC instant together with its `YYYYMMDD` date string.
///
/// Both come from a single `now_utc()` read so a cached date key and the
/// `x-oss-date` timestamp computed from the same instant can never straddle a UTC
/// midnight.
pub(crate) fn current_date() -> (time::OffsetDateTime, String) {
    let now = time::OffsetDateTime::now_utc();
    let date = oss_date(now);
    (now, date)
}

/// Derives the 4-step OSS v4 signing key.
///
/// The result depends only on `(access_key_secret, date, region)`, so callers may
/// cache it per UTC day; only the final `HMAC(signing_key, string_to_sign)` step
/// is per-request.
pub(crate) fn derive_signing_key(
    access_key_secret: &str,
    date: &str,
    region: &str,
) -> SecretKeyBytes {
    let signing_key = hmac_sha256(
        format!("aliyun_v4{access_key_secret}").as_bytes(),
        date.as_bytes(),
    );
    let signing_key = hmac_sha256(&signing_key, region.as_bytes());
    let signing_key = hmac_sha256(&signing_key, b"oss");
    let signing_key = hmac_sha256(&signing_key, b"aliyun_v4_request");
    SecretKeyBytes(signing_key)
}

/// Builds OSS v4 signed headers from a pre-derived signing key.
///
/// `now` must be the instant whose date produced `signing_key` (see
/// [`current_date`]); the `x-oss-date` timestamp and credential scope are derived
/// from it, keeping the signature internally consistent. Output is byte-identical
/// to deriving the key inline for the same `now`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn signed_headers_with_key(
    method: &str,
    canonical_uri: &str,
    raw_query: Option<&str>,
    sign_pairs: &[(&str, &str)],
    additional_headers: Option<&str>,
    access_key_id: &str,
    region: &str,
    now: time::OffsetDateTime,
    signing_key: &SecretKeyBytes,
) -> Result<HeaderMap, MeowError> {
    let iso8601 = format_iso8601_basic_z(now);
    let date = oss_date(now);
    let mut entries = vec![
        (
            "x-oss-content-sha256".to_string(),
            OSS_UNSIGNED_PAYLOAD.to_string(),
        ),
        ("x-oss-date".to_string(), iso8601.clone()),
    ];
    for (k, v) in sign_pairs {
        entries.push((k.to_ascii_lowercase(), (*v).to_string()));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let canonical_headers = entries
        .iter()
        .map(|(k, v)| format!("{k}:{v}\n"))
        .collect::<String>();
    let additional_headers = additional_headers.unwrap_or_default();
    let canonical_request = format!(
        "{method}\n{canonical_uri}\n{}\n{canonical_headers}\n{additional_headers}\n{OSS_UNSIGNED_PAYLOAD}",
        raw_query.unwrap_or_default()
    );
    let scope = format!("{date}/{region}/oss/aliyun_v4_request");
    let string_to_sign = format!(
        "OSS4-HMAC-SHA256\n{iso8601}\n{scope}\n{}",
        hex_encode(Sha256::digest(canonical_request.as_bytes()).as_slice())
    );
    let signature = hex_encode(&hmac_sha256(signing_key.as_slice(), string_to_sign.as_bytes()));
    let mut headers = HeaderMap::new();
    headers.insert("x-oss-date", header_value(&iso8601)?);
    headers.insert("x-oss-content-sha256", header_value(OSS_UNSIGNED_PAYLOAD)?);
    for (k, v) in sign_pairs {
        let name = HeaderName::from_bytes(k.as_bytes()).map_err(|e| {
            MeowError::from_code(
                InnerErrorCode::ParameterEmpty,
                format!("invalid header name '{k}': {e}"),
            )
        })?;
        headers.insert(name, header_value(v)?);
    }
    let mut auth = format!("OSS4-HMAC-SHA256 Credential={access_key_id}/{scope}");
    if !additional_headers.is_empty() {
        auth.push_str(",AdditionalHeaders=");
        auth.push_str(additional_headers);
    }
    auth.push_str(",Signature=");
    auth.push_str(&signature);
    headers.insert("authorization", header_value(&auth)?);
    Ok(headers)
}

/// Builds OSS v4 signed headers, deriving the signing key on every call.
///
/// Kept for callers that do not cache the signing key (e.g. the download path).
#[allow(clippy::too_many_arguments)]
pub(crate) fn signed_headers(
    method: &str,
    canonical_uri: &str,
    raw_query: Option<&str>,
    sign_pairs: &[(&str, &str)],
    additional_headers: Option<&str>,
    access_key_id: &str,
    access_key_secret: &str,
    region: &str,
) -> Result<HeaderMap, MeowError> {
    let now = time::OffsetDateTime::now_utc();
    let date = oss_date(now);
    let signing_key = derive_signing_key(access_key_secret, &date, region);
    signed_headers_with_key(
        method,
        canonical_uri,
        raw_query,
        sign_pairs,
        additional_headers,
        access_key_id,
        region,
        now,
        &signing_key,
    )
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn format_iso8601_basic_z(t: time::OffsetDateTime) -> String {
    let t = t.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

pub(crate) fn header_value(v: &str) -> Result<HeaderValue, MeowError> {
    HeaderValue::from_str(v).map_err(|e| {
        MeowError::from_code(
            InnerErrorCode::ParameterEmpty,
            format!("invalid header value: {e}"),
        )
    })
}

pub(crate) fn param_error(e: impl std::fmt::Display) -> MeowError {
    MeowError::from_code(InnerErrorCode::ParameterEmpty, e.to_string())
}

#[cfg(test)]
mod tests {
    use super::{derive_signing_key, hex_encode, signed_headers_with_key};

    #[test]
    fn test_hex_encode() {
        assert_eq!(hex_encode(&[0, 15, 16, 255]), "000f10ff");
    }

    #[test]
    fn derive_signing_key_is_date_sensitive() {
        // Same (secret, date, region) → identical key (cache hit is safe);
        // a different UTC date → different key (cache must invalidate at midnight).
        let a = derive_signing_key("secret", "20260627", "cn-hangzhou");
        let b = derive_signing_key("secret", "20260627", "cn-hangzhou");
        let c = derive_signing_key("secret", "20260628", "cn-hangzhou");
        assert_eq!(a.as_slice(), b.as_slice());
        assert_ne!(a.as_slice(), c.as_slice());
    }

    #[test]
    fn signed_headers_with_key_is_deterministic_for_fixed_now() {
        let now = time::OffsetDateTime::from_unix_timestamp(1_782_500_000)
            .expect("valid timestamp");
        let date = super::oss_date(now);
        let key = derive_signing_key("secret", &date, "cn-hangzhou");
        let make = || {
            signed_headers_with_key(
                "PUT",
                "/bucket/object",
                Some("partNumber=1&uploadId=abc"),
                &[],
                None,
                "akid",
                "cn-hangzhou",
                now,
                &key,
            )
            .expect("sign")
        };
        let h1 = make();
        let h2 = make();
        // Re-deriving the cached key and re-signing the same request at the same
        // instant must be byte-identical (signing is a pure function of inputs).
        assert_eq!(h1.get("authorization"), h2.get("authorization"));
        assert_eq!(h1.get("x-oss-date"), h2.get("x-oss-date"));
        let auth = h1
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(auth.starts_with("OSS4-HMAC-SHA256 Credential=akid/"));
        assert!(auth.contains("/cn-hangzhou/oss/aliyun_v4_request"));
    }

    #[test]
    fn secret_key_bytes_debug_is_redacted() {
        let key = derive_signing_key("secret", "20260627", "cn-hangzhou");
        assert_eq!(format!("{key:?}"), "SecretKeyBytes(<redacted>)");
    }
}
