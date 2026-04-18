use std::time::Duration;

use crate::http_breakpoint::BreakpointDownloadHttpConfig;

/// Runtime and transport configuration for [`crate::MeowClient`].
///
/// This type is immutable after client creation. Build it with `new()` or
/// `Default::default()`, then chain `with_*` methods.
///
/// # Recommended workflow
///
/// 1. Start with `MeowConfig::default()` for safe baseline values.
/// 2. Tune concurrency and queue capacities according to workload.
/// 3. Set HTTP timeout and keepalive for your network environment.
/// 4. Optionally inject a preconfigured `reqwest::Client`.
///
/// # Value constraints
///
/// - Concurrency values should be `>= 1`.
/// - Queue capacities should be `>= 1`.
/// - `http_timeout` and `tcp_keepalive` should be positive durations.
/// - Zero or extremely small durations may cause request failures.
#[derive(Debug, Clone)]
pub struct MeowConfig {
    /// Maximum number of upload groups processed concurrently.
    ///
    /// Recommended range: `1..=64`.
    max_upload_concurrency: usize,
    /// Maximum number of download groups processed concurrently.
    ///
    /// Recommended range: `1..=64`.
    max_download_concurrency: usize,
    /// HTTP-level settings used by range download requests.
    breakpoint_download_http: BreakpointDownloadHttpConfig,
    /// Optional custom HTTP client used by transfer requests.
    ///
    /// If `None`, the library builds an internal `reqwest::Client`.
    http_client: Option<reqwest::Client>,
    /// Per-request timeout used when building internal HTTP clients.
    ///
    /// Recommended range: `1s..=120s`.
    http_timeout: Duration,
    /// TCP keepalive duration for internal HTTP clients.
    ///
    /// Recommended range: `10s..=5min`.
    tcp_keepalive: Duration,
    /// Capacity of the control-plane command queue.
    ///
    /// Recommended range: `16..=4096`.
    command_queue_capacity: usize,
    /// Capacity of the worker event queue (progress/state events).
    ///
    /// Recommended range: `32..=8192`.
    worker_event_queue_capacity: usize,
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
            command_queue_capacity: 128,
            worker_event_queue_capacity: 256,
        }
    }
}

impl MeowConfig {
    /// Creates a new config with explicit upload/download concurrency.
    ///
    /// Other fields use the same defaults as [`Default::default`].
    ///
    /// # Parameters
    ///
    /// - `max_upload_concurrency`: Recommended `>= 1`.
    /// - `max_download_concurrency`: Recommended `>= 1`.
    ///
    /// # Usage rules
    ///
    /// Prefer values close to CPU and network capacity. Very high values can
    /// increase memory pressure and request contention.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::MeowConfig;
    ///
    /// let config = MeowConfig::new(4, 4);
    /// assert_eq!(config.max_upload_concurrency(), 4);
    /// assert_eq!(config.max_download_concurrency(), 4);
    /// ```
    pub fn new(max_upload_concurrency: usize, max_download_concurrency: usize) -> Self {
        Self {
            max_upload_concurrency,
            max_download_concurrency,
            breakpoint_download_http: BreakpointDownloadHttpConfig::default(),
            http_client: None,
            http_timeout: Duration::from_secs(5),
            tcp_keepalive: Duration::from_secs(30),
            command_queue_capacity: 128,
            worker_event_queue_capacity: 256,
        }
    }

    /// Injects a custom HTTP client for all transfer requests.
    ///
    /// Use this when you need custom proxy, TLS, default headers, middleware,
    /// or observability behavior.
    ///
    /// # Usage rules
    ///
    /// - The provided client should be reusable and long-lived.
    /// - Keep timeout/connection settings aligned with your workload.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::MeowConfig;
    ///
    /// let client = reqwest::Client::new();
    /// let config = MeowConfig::default().with_http_client(client);
    /// let _ = config;
    /// ```
    pub fn with_http_client(mut self, client: reqwest::Client) -> Self {
        self.http_client = Some(client);
        self
    }

    /// Sets request timeout used by internally created HTTP clients.
    ///
    /// # Range guidance
    ///
    /// - Typical: `3s..=60s`.
    /// - High-latency network: `30s..=120s`.
    /// - Avoid zero duration.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::time::Duration;
    /// use rusty_cat::api::MeowConfig;
    ///
    /// let config = MeowConfig::default().with_http_timeout(Duration::from_secs(20));
    /// assert_eq!(config.http_timeout(), Duration::from_secs(20));
    /// ```
    pub fn with_http_timeout(mut self, timeout: Duration) -> Self {
        self.http_timeout = timeout;
        self
    }

