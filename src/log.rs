//! Flow-level debug logging utilities.
//!
//! At most one global listener can be registered. When no listener is present,
//! `emit`/`emit_lazy` return quickly with minimal overhead. Listener panics are
//! isolated with `catch_unwind` so SDK internal logic remains safe.

use std::fmt;
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Log severity level for flow debug entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
        };
        f.write_str(s)
    }
}

/// One structured log record that can be printed or persisted externally.
#[derive(Debug, Clone)]
pub struct Log {
    /// Unix epoch timestamp in milliseconds.
    timestamp_ms: u64,
    /// Log severity level.
    level: LogLevel,
    /// Static tag such as `"meow_client"` or `"enqueue"` for filtering.
    tag: &'static str,
    /// Human-readable message content.
    message: String,
}

impl Log {
    /// Creates a log entry with explicit level and tag.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::{Log, LogLevel};
    ///
    /// let log = Log::new(LogLevel::Info, "demo", "hello");
    /// assert_eq!(log.level(), LogLevel::Info);
    /// ```
    pub fn new(level: LogLevel, tag: &'static str, message: impl Into<String>) -> Self {
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self {
            timestamp_ms,
            level,
            tag,
            message: message.into(),
        }
    }

    /// Creates a debug-level log entry.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::Log;
    ///
    /// let log = Log::debug("demo", "debug message");
    /// assert_eq!(log.tag(), "demo");
    /// ```
    pub fn debug(tag: &'static str, message: impl Into<String>) -> Self {
        Self::new(LogLevel::Debug, tag, message)
    }

    /// Returns timestamp in milliseconds.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::Log;
    ///
    /// let log = Log::debug("demo", "message");
    /// let _ts = log.timestamp_ms();
    /// ```
    pub fn timestamp_ms(&self) -> u64 {
        self.timestamp_ms
    }

    /// Returns log level.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::{Log, LogLevel};
    ///
    /// let log = Log::new(LogLevel::Warn, "demo", "warn");
    /// assert_eq!(log.level(), LogLevel::Warn);
    /// ```
    pub fn level(&self) -> LogLevel {
        self.level
    }

    /// Returns static tag.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::Log;
    ///
    /// let log = Log::debug("network", "retry");
    /// assert_eq!(log.tag(), "network");
    /// ```
    pub fn tag(&self) -> &'static str {
        self.tag
    }

    /// Returns message by shared reference.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::Log;
    ///
    /// let log = Log::debug("demo", "hello");
    /// assert_eq!(log.message(), "hello");
    /// ```
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Consumes the entry and returns owned message.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::Log;
    ///
    /// let log = Log::debug("demo", "hello");
    /// let msg = log.into_message();
    /// assert_eq!(msg, "hello");
    /// ```
    pub fn into_message(self) -> String {
        self.message
    }
}

impl fmt::Display for Log {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[{}] {} [{}] {}",
            self.timestamp_ms, self.level, self.tag, self.message
        )
    }
}

/// Callback type for global debug log listener.
pub type DebugLogListener = Arc<dyn Fn(Log) + Send + Sync + 'static>;

static DEBUG_LOG_LISTENER: OnceLock<Mutex<Option<DebugLogListener>>> = OnceLock::new();

fn debug_log_listener_slot() -> &'static Mutex<Option<DebugLogListener>> {
    DEBUG_LOG_LISTENER.get_or_init(|| Mutex::new(None))
}

/// Returns whether a debug log listener is currently registered.
///
/// Useful on hot paths to avoid constructing log objects unnecessarily.
///
/// # Examples
///
/// ```no_run
/// use rusty_cat::api::debug_log_listener_active;
///
/// let _active = debug_log_listener_active();
/// ```
#[inline]
pub fn debug_log_listener_active() -> bool {
    match debug_log_listener_slot().lock() {
        Ok(g) => g.is_some(),
        Err(_) => false,
    }
}

/// Sets or clears the global debug log listener.
///
/// - `Some(listener)`: set or replace current listener.
/// - `None`: clear listener (unregister).
///
/// # Errors
///
/// Returns [`DebugLogListenerError`] when internal listener storage lock is
/// poisoned.
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
/// use rusty_cat::api::{set_debug_log_listener, DebugLogListener, Log};
///
/// let listener: DebugLogListener = Arc::new(|log: Log| println!("{log}"));
/// set_debug_log_listener(Some(listener))?;
/// set_debug_log_listener(None)?;
/// # Ok::<(), rusty_cat::api::DebugLogListenerError>(())
/// ```
pub fn set_debug_log_listener(
    listener: Option<DebugLogListener>,
) -> Result<(), DebugLogListenerError> {
    let mut g = debug_log_listener_slot()
        .lock()
        .map_err(|_| DebugLogListenerError(()))?;
    *g = listener;
    Ok(())
}

/// Registers a global singleton debug log listener.
///
/// Returns `Err` if a listener is already registered.
///
/// # Errors
///
/// Returns [`DebugLogListenerError`] when:
/// - a listener is already registered, or
/// - internal listener storage lock is poisoned.
///
/// # Examples
///
/// ```no_run
/// use rusty_cat::api::{try_set_debug_log_listener, Log};
///
/// let _ = try_set_debug_log_listener(|log: Log| {
///     println!("{log}");
/// });
/// ```
pub fn try_set_debug_log_listener<F>(f: F) -> Result<(), DebugLogListenerError>
where
    F: Fn(Log) + Send + Sync + 'static,
{
    let mut g = debug_log_listener_slot()
        .lock()
        .map_err(|_| DebugLogListenerError(()))?;
    if g.is_some() {
        return Err(DebugLogListenerError(()));
    }
    *g = Some(Arc::new(f));
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DebugLogListenerError(());

impl fmt::Display for DebugLogListenerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("debug log listener already set")
    }
}

impl std::error::Error for DebugLogListenerError {}

/// Emits one log entry.
///
/// Returns immediately when no listener is set. Listener panics are caught and
/// discarded.
///
/// # Panics
///
/// This function does not panic. Listener panics are caught internally.
///
/// # Examples
///
/// ```no_run
/// use rusty_cat::api::{emit, Log};
///
/// emit(Log::debug("demo", "manual emit"));
/// ```
pub fn emit(log: Log) {
    let cb_opt = debug_log_listener_slot()
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(Arc::clone));
    let Some(cb) = cb_opt else {
        return;
    };
    let _ = panic::catch_unwind(AssertUnwindSafe(move || {
        cb(log);
    }));
}

/// Lazily emits a log entry only when listener is active.
///
/// This avoids formatting/allocation overhead when logging is disabled.
///
/// # Panics
///
/// This function does not panic. Any panic from listener callback is caught by
/// [`emit`].
///
/// # Examples
///
/// ```no_run
/// use rusty_cat::api::{emit_lazy, Log};
///
/// emit_lazy(|| Log::debug("demo", format!("computed {}", 42)));
/// ```
#[inline]
pub fn emit_lazy<F>(f: F)
where
    F: FnOnce() -> Log,
{
    if !debug_log_listener_active() {
        return;
    }
    emit(f());
}

/// Internal flow debug logging macro.
///
/// The `format!` expression is evaluated only when listener is active.
/// crate::meow_flow_log!(
///     "enqueue",
///    "task_id={:?} offset={} total={}",
///     task_id,
///     offset,
///    total
/// );
#[macro_export]
macro_rules! meow_flow_log {
    ($tag:expr, $($arg:tt)*) => {
        $crate::log::emit_lazy(|| {
            $crate::log::Log::debug($tag, format!($($arg)*))
        });
    };
}
