use std::error::Error as StdError;
use std::fmt::{Display, Formatter};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InnerErrorCode {
    /// Unknown/unclassified error.
    Unknown = -1,
    /// Success (non-error sentinel).
    Success = 0,
    /// Runtime creation failed.
    RuntimeCreationFailedError = 101,
    /// Required parameter is empty or invalid.
    ParameterEmpty = 102,
    /// The same file/task is already queued or running.
    DuplicateTaskError = 103,
    /// Failed to enqueue task.
    EnqueueError = 104,
    /// Local I/O operation failed.
    IoError = 105,
    /// HTTP request/response operation failed.
    HttpError = 106,
    /// Client has already been closed and can no longer accept operations.
    ClientClosed = 107,
    /// Unknown task ID in control API.
    TaskNotFound = 108,
    /// HTTP response status is not expected.
    ResponseStatusError = 109,
    /// `Content-Length` from HEAD is missing or invalid.
    MissingOrInvalidContentLengthFromHead = 110,
    /// Failed to send command to scheduler thread.
    CommandSendFailed = 111,
    /// Command response channel closed unexpectedly.
    CommandResponseFailed = 112,
    /// Failed to parse response payload (for example JSON).
    ResponseParseError = 113,
    /// Invalid HTTP range semantics or headers.
    InvalidRange = 114,
    /// Local file does not exist.
    FileNotFound = 115,
    /// File checksum/signature does not match expected value.
    ChecksumMismatch = 116,
    /// Current task state does not allow requested operation.
    InvalidTaskState = 117,
    /// Internal lock is poisoned.
    LockPoisoned = 118,
    /// Failed to build internal HTTP client.
    HttpClientBuildFailed = 119,
}

/// Library error type returned by most public APIs.
#[derive(Debug, Clone)]
pub struct MeowError {
    /// Numeric error code, usually mapped from [`InnerErrorCode`].
    code: i32,
    /// Human-readable error message.
    msg: String,
    /// Optional chained source error.
    source: Option<Arc<dyn StdError + Send + Sync>>,
}

impl MeowError {
    /// Creates a new error with raw numeric code and message.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::MeowError;
    ///
    /// let err = MeowError::new(9999, "custom failure".to_string());
    /// assert_eq!(err.code(), 9999);
    /// ```
    pub fn new(code: i32, msg: String) -> Self {
        crate::log::emit_lazy(|| {
            crate::log::Log::debug("error", format!("MeowError::new code={} msg={}", code, msg))
        });
        MeowError {
            code,
            msg,
            source: None,
        }
    }

    /// Returns numeric error code.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::{InnerErrorCode, MeowError};
    ///
    /// let err = MeowError::from_code1(InnerErrorCode::ClientClosed);
    /// assert_eq!(err.code(), InnerErrorCode::ClientClosed as i32);
    /// ```
    pub fn code(&self) -> i32 {
        self.code
    }

    /// Returns error message as an owned `String`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::{InnerErrorCode, MeowError};
    ///
    /// let err = MeowError::from_code_str(InnerErrorCode::InvalidRange, "bad range");
    /// assert_eq!(err.msg(), "bad range".to_string());
    /// ```
    pub fn msg(&self) -> String {
        self.msg.clone()
    }

    /// Creates an error from [`InnerErrorCode`] with empty message.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::{InnerErrorCode, MeowError};
    ///
    /// let err = MeowError::from_code1(InnerErrorCode::ParameterEmpty);
    /// assert_eq!(err.code(), InnerErrorCode::ParameterEmpty as i32);
    /// ```
    pub fn from_code1(code: InnerErrorCode) -> Self {
        crate::log::emit_lazy(|| {
            crate::log::Log::debug("error", format!("MeowError::from_code1 code={:?}", code))
        });
        MeowError {
            code: code as i32,
            msg: String::new(),
            source: None,
        }
    }

    /// Creates an error from [`InnerErrorCode`] and message.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::{InnerErrorCode, MeowError};
    ///
    /// let err = MeowError::from_code(InnerErrorCode::EnqueueError, "enqueue failed".to_string());
    /// assert_eq!(err.code(), InnerErrorCode::EnqueueError as i32);
    /// ```
    pub fn from_code(code: InnerErrorCode, msg: String) -> Self {
        crate::log::emit_lazy(|| {
            crate::log::Log::debug(
                "error",
                format!("MeowError::from_code code={:?} msg={}", code, msg),
            )
        });
        MeowError {
            code: code as i32,
            msg,
            source: None,
        }
    }

    /// Creates an error from [`InnerErrorCode`] and `&str` message.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::{InnerErrorCode, MeowError};
    ///
    /// let err = MeowError::from_code_str(InnerErrorCode::TaskNotFound, "unknown id");
    /// assert_eq!(err.code(), InnerErrorCode::TaskNotFound as i32);
    /// ```
    pub fn from_code_str(code: InnerErrorCode, msg: &str) -> Self {
        crate::log::emit_lazy(|| {
            crate::log::Log::debug(
                "error",
                format!("MeowError::from_code_str code={:?} msg={}", code, msg),
            )
        });
        MeowError {
            code: code as i32,
            msg: msg.to_string(),
            source: None,
        }
    }

    /// Creates an error with source chaining.
    ///
    /// Use this helper to preserve original low-level errors.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::{InnerErrorCode, MeowError};
    ///
    /// let source = std::io::Error::other("disk error");
    /// let err = MeowError::from_source(InnerErrorCode::IoError, "upload failed", source);
    /// assert_eq!(err.code(), InnerErrorCode::IoError as i32);
    /// ```
    pub fn from_source<E>(code: InnerErrorCode, msg: impl Into<String>, source: E) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        let msg = msg.into();
        let source_preview = source.to_string();
        crate::log::emit_lazy(|| {
            crate::log::Log::debug(
                "error",
                format!(
                    "MeowError::from_source code={:?} msg={} source={}",
                    code, msg, source_preview
                ),
            )
        });
        MeowError {
            code: code as i32,
            msg,
            source: Some(Arc::new(source)),
        }
    }
}

impl PartialEq for MeowError {
    fn eq(&self, other: &Self) -> bool {
        self.code == other.code && self.msg == other.msg
    }
}

impl Eq for MeowError {}

impl Display for MeowError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if self.msg.is_empty() {
            write!(f, "MeowError(code={})", self.code)
        } else {
            write!(f, "MeowError(code={}, msg={})", self.code, self.msg)
        }
    }
}

impl StdError for MeowError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.source
            .as_deref()
            .map(|e| e as &(dyn StdError + 'static))
    }
}

#[cfg(test)]
mod tests {
    use super::{InnerErrorCode, MeowError};

    #[test]
    fn meow_error_display_contains_code_and_message() {
        let err = MeowError::from_code_str(InnerErrorCode::InvalidRange, "bad range");
        let s = format!("{err}");
        assert!(s.contains("code="));
        assert!(s.contains("bad range"));
    }

    #[test]
    fn meow_error_source_is_accessible() {
        let io = std::io::Error::other("disk io");
        let err = MeowError::from_source(InnerErrorCode::IoError, "io failed", io);
        assert!(std::error::Error::source(&err).is_some());
    }
}
