use crate::direction::Direction;

/// 任务去重键：上传为 `(Upload, file_sign)`，下载为 `(Download, url)`。
pub(crate) type UniqueId = (Direction, String);

/// Formats a scheduler key for logging without leaking secrets.
///
/// For download tasks the key's string component is the (possibly SAS/presigned)
/// URL, so it is run through [`crate::log::sanitize_url`] to strip signature
/// query parameters. Upload keys hold a file signature and pass through
/// unchanged (no query string). Use this anywhere a key reaches a persisted log
/// (`Key`/`Warn`/`Error`) instead of `{:?}` on the raw key.
pub(crate) fn safe_key(key: &UniqueId) -> String {
    format!("({:?}, {})", key.0, crate::log::sanitize_url(&key.1))
}

pub(crate) mod active_state;
pub(crate) mod cb_dispatcher;
pub(crate) mod exec_impl;
pub(crate) mod executor;
pub(crate) mod group_state;
pub(crate) mod inner_task;
pub(crate) mod scheduler_state;
pub(crate) mod sign;
pub(crate) mod task_callbacks;
pub(crate) mod worker_event;
