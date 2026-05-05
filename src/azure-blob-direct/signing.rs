use std::collections::BTreeMap;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use hmac::{Hmac, Mac};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::Url;
use sha2::Sha256;

use super::constants::{HEADER_AUTHORIZATION, HEADER_MS_DATE, HEADER_MS_VERSION, MS_VERSION};
use super::time_util::now_rfc1123_gmt;
use crate::{InnerErrorCode, MeowError};

type HmacSha256 = Hmac<Sha256>;

pub(crate) fn signed_headers(
    method: &str,
    url: &Url,
    content_length: Option<usize>,
    content_type: Option<&str>,
    extra_headers: &[(&str, &str)],
    account_name: &str,
    account_key_b64: &str,
) -> Result<HeaderMap, MeowError> {
    let mut headers = HeaderMap::new();
    insert_header(&mut headers, HEADER_MS_VERSION, MS_VERSION)?;
    insert_header(&mut headers, HEADER_MS_DATE, now_rfc1123_gmt().as_str())?;
    if let Some(v) = content_type {
        insert_header(&mut headers, "content-type", v)?;
    }
    if let Some(v) = content_length {
        insert_header(&mut headers, "content-length", v.to_string().as_str())?;
    }
    for (k, v) in extra_headers {
        insert_header(&mut headers, k, v)?;
    }
    let authorization = build_authorization(method, url, &headers, account_name, account_key_b64)?;
    insert_header(&mut headers, HEADER_AUTHORIZATION, authorization.as_str())?;
    Ok(headers)
}

pub(crate) fn apply_signed_headers(
    task_url: &str,
    method: &str,
    base: &mut HeaderMap,
    account_name: &str,
    account_key_b64: &str,
) -> Result<(), MeowError> {
    let url = Url::parse(task_url).map_err(|e| {
        MeowError::from_code(
            InnerErrorCode::ParameterEmpty,
            format!("invalid azure blob url: {task_url} ({e})"),
        )
    })?;
    insert_header(base, HEADER_MS_VERSION, MS_VERSION)?;
    insert_header(base, HEADER_MS_DATE, now_rfc1123_gmt().as_str())?;
    let auth = build_authorization(method, &url, base, account_name, account_key_b64)?;
    insert_header(base, HEADER_AUTHORIZATION, auth.as_str())?;
    Ok(())
}

fn build_authorization(
    method: &str,
    url: &Url,
    headers: &HeaderMap,
    account_name: &str,
    account_key_b64: &str,
) -> Result<String, MeowError> {
    let canonicalized_headers = canonicalized_headers(headers)?;
    let canonicalized_resource = canonicalized_resource(url, account_name);
    let string_to_sign = format!(
        "{method}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{canonicalized_headers}{canonicalized_resource}",
        header_map_value(headers, "content-encoding"),
        header_map_value(headers, "content-language"),
        canonicalized_content_length(headers),
        header_map_value(headers, "content-md5"),
        header_map_value(headers, "content-type"),
        "",
        header_map_value(headers, "if-modified-since"),
        header_map_value(headers, "if-match"),
        header_map_value(headers, "if-none-match"),
        header_map_value(headers, "if-unmodified-since"),
        header_map_value(headers, "range"),
    );
    let key = BASE64_STANDARD.decode(account_key_b64).map_err(|e| {
        MeowError::from_code(
            InnerErrorCode::ParameterEmpty,
            format!("decode azure account key failed: {e}"),
        )
    })?;
    let mut mac = HmacSha256::new_from_slice(&key).map_err(|e| {
        MeowError::from_code(
            InnerErrorCode::ParameterEmpty,
            format!("build HMAC-SHA256 failed: {e}"),
        )
    })?;
    mac.update(string_to_sign.as_bytes());
    let signature = BASE64_STANDARD.encode(mac.finalize().into_bytes());
    Ok(format!("SharedKey {account_name}:{signature}"))
}

fn canonicalized_headers(headers: &HeaderMap) -> Result<String, MeowError> {
    let mut pairs = Vec::new();
    for (k, v) in headers {
        let k = k.as_str().to_ascii_lowercase();
        if !k.starts_with("x-ms-") {
            continue;
        }
        let value = v.to_str().map_err(|e| {
            MeowError::from_code(
                InnerErrorCode::ParameterEmpty,
                format!("x-ms header is not valid ASCII: {e}"),
            )
        })?;
        pairs.push((k, value.trim().to_string()));
    }
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = String::new();
    for (k, v) in pairs {
        out.push_str(&k);
        out.push(':');
        out.push_str(&v);
        out.push('\n');
    }
    Ok(out)
}

fn canonicalized_resource(url: &Url, account_name: &str) -> String {
    let mut out = format!("/{account_name}{}", url.path());
    let mut query_map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (k, v) in url.query_pairs() {
        query_map
            .entry(k.to_ascii_lowercase())
            .or_default()
            .push(v.into_owned());
    }
    for (k, mut values) in query_map {
        values.sort();
        out.push('\n');
        out.push_str(&k);
        out.push(':');
        out.push_str(&values.join(","));
    }
    out
}

fn canonicalized_content_length(headers: &HeaderMap) -> String {
    let raw = header_map_value(headers, "content-length");
    if raw == "0" {
        String::new()
    } else {
        raw
    }
}

fn header_map_value(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

pub(crate) fn insert_header(
    headers: &mut HeaderMap,
    name: &str,
    value: &str,
) -> Result<(), MeowError> {
    let name = HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
        MeowError::from_code(
            InnerErrorCode::ParameterEmpty,
            format!("invalid header name '{name}': {e}"),
        )
    })?;
    let value = HeaderValue::from_str(value).map_err(|e| {
        MeowError::from_code(
            InnerErrorCode::ParameterEmpty,
            format!("invalid header value for '{name}': {e}"),
        )
    })?;
    headers.insert(name, value);
    Ok(())
}

pub(crate) fn header_value(v: &str) -> Result<HeaderValue, MeowError> {
    HeaderValue::from_str(v).map_err(|e| {
        MeowError::from_code(
            InnerErrorCode::ParameterEmpty,
            format!("invalid header value: {e}"),
        )
    })
}
