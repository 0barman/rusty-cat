use reqwest::header::HeaderMap;
use reqwest::Method;

/// One HTTP request used for completion or abort callbacks.
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    /// HTTP method used for the callback.
    pub method: Method,
    /// Target URL.
    pub url: String,
    /// Request headers.
    pub headers: HeaderMap,
    /// Optional request body.
    pub body: Option<Vec<u8>>,
    /// When true and `body` is `None`, completion sends a JSON payload with
    /// `upload_id`, sizes, and uploaded part metadata (`ETag`, provider id).
    pub uploaded_parts_json_body: bool,
}

impl CompletionRequest {
    /// Creates a request without body.
    pub fn new(method: Method, url: impl Into<String>) -> Self {
        Self {
            method,
            url: url.into(),
            headers: HeaderMap::new(),
            body: None,
            uploaded_parts_json_body: false,
        }
    }

    /// Replaces request headers.
    pub fn with_headers(mut self, headers: HeaderMap) -> Self {
        self.headers = headers;
        self
    }

    /// Sets request body bytes.
    pub fn with_body(mut self, body: Vec<u8>) -> Self {
        self.body = Some(body);
        self
    }

    /// Uses an auto-generated JSON body containing uploaded part metadata.
    ///
    /// This is intended for presigned multipart flows where the application
    /// server performs final merge after receiving the uploaded `partNumber` +
    /// `ETag` list from the client.
    pub fn with_uploaded_parts_json_body(mut self) -> Self {
        self.uploaded_parts_json_body = true;
        self
    }
}
