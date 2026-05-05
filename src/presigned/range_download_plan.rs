use reqwest::header::HeaderMap;

/// Provider-neutral presigned range-download plan.
#[derive(Debug, Clone)]
pub struct PresignedRangeDownloadPlan {
    /// Known remote object size. When present, download prepare skips HEAD.
    pub total_size: Option<u64>,
    /// Optional dedicated HEAD URL.
    pub head_url: Option<String>,
    /// URL used for range GET requests.
    pub range_url: String,
    /// Optional range URL expiration timestamp in Unix seconds.
    pub range_expires_at_unix_secs: Option<u64>,
    /// Refresh threshold in seconds. Range URL is refreshed before a chunk when
    /// `now + refresh_before_secs >= range_expires_at_unix_secs`.
    pub refresh_before_secs: u64,
    /// Headers added to HEAD requests.
    pub head_headers: HeaderMap,
    /// Headers added to range GET requests.
    pub range_headers: HeaderMap,
}

impl PresignedRangeDownloadPlan {
    /// Creates a plan using the same URL for range GET and optional HEAD.
    pub fn new(range_url: impl Into<String>) -> Self {
        Self {
            total_size: None,
            head_url: None,
            range_url: range_url.into(),
            range_expires_at_unix_secs: None,
            refresh_before_secs: 60,
            head_headers: HeaderMap::new(),
            range_headers: HeaderMap::new(),
        }
    }

    /// Sets known remote object size and enables HEAD skipping.
    pub fn with_total_size(mut self, total_size: u64) -> Self {
        self.total_size = Some(total_size);
        self
    }

    /// Sets dedicated HEAD URL.
    pub fn with_head_url(mut self, url: impl Into<String>) -> Self {
        self.head_url = Some(url.into());
        self
    }

    /// Sets range URL expiration timestamp in Unix seconds.
    pub fn with_range_expires_at_unix_secs(mut self, expires_at_unix_secs: u64) -> Self {
        self.range_expires_at_unix_secs = Some(expires_at_unix_secs);
        self
    }

    /// Sets range URL refresh threshold in seconds.
    pub fn with_refresh_before_secs(mut self, secs: u64) -> Self {
        self.refresh_before_secs = secs;
        self
    }

    /// Replaces HEAD headers.
    pub fn with_head_headers(mut self, headers: HeaderMap) -> Self {
        self.head_headers = headers;
        self
    }

    /// Replaces range GET headers.
    pub fn with_range_headers(mut self, headers: HeaderMap) -> Self {
        self.range_headers = headers;
        self
    }
}
