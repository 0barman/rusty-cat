use std::time::Duration;

use crate::http_breakpoint::BreakpointDownloadHttpConfig;

#[derive(Debug, Clone)]
pub struct MeowConfig {
    max_upload_concurrency: usize,
    max_download_concurrency: usize,
    breakpoint_download_http: BreakpointDownloadHttpConfig,
    http_client: Option<reqwest::Client>,
    http_timeout: Duration,
    tcp_keepalive: Duration,
}

impl Default for MeowConfig {
    fn default() -> Self {
        Self {
            max_upload_concurrency: 2,
            max_download_concurrency: 2,
            breakpoint_download_http: BreakpointDownloadHttpConfig::default(),
            http_client: None,
            http_timeout: Duration::from_secs(5),
            tcp_keepalive: Duration::from_secs(30),
        }
    }
}

impl MeowConfig {
    pub fn new(max_upload_concurrency: usize, max_download_concurrency: usize) -> Self {
        Self {
            max_upload_concurrency,
            max_download_concurrency,
            breakpoint_download_http: BreakpointDownloadHttpConfig::default(),
            http_client: None,
            http_timeout: Duration::from_secs(5),
            tcp_keepalive: Duration::from_secs(30),
        }
    }

    pub fn with_http_client(mut self, client: reqwest::Client) -> Self {
        self.http_client = Some(client);
        self
    }

    pub fn with_http_timeout(mut self, timeout: Duration) -> Self {
        self.http_timeout = timeout;
        self
    }

    pub fn with_tcp_keepalive(mut self, keepalive: Duration) -> Self {
        self.tcp_keepalive = keepalive;
        self
    }

    pub fn max_upload_concurrency(&self) -> usize {
        self.max_upload_concurrency
    }

    pub fn max_download_concurrency(&self) -> usize {
        self.max_download_concurrency
    }

    pub fn breakpoint_download_http(&self) -> &BreakpointDownloadHttpConfig {
        &self.breakpoint_download_http
    }

    pub fn with_breakpoint_download_http(mut self, config: BreakpointDownloadHttpConfig) -> Self {
        self.breakpoint_download_http = config;
        self
    }

    pub fn http_timeout(&self) -> Duration {
        self.http_timeout
    }

    pub fn tcp_keepalive(&self) -> Duration {
        self.tcp_keepalive
    }

    pub(crate) fn http_client_ref(&self) -> Option<&reqwest::Client> {
        self.http_client.as_ref()
    }
}
