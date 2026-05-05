use hmac::{Hmac, Mac};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use sha2::{Digest, Sha256};

use super::constants::OSS_UNSIGNED_PAYLOAD;
use crate::{InnerErrorCode, MeowError};

type HmacSha256 = Hmac<Sha256>;

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
    let iso8601 = format_iso8601_basic_z(now);
    let date = format!(
        "{:04}{:02}{:02}",
        now.year(),
        u8::from(now.month()),
        now.day()
    );
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
    let signing_key = hmac_sha256(
        format!("aliyun_v4{access_key_secret}").as_bytes(),
        date.as_bytes(),
    );
    let signing_key = hmac_sha256(&signing_key, region.as_bytes());
    let signing_key = hmac_sha256(&signing_key, b"oss");
    let signing_key = hmac_sha256(&signing_key, b"aliyun_v4_request");
    let signature = hex_encode(&hmac_sha256(&signing_key, string_to_sign.as_bytes()));
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
    use super::hex_encode;

    #[test]
    fn test_hex_encode() {
        assert_eq!(hex_encode(&[0, 15, 16, 255]), "000f10ff");
    }
}
