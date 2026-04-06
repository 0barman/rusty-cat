//! 流程性调试日志：全局至多注册一个监听器；未注册时 `emit` 路径为低开销快速返回。
//! 监听器调用包在 `catch_unwind` 中，避免用户回调 `panic!` 影响 SDK 内部逻辑。

use std::fmt;
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// 与流程日志条目一同输出的级别（便于外部过滤或映射到 tracing 等）。
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

/// 一条可交给外部打印或持久化的日志记录。
#[derive(Debug, Clone)]
pub struct Log {
    /// 毫秒时间戳（Unix epoch），便于外部序列化。
    timestamp_ms: u64,
    level: LogLevel,
    /// 固定标签，如 `"meow_client"`、`"enqueue"`，便于过滤。
    tag: &'static str,
    message: String,
}

impl Log {
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

    pub fn debug(tag: &'static str, message: impl Into<String>) -> Self {
        Self::new(LogLevel::Debug, tag, message)
    }

    pub fn timestamp_ms(&self) -> u64 {
        self.timestamp_ms
    }

    pub fn level(&self) -> LogLevel {
        self.level
    }

    pub fn tag(&self) -> &'static str {
        self.tag
    }

    pub fn message(&self) -> &str {
        &self.message
    }

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

pub type DebugLogListener = Arc<dyn Fn(Log) + Send + Sync + 'static>;

static DEBUG_LOG_LISTENER: OnceLock<Mutex<Option<DebugLogListener>>> = OnceLock::new();

fn debug_log_listener_slot() -> &'static Mutex<Option<DebugLogListener>> {
    DEBUG_LOG_LISTENER.get_or_init(|| Mutex::new(None))
}

/// 是否已注册调试日志监听器（热路径上可先读此再决定是否构造 `Log`）。
#[inline]
pub fn debug_log_listener_active() -> bool {
    match debug_log_listener_slot().lock() {
        Ok(g) => g.is_some(),
        Err(_) => false,
    }
}

/// 设置（或清空）全局调试日志监听器。
///
/// - `Some(listener)`: 设置/替换当前监听器
/// - `None`: 清空监听器（等价于取消注册）
pub fn set_debug_log_listener(
    listener: Option<DebugLogListener>,
) -> Result<(), DebugLogListenerError> {
    let mut g = debug_log_listener_slot()
        .lock()
        .map_err(|_| DebugLogListenerError(()))?;
    *g = listener;
    Ok(())
}

/// 注册全局唯一的调试日志监听器；重复注册返回 `Err`。
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

/// 发出一条日志；无监听器时立即返回。监听器 `panic` 会被捕获并丢弃。
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

/// 仅当已注册监听器时才执行闭包构造 `Log`，避免无监听器时的字符串格式化开销。
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

/// 内部流程日志宏：无监听器时不展开 `format!`。
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
