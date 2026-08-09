use std::time::Duration;

use crate::error::{InnerErrorCode, MeowError};

/// Default maximum response body retained by an in-memory binary task.
pub const DEFAULT_BINARY_MAX_BODY_BYTES: u64 = 5 * 1024 * 1024;
/// Hard safety ceiling for one in-memory response body.
pub const BINARY_ABSOLUTE_MAX_BODY_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const BINARY_MAX_RETRY_DELAYS: usize = 8;
pub(crate) const BINARY_MAX_REDIRECTS: usize = 10;

/// HTTP and memory limits for [`crate::api::BinaryTask`].
///
/// The configuration belongs to a [`crate::api::MeowConfig`] and is only used
/// when the first binary task initializes its isolated executor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BinaryDownloadConfig {
    max_body_bytes: u64,
    request_timeout: Option<Duration>,
    tcp_keepalive: Option<Duration>,
    redirect_limit: usize,
    retry_delays: Vec<Duration>,
}

/// Builder for validated [`BinaryDownloadConfig`] values.
#[derive(Clone, Debug)]
pub struct BinaryDownloadConfigBuilder {
    config: BinaryDownloadConfig,
}

impl Default for BinaryDownloadConfig {
    fn default() -> Self {
        Self {
            max_body_bytes: DEFAULT_BINARY_MAX_BODY_BYTES,
            request_timeout: None,
            tcp_keepalive: None,
            redirect_limit: 5,
            retry_delays: vec![Duration::from_millis(300), Duration::from_millis(800)],
        }
    }
}

impl BinaryDownloadConfig {
    /// Starts a builder from the safe defaults.
    pub fn builder() -> BinaryDownloadConfigBuilder {
        BinaryDownloadConfigBuilder {
            config: Self::default(),
        }
    }

    /// Maximum response body size in bytes.
    pub fn max_body_bytes(&self) -> u64 {
        self.max_body_bytes
    }

    /// Optional per-attempt timeout. `None` inherits `MeowConfig::http_timeout`.
    pub fn request_timeout(&self) -> Option<Duration> {
        self.request_timeout
    }

    /// Optional TCP keepalive. `None` inherits `MeowConfig::tcp_keepalive`.
    pub fn tcp_keepalive(&self) -> Option<Duration> {
        self.tcp_keepalive
    }

    /// Maximum number of redirects followed by one request.
    pub fn redirect_limit(&self) -> usize {
        self.redirect_limit
    }

    /// Delay before each retry. An empty slice disables retries.
    pub fn retry_delays(&self) -> &[Duration] {
        &self.retry_delays
    }

    pub(crate) fn validate(&self) -> Result<(), MeowError> {
        if !(1..=BINARY_ABSOLUTE_MAX_BODY_BYTES).contains(&self.max_body_bytes) {
            return Err(parameter_error(format!(
                "binary max_body_bytes must be in 1..={BINARY_ABSOLUTE_MAX_BODY_BYTES}"
            )));
        }
        if self.request_timeout.is_some_and(|value| value.is_zero()) {
            return Err(parameter_error(
                "binary request_timeout must be greater than zero",
            ));
        }
        if self.tcp_keepalive.is_some_and(|value| value.is_zero()) {
            return Err(parameter_error(
                "binary tcp_keepalive must be greater than zero",
            ));
        }
        if self.redirect_limit > BINARY_MAX_REDIRECTS {
            return Err(parameter_error(format!(
                "binary redirect_limit must be <= {BINARY_MAX_REDIRECTS}"
            )));
        }
        if self.retry_delays.len() > BINARY_MAX_RETRY_DELAYS {
            return Err(parameter_error(format!(
                "binary retry_delays must contain at most {BINARY_MAX_RETRY_DELAYS} entries"
            )));
        }
        if self.retry_delays.iter().any(Duration::is_zero) {
            return Err(parameter_error(
                "binary retry delays must be greater than zero",
            ));
        }
        Ok(())
    }
}

impl BinaryDownloadConfigBuilder {
    /// Sets the global response body limit.
    pub fn max_body_bytes(mut self, value: u64) -> Self {
        self.config.max_body_bytes = value;
        self
    }

    /// Overrides the inherited request timeout.
    pub fn request_timeout(mut self, value: Duration) -> Self {
        self.config.request_timeout = Some(value);
        self
    }

    /// Overrides the inherited TCP keepalive.
    pub fn tcp_keepalive(mut self, value: Duration) -> Self {
        self.config.tcp_keepalive = Some(value);
        self
    }

    /// Sets the redirect limit. Zero disables redirect following.
    pub fn redirect_limit(mut self, value: usize) -> Self {
        self.config.redirect_limit = value;
        self
    }

    /// Replaces the retry schedule. An empty vector disables retries.
    pub fn retry_delays(mut self, value: Vec<Duration>) -> Self {
        self.config.retry_delays = value;
        self
    }

    /// Validates and builds the configuration.
    pub fn build(self) -> Result<BinaryDownloadConfig, MeowError> {
        self.config.validate()?;
        Ok(self.config)
    }
}

fn parameter_error(message: impl Into<String>) -> MeowError {
    MeowError::from_code(InnerErrorCode::ParameterEmpty, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_bounded() {
        let config = BinaryDownloadConfig::default();
        assert_eq!(config.max_body_bytes(), 5 * 1024 * 1024);
        assert_eq!(config.redirect_limit(), 5);
        assert_eq!(config.retry_delays().len(), 2);
        config.validate().expect("defaults must be valid");
    }

    #[test]
    fn invalid_memory_and_timing_values_are_rejected() {
        for max in [0, BINARY_ABSOLUTE_MAX_BODY_BYTES + 1] {
            let error = BinaryDownloadConfig::builder()
                .max_body_bytes(max)
                .build()
                .expect_err("invalid maximum must fail");
            assert_eq!(error.code(), InnerErrorCode::ParameterEmpty as i32);
        }
        assert!(BinaryDownloadConfig::builder()
            .request_timeout(Duration::ZERO)
            .build()
            .is_err());
        assert!(BinaryDownloadConfig::builder()
            .tcp_keepalive(Duration::ZERO)
            .build()
            .is_err());
        assert!(BinaryDownloadConfig::builder()
            .retry_delays(vec![Duration::ZERO])
            .build()
            .is_err());
    }
}
