use reqwest::header::HeaderMap;
use reqwest::Method;

/// A single presigned upload part.
#[derive(Debug, Clone)]
pub struct PresignedUploadPart {
    /// Provider-defined part number, usually 1-based for OSS/S3 or 0-based for
    /// local scheduling.
    pub part_number: u64,
    /// Start offset in the full file.
    pub offset: u64,
    /// Expected byte size of this part.
    pub size: u64,
    /// HTTP method, usually PUT.
    pub method: Method,
    /// Full presigned URL for this part.
    pub url: String,
    /// Optional provider-specific part identifier, for example Azure block id.
    pub provider_part_id: Option<String>,
    /// Optional URL expiration timestamp in Unix seconds.
    pub expires_at_unix_secs: Option<u64>,
    /// Headers that must be sent exactly as they were signed.
    pub headers: HeaderMap,
}

impl PresignedUploadPart {
    /// Creates a PUT part with empty headers.
    pub fn put(part_number: u64, offset: u64, size: u64, url: impl Into<String>) -> Self {
        Self {
            part_number,
            offset,
            size,
            method: Method::PUT,
            url: url.into(),
            provider_part_id: None,
            expires_at_unix_secs: None,
            headers: HeaderMap::new(),
        }
    }

    /// Sets provider-specific part identifier, for example Azure block id.
    pub fn with_provider_part_id(mut self, id: impl Into<String>) -> Self {
        self.provider_part_id = Some(id.into());
        self
    }

    /// Sets URL expiration timestamp in Unix seconds.
    pub fn with_expires_at_unix_secs(mut self, expires_at_unix_secs: u64) -> Self {
        self.expires_at_unix_secs = Some(expires_at_unix_secs);
        self
    }

    /// Replaces part headers.
    pub fn with_headers(mut self, headers: HeaderMap) -> Self {
        self.headers = headers;
        self
    }
}
