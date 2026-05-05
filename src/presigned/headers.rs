use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use crate::error::{InnerErrorCode, MeowError};

/// Builds a header map from static key-value pairs.
pub fn headers_from_pairs(pairs: &[(&str, &str)]) -> Result<HeaderMap, MeowError> {
    let mut headers = HeaderMap::new();
    for (k, v) in pairs {
        let name = HeaderName::from_bytes(k.as_bytes()).map_err(|e| {
            MeowError::from_code(
                InnerErrorCode::ParameterEmpty,
                format!("invalid header name '{k}': {e}"),
            )
        })?;
        let value = HeaderValue::from_str(v).map_err(|e| {
            MeowError::from_code(
                InnerErrorCode::ParameterEmpty,
                format!("invalid header value for '{k}': {e}"),
            )
        })?;
        headers.insert(name, value);
    }
    Ok(headers)
}
