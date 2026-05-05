use std::time::Duration;

use crate::error::{InnerErrorCode, MeowError};
use crate::http_breakpoint::BreakpointDownloadHttpConfig;

/// Runtime and transport configuration for [`crate::MeowClient`].
///
/// This type is immutable after client creation. Use [`Self::default`] for a
/// safe baseline, or [`Self::builder`] for validated customization.
///
/// # Recommended workflow
///
/// 1. Start with `MeowConfig::default()` for safe baseline values.
/// 2. Use [`Self::builder`] when you need custom values.
/// 3. Tune concurrency and queue capacities according to workload.
/// 4. Set HTTP timeout and keepalive for your network environment.
/// 5. Optionally inject a preconfigured `reqwest::Client`.
///
/// # Value constraints
///
/// - Concurrency values must be `>= 1`.
/// - Queue capacities must be `>= 1`.
/// - `http_timeout` and `tcp_keepalive` must be positive durations.
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

/// Builder for validated [`MeowConfig`] construction.
///
/// The builder starts from the same safe defaults as [`MeowConfig::default`].
/// Invalid hard-constraint values are rejected by [`Self::build`] instead of
/// being silently clamped or delayed until runtime initialization.
#[derive(Debug, Clone)]
pub struct MeowConfigBuilder {
    max_upload_concurrency: usize,
    max_download_concurrency: usize,
    breakpoint_download_http: BreakpointDownloadHttpConfig,
    http_client: Option<reqwest::Client>,
    http_timeout: Duration,
    tcp_keepalive: Duration,
    command_queue_capacity: usize,
    worker_event_queue_capacity: usize,
}

impl Default for MeowConfig {
    fn default() -> Self {
        let builder = MeowConfigBuilder::default();
        Self {
            max_upload_concurrency: builder.max_upload_concurrency,
            max_download_concurrency: builder.max_download_concurrency,
            breakpoint_download_http: builder.breakpoint_download_http,
            http_client: builder.http_client,
            http_timeout: builder.http_timeout,
            tcp_keepalive: builder.tcp_keepalive,
            command_queue_capacity: builder.command_queue_capacity,
            worker_event_queue_capacity: builder.worker_event_queue_capacity,
        }
    }
}

impl Default for MeowConfigBuilder {
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
    /// Starts a validated config builder from safe defaults.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::MeowConfig;
    ///
    /// let config = MeowConfig::builder()
    ///     .max_upload_concurrency(4)
    ///     .max_download_concurrency(4)
    ///     .build()?;
    /// assert_eq!(config.max_upload_concurrency(), 4);
    /// # Ok::<(), rusty_cat::api::MeowError>(())
    /// ```
    pub fn builder() -> MeowConfigBuilder {
        MeowConfigBuilder::default()
    }

    /// Returns maximum upload concurrency.
    ///
    /// Guaranteed effective range: `>= 1`.
    pub fn max_upload_concurrency(&self) -> usize {
        self.max_upload_concurrency
    }

    /// Returns maximum download concurrency.
    ///
    /// Guaranteed effective range: `>= 1`.
    pub fn max_download_concurrency(&self) -> usize {
        self.max_download_concurrency
    }

    /// Returns range-download HTTP behavior configuration.
    pub fn breakpoint_download_http(&self) -> &BreakpointDownloadHttpConfig {
        &self.breakpoint_download_http
    }

    /// Returns request timeout used by internal HTTP clients.
    pub fn http_timeout(&self) -> Duration {
        self.http_timeout
    }

    /// Returns TCP keepalive used by internal HTTP clients.
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
    pub fn command_queue_capacity(&self) -> usize {
        self.command_queue_capacity
    }

    /// Returns worker event queue capacity.
    pub fn worker_event_queue_capacity(&self) -> usize {
        self.worker_event_queue_capacity
    }
}

impl MeowConfigBuilder {
    /// Sets maximum upload concurrency.
    ///
    /// Must be `>= 1`; validated by [`Self::build`].
    pub fn max_upload_concurrency(mut self, max_upload_concurrency: usize) -> Self {
        self.max_upload_concurrency = max_upload_concurrency;
        self
    }

