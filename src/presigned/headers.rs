use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use crate::error::{InnerErrorCode, MeowError};

/// Builds a header map from static key-value pairs.
pub fn headers_from_pairs(pairs: &[(&str, &str)]) -> Result<HeaderMap, MeowError> {
    headers_from_iter(pairs.iter().copied())
}

/// Builds a header map from dynamic key-value pairs.
pub fn headers_from_iter<I, K, V>(pairs: I) -> Result<HeaderMap, MeowError>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let mut headers = HeaderMap::new();
    for (k, v) in pairs {
        let key = k.as_ref();
        let value = v.as_ref();
        let name = HeaderName::from_bytes(key.as_bytes()).map_err(|e| {
            MeowError::from_code(
                InnerErrorCode::ParameterEmpty,
                format!("invalid header name '{key}': {e}"),
            )
        })?;
        let value = HeaderValue::from_str(value).map_err(|e| {
            MeowError::from_code(
                InnerErrorCode::ParameterEmpty,
                format!("invalid header value for '{key}': {e}"),
            )
        })?;
        headers.insert(name, value);
    }
    Ok(headers)
}
