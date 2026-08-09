use bytes::Bytes;
use reqwest::header::{HeaderMap, HeaderValue};

use crate::binary::binary_download_config::BINARY_ABSOLUTE_MAX_BODY_BYTES;
use crate::error::{InnerErrorCode, MeowError};

/// One bounded in-memory HTTP GET request.
#[derive(Clone, Debug)]
pub struct BinaryTask {
    url: String,
    headers: HeaderMap,
    max_body_bytes: Option<u64>,
}

impl BinaryTask {
    /// Creates a task. URL validation happens synchronously during enqueue.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            headers: HeaderMap::new(),
            max_body_bytes: None,
        }
    }

    /// Replaces request headers.
    pub fn with_headers(mut self, headers: HeaderMap) -> Self {
        self.headers = headers;
        self
    }

    /// Adds or replaces one request header.
    pub fn with_header(mut self, name: reqwest::header::HeaderName, value: HeaderValue) -> Self {
        self.headers.insert(name, value);
        self
    }

    /// Applies a task-specific body limit. It may only tighten the client limit.
    pub fn with_max_body_bytes(mut self, max_body_bytes: u64) -> Self {
        self.max_body_bytes = Some(max_body_bytes);
        self
    }

    /// Returns the request URL exactly as supplied by the caller.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Returns request headers.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// Returns the task-specific body limit, if set.
    pub fn max_body_bytes(&self) -> Option<u64> {
        self.max_body_bytes
    }

    pub(crate) fn validate(&self, global_max: u64) -> Result<reqwest::Url, MeowError> {
        if self.url.trim().is_empty() {
            return Err(parameter_error("binary task URL must not be empty"));
        }
        let parsed = reqwest::Url::parse(&self.url)
            .map_err(|_| parameter_error("binary task URL is invalid"))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(parameter_error("binary task URL must use HTTP or HTTPS"));
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(parameter_error("binary task URL must not contain userinfo"));
        }
        if let Some(max) = self.max_body_bytes {
            if max == 0 || max > global_max || max > BINARY_ABSOLUTE_MAX_BODY_BYTES {
                return Err(parameter_error(
                    "binary task max_body_bytes must be within the client limit",
                ));
            }
        }
        Ok(parsed)
    }

    pub(crate) fn effective_max_body_bytes(&self, global_max: u64) -> u64 {
        self.max_body_bytes.unwrap_or(global_max)
    }
}

/// Successful result of a [`BinaryTask`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BinaryDownloadOutput {
    bytes: Bytes,
    content_type: Option<HeaderValue>,
}

impl BinaryDownloadOutput {
    pub(crate) fn new(bytes: Bytes, content_type: Option<HeaderValue>) -> Self {
        Self {
            bytes,
            content_type,
        }
    }

    /// Borrows the downloaded body.
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    /// Borrows the final successful response's `Content-Type` header.
    pub fn content_type(&self) -> Option<&HeaderValue> {
        self.content_type.as_ref()
    }

    /// Moves body and metadata out without copying the body.
    pub fn into_parts(self) -> (Bytes, Option<HeaderValue>) {
        (self.bytes, self.content_type)
    }
}

fn parameter_error(message: impl Into<String>) -> MeowError {
    MeowError::from_code(InnerErrorCode::ParameterEmpty, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_rejects_unsafe_urls_and_limits() {
        for url in [
            "",
            "not a url",
            "file:///tmp/a",
            "https://user@example.com/a",
        ] {
            assert!(BinaryTask::new(url).validate(1024).is_err(), "{url}");
        }
        assert!(BinaryTask::new("https://example.com")
            .with_max_body_bytes(1025)
            .validate(1024)
            .is_err());
    }

    #[test]
    fn output_into_parts_preserves_backing_storage() {
        let bytes = Bytes::from_static(b"binary");
        let ptr = bytes.as_ptr();
        let output = BinaryDownloadOutput::new(
            bytes,
            Some(HeaderValue::from_static("application/octet-stream")),
        );
        let (bytes, content_type) = output.into_parts();
        assert_eq!(bytes.as_ptr(), ptr);
        assert_eq!(
            content_type,
            Some(HeaderValue::from_static("application/octet-stream"))
        );
    }
}