    /// Sets maximum download concurrency.
    ///
    /// Must be `>= 1`; validated by [`Self::build`].
    pub fn max_download_concurrency(mut self, max_download_concurrency: usize) -> Self {
        self.max_download_concurrency = max_download_concurrency;
        self
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
    /// let config = MeowConfig::builder().http_client(client).build()?;
    /// let _ = config;
    /// # Ok::<(), rusty_cat::api::MeowError>(())
    /// ```
    pub fn http_client(mut self, client: reqwest::Client) -> Self {
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
    /// let config = MeowConfig::builder()
    ///     .http_timeout(Duration::from_secs(20))
    ///     .build()?;
    /// assert_eq!(config.http_timeout(), Duration::from_secs(20));
    /// # Ok::<(), rusty_cat::api::MeowError>(())
    /// ```
    pub fn http_timeout(mut self, timeout: Duration) -> Self {
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
    /// let config = MeowConfig::builder()
    ///     .tcp_keepalive(Duration::from_secs(60))
    ///     .build()?;
    /// assert_eq!(config.tcp_keepalive(), Duration::from_secs(60));
    /// # Ok::<(), rusty_cat::api::MeowError>(())
    /// ```
    pub fn tcp_keepalive(mut self, keepalive: Duration) -> Self {
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
    /// let config = MeowConfig::builder()
    ///     .command_queue_capacity(512)
    ///     .build()?;
    /// assert_eq!(config.command_queue_capacity(), 512);
    /// # Ok::<(), rusty_cat::api::MeowError>(())
    /// ```
    pub fn command_queue_capacity(mut self, command_queue_capacity: usize) -> Self {
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
    /// let config = MeowConfig::builder()
    ///     .worker_event_queue_capacity(1024)
    ///     .build()?;
    /// assert_eq!(config.worker_event_queue_capacity(), 1024);
    /// # Ok::<(), rusty_cat::api::MeowError>(())
    /// ```
    pub fn worker_event_queue_capacity(mut self, worker_event_queue_capacity: usize) -> Self {
        self.worker_event_queue_capacity = worker_event_queue_capacity;
        self
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
    /// let config = MeowConfig::builder().breakpoint_download_http(
    ///     BreakpointDownloadHttpConfig {
    ///         range_accept: "application/octet-stream".to_string(),
    ///     },
    /// ).build()?;
    /// let _ = config;
    /// # Ok::<(), rusty_cat::api::MeowError>(())
    /// ```
    pub fn breakpoint_download_http(mut self, config: BreakpointDownloadHttpConfig) -> Self {
        self.breakpoint_download_http = config;
        self
    }

    /// Validates and builds a [`MeowConfig`].
    pub fn build(self) -> Result<MeowConfig, MeowError> {
        validate_non_zero("max_upload_concurrency", self.max_upload_concurrency)?;
        validate_non_zero("max_download_concurrency", self.max_download_concurrency)?;
        validate_non_zero("command_queue_capacity", self.command_queue_capacity)?;
        validate_non_zero(
            "worker_event_queue_capacity",
            self.worker_event_queue_capacity,
        )?;
        validate_positive_duration("http_timeout", self.http_timeout)?;
        validate_positive_duration("tcp_keepalive", self.tcp_keepalive)?;
        Ok(MeowConfig {
            max_upload_concurrency: self.max_upload_concurrency,
            max_download_concurrency: self.max_download_concurrency,
            breakpoint_download_http: self.breakpoint_download_http,
            http_client: self.http_client,
            http_timeout: self.http_timeout,
            tcp_keepalive: self.tcp_keepalive,
            command_queue_capacity: self.command_queue_capacity,
            worker_event_queue_capacity: self.worker_event_queue_capacity,
        })
    }
}

fn validate_non_zero(name: &str, value: usize) -> Result<(), MeowError> {
    if value == 0 {
        return Err(MeowError::from_code(
            InnerErrorCode::ParameterEmpty,
            format!("{name} must be >= 1"),
        ));
    }
    Ok(())
}

fn validate_positive_duration(name: &str, value: Duration) -> Result<(), MeowError> {
    if value.is_zero() {
        return Err(MeowError::from_code(
            InnerErrorCode::ParameterEmpty,
            format!("{name} must be greater than 0"),
        ));
    }
    Ok(())
}
