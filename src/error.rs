use std::error::Error as StdError;
use std::fmt::{Display, Formatter};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InnerErrorCode {
    Unknown = -1,
    /// 成功
    Success = 0,
    ///
    RuntimeCreationFailedError = 101,

    ParameterEmpty = 102,

    /// the same file is already queued or running
    DuplicateTaskError = 103,
    EnqueueError = 104,

    IoError = 105,
    HttpError = 106,
    /// 客户端已经执行过 close，不可再提交或控制任务。
    ClientClosed = 107,
    /// 控制接口收到未知 task_id（例如任务已结束或 id 不存在）。
    TaskNotFound = 108,
    ResponseStatusError = 109,
    MissingOrInvalidContentLengthFromHead = 110,
    /// 控制命令发送到调度线程失败（队列关闭/线程退出等）。
    CommandSendFailed = 111,
    /// 控制命令已发送，但应答通道异常关闭。
    CommandResponseFailed = 112,
    /// JSON 等响应体解析失败。
    ResponseParseError = 113,
    /// HTTP Range 协议非法（状态、Content-Range、偏移等不一致）。
    InvalidRange = 114,
    /// 本地文件不存在（常见于上传源文件丢失）。
    FileNotFound = 115,
    /// 文件校验失败（例如签名/摘要不匹配）。
    ChecksumMismatch = 116,
    /// 任务当前状态不允许该操作（例如 resume 非 paused 任务）。
    InvalidTaskState = 117,
    /// 内部锁被 poison，无法安全读取/写入共享状态。
    LockPoisoned = 118,
    /// 构建内置 HTTP 客户端失败。
    HttpClientBuildFailed = 119,
}

#[derive(Debug, Clone)]
pub struct MeowError {
    /// [InnerErrorCode]
    code: i32,
    msg: String,
    source: Option<Arc<dyn StdError + Send + Sync>>,
}

impl MeowError {
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

    pub fn code(&self) -> i32 {
        self.code
    }

    pub fn msg(&self) -> String {
        self.msg.clone()
    }

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
