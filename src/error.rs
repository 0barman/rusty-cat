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
    /// Task was canceled before reaching `Complete`.
    TaskCanceled = 120,
    /// Local disk ran out of space (`ENOSPC` / `ERROR_DISK_FULL`).
    DiskFull = 121,
    /// Local source/target file was removed or replaced while a transfer was
    /// in progress (for example the user deleted it mid-download).
    LocalFileRemoved = 122,
    /// Binary task capacity (queued, active, or callback pending) is exhausted.
    BinaryTaskQueueFull = 123,
    /// A binary response exceeded its configured in-memory body limit.
    BinaryBodyTooLarge = 124,
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
    /// Optional HTTP status code, set when the error was produced from a
    /// non-success HTTP response. Lets the retry layer distinguish a
    /// non-retryable client error (4xx) from a transient server error (5xx).
    http_status: Option<u16>,
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
            crate::log::Log::debug(
                "error",
                format!(
                    "MeowError::new code={} msg={}",
                    code,
                    crate::log::redact_secrets(&msg)
                ),
            )
        });
        MeowError {
            code,
            msg,
            source: None,
            http_status: None,
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

    /// Returns the error message as a borrowed `&str`.
    ///
    /// Borrowing avoids an allocation on every call; callers that need an
    /// owned `String` can do `err.msg().to_owned()` explicitly.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::{InnerErrorCode, MeowError};
    ///
    /// let err = MeowError::from_code_str(InnerErrorCode::InvalidRange, "bad range");
    /// assert_eq!(err.msg(), "bad range");
    /// ```
    pub fn msg(&self) -> &str {
        &self.msg
    }

    /// Returns the HTTP status code when this error came from a non-success HTTP
    /// response, or `None` otherwise.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::{InnerErrorCode, MeowError};
    ///
    /// let err = MeowError::from_code_str(InnerErrorCode::ParameterEmpty, "bad");
    /// assert_eq!(err.http_status(), None);
    /// ```
    pub fn http_status(&self) -> Option<u16> {
        self.http_status
    }

    /// Attaches the originating HTTP status code, returning the updated error.
    ///
    /// Used by transport code that turns a non-success HTTP response into a
    /// [`MeowError`], so the retry layer can fast-fail non-retryable client
    /// errors (4xx) while still retrying transient server errors (5xx).
    pub(crate) fn with_http_status(mut self, status: u16) -> Self {
        self.http_status = Some(status);
        self
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
            http_status: None,
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
                format!(
                    "MeowError::from_code code={:?} msg={}",
                    code,
                    crate::log::redact_secrets(&msg)
                ),
            )
        });
        MeowError {
            code: code as i32,
            msg,
            source: None,
            http_status: None,
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
                format!(
                    "MeowError::from_code_str code={:?} msg={}",
                    code,
                    crate::log::redact_secrets(msg)
                ),
            )
        });
        MeowError {
            code: code as i32,
            msg: msg.to_string(),
            source: None,
            http_status: None,
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
                    code,
                    crate::log::redact_secrets(&msg),
                    crate::log::redact_secrets(&source_preview)
                ),
            )
        });
        MeowError {
            code: code as i32,
            msg,
            source: Some(Arc::new(source)),
            http_status: None,
        }
    }

    /// Creates an error from a local I/O error, automatically classifying
    /// common failure modes into more specific codes:
    ///
    /// - out-of-space (`ENOSPC` on Unix / `ERROR_DISK_FULL` on Windows) maps to
    ///   [`InnerErrorCode::DiskFull`];
    /// - a missing target (`std::io::ErrorKind::NotFound`) maps to
    ///   [`InnerErrorCode::LocalFileRemoved`], which is what surfaces when a
    ///   source/target file is deleted while a transfer is running;
    /// - anything else falls back to [`InnerErrorCode::IoError`].
    ///
    /// The original [`std::io::Error`] is preserved in the error source chain so
    /// callers can still inspect `raw_os_error()` / `kind()` if needed.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::{InnerErrorCode, MeowError};
    ///
    /// let not_found = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
    /// let err = MeowError::from_io("download file missing", not_found);
    /// assert_eq!(err.code(), InnerErrorCode::LocalFileRemoved as i32);
    /// ```
    pub fn from_io(msg: impl Into<String>, source: std::io::Error) -> Self {
        let code = classify_io_error(&source);
        Self::from_source(code, msg, source)
    }
}

/// Classifies a local I/O error into the most specific SDK error code.
///
/// Used by [`MeowError::from_io`]; kept as a standalone function so the mapping
/// can be unit-tested in isolation.
pub(crate) fn classify_io_error(e: &std::io::Error) -> InnerErrorCode {
    if is_disk_full(e) {
        InnerErrorCode::DiskFull
    } else if e.kind() == std::io::ErrorKind::NotFound {
        InnerErrorCode::LocalFileRemoved
    } else {
        InnerErrorCode::IoError
    }
}

/// Detects "no space left on device" across platforms via raw OS error codes.
///
/// `std::io::ErrorKind` does not expose a stable out-of-space variant across the
/// toolchains this crate targets, so the raw OS error number is checked instead:
/// `ENOSPC` (28) on Unix-like systems, and `ERROR_DISK_FULL` (112) /
/// `ERROR_HANDLE_DISK_FULL` (39) on Windows.
fn is_disk_full(e: &std::io::Error) -> bool {
    if let Some(code) = e.raw_os_error() {
        #[cfg(unix)]
        if code == 28 {
            return true;
        }
        #[cfg(windows)]
        if code == 112 || code == 39 {
            return true;
        }
        let _ = code;
    }
    false
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

    #[test]
    fn from_io_classifies_not_found_as_local_file_removed() {
        let not_found = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        let err = MeowError::from_io("target file missing", not_found);
        assert_eq!(err.code(), InnerErrorCode::LocalFileRemoved as i32);
        // Original io error is preserved for callers that want to inspect it.
        assert!(std::error::Error::source(&err).is_some());
    }

    #[test]
    fn from_io_classifies_generic_error_as_io_error() {
        let denied = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let err = MeowError::from_io("write failed", denied);
        assert_eq!(err.code(), InnerErrorCode::IoError as i32);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn from_io_classifies_out_of_space_as_disk_full() {
        // ENOSPC on Unix, ERROR_DISK_FULL on Windows.
        #[cfg(unix)]
        let full = std::io::Error::from_raw_os_error(28);
        #[cfg(windows)]
        let full = std::io::Error::from_raw_os_error(112);

        let err = MeowError::from_io("write download file failed", full);
        assert_eq!(err.code(), InnerErrorCode::DiskFull as i32);
    }

    #[cfg(windows)]
    #[test]
    fn from_io_classifies_handle_disk_full_as_disk_full() {
        // ERROR_HANDLE_DISK_FULL (39) is the other Windows out-of-space code.
        let full = std::io::Error::from_raw_os_error(39);
        let err = MeowError::from_io("write download file failed", full);
        assert_eq!(err.code(), InnerErrorCode::DiskFull as i32);
    }
}