    /// Sets TCP keepalive for internally created HTTP clients.
    ///
    /// # Range guidance
    ///
    /// - Typical: `15s..=120s`.
    /// - Keepalive too small may increase churn.
    /// - Keepalive too large may delay broken-connection detection.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::time::Duration;
    /// use rusty_cat::api::MeowConfig;
    ///
    /// let config = MeowConfig::default().with_tcp_keepalive(Duration::from_secs(60));
    /// assert_eq!(config.tcp_keepalive(), Duration::from_secs(60));
    /// ```
    pub fn with_tcp_keepalive(mut self, keepalive: Duration) -> Self {
        self.tcp_keepalive = keepalive;
        self
    }

    /// Sets command queue capacity for control-plane operations.
    ///
    /// This queue carries operations such as enqueue, pause, resume, cancel,
    /// snapshot, and close.
    ///
    /// # Range guidance
    ///
    /// Recommended range: `16..=4096`; must be `>= 1`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::MeowConfig;
    ///
    /// let config = MeowConfig::default().with_command_queue_capacity(512);
    /// assert_eq!(config.command_queue_capacity(), 512);
    /// ```
    pub fn with_command_queue_capacity(mut self, command_queue_capacity: usize) -> Self {
        self.command_queue_capacity = command_queue_capacity;
        self
    }

    /// Sets worker event queue capacity for runtime task events.
    ///
    /// Events include progress updates and task terminal states.
    ///
    /// # Range guidance
    ///
    /// Recommended range: `32..=8192`; must be `>= 1`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::MeowConfig;
    ///
    /// let config = MeowConfig::default().with_worker_event_queue_capacity(1024);
    /// assert_eq!(config.worker_event_queue_capacity(), 1024);
    /// ```
    pub fn with_worker_event_queue_capacity(mut self, worker_event_queue_capacity: usize) -> Self {
        self.worker_event_queue_capacity = worker_event_queue_capacity;
        self
    }

    /// Returns maximum upload concurrency.
    ///
    /// Expected effective range: `>= 1`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::MeowConfig;
    ///
    /// let config = MeowConfig::new(3, 2);
    /// assert_eq!(config.max_upload_concurrency(), 3);
    /// ```
    pub fn max_upload_concurrency(&self) -> usize {
        self.max_upload_concurrency
    }

    /// Returns maximum download concurrency.
    ///
    /// Expected effective range: `>= 1`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::MeowConfig;
    ///
    /// let config = MeowConfig::new(3, 2);
    /// assert_eq!(config.max_download_concurrency(), 2);
    /// ```
    pub fn max_download_concurrency(&self) -> usize {
        self.max_download_concurrency
    }

    /// Returns range-download HTTP behavior configuration.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::MeowConfig;
    ///
    /// let config = MeowConfig::default();
    /// let _ = config.breakpoint_download_http();
    /// ```
    pub fn breakpoint_download_http(&self) -> &BreakpointDownloadHttpConfig {
        &self.breakpoint_download_http
    }

    /// Overrides range-download HTTP behavior configuration.
    ///
    /// Use this to customize request headers (for example `Accept`) for range
    /// download chunks.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::{BreakpointDownloadHttpConfig, MeowConfig};
    ///
    /// let config = MeowConfig::default().with_breakpoint_download_http(
    ///     BreakpointDownloadHttpConfig {
    ///         range_accept: "application/octet-stream".to_string(),
    ///     },
    /// );
    /// let _ = config;
    /// ```
    pub fn with_breakpoint_download_http(mut self, config: BreakpointDownloadHttpConfig) -> Self {
        self.breakpoint_download_http = config;
        self
    }

    /// Returns request timeout used by internal HTTP clients.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::time::Duration;
    /// use rusty_cat::api::MeowConfig;
    ///
    /// let config = MeowConfig::default();
    /// assert_eq!(config.http_timeout(), Duration::from_secs(5));
    /// ```
    pub fn http_timeout(&self) -> Duration {
        self.http_timeout
    }

    /// Returns TCP keepalive used by internal HTTP clients.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::time::Duration;
    /// use rusty_cat::api::MeowConfig;
    ///
    /// let config = MeowConfig::default();
    /// assert_eq!(config.tcp_keepalive(), Duration::from_secs(30));
    /// ```
    pub fn tcp_keepalive(&self) -> Duration {
        self.tcp_keepalive
    }

    /// Returns the injected custom HTTP client, if present.
    ///
    /// This is an internal accessor used by runtime components.
    pub(crate) fn http_client_ref(&self) -> Option<&reqwest::Client> {
        self.http_client.as_ref()
    }

    /// Returns control-plane command queue capacity.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::MeowConfig;
    ///
    /// let config = MeowConfig::default();
    /// assert_eq!(config.command_queue_capacity(), 128);
    /// ```
    pub fn command_queue_capacity(&self) -> usize {
        self.command_queue_capacity
    }

    /// Returns worker event queue capacity.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::MeowConfig;
    ///
    /// let config = MeowConfig::default();
    /// assert_eq!(config.worker_event_queue_capacity(), 256);
    /// ```
    pub fn worker_event_queue_capacity(&self) -> usize {
        self.worker_event_queue_capacity
    }
}
