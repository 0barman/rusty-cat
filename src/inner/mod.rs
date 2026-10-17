use crate::direction::Direction;

/// 任务去重键：上传为 `(Upload, file_sign)`，下载为 `(Download, url)`。
pub(crate) type UniqueId = (Direction, String);

/// Returns the key component safe for a persisted structured log field.
pub(crate) fn safe_object_key(key: &UniqueId) -> String {
    crate::log::sanitize_url(&key.1)
}

/// Formats a scheduler key for logging without leaking secrets.
///
/// For download tasks the key's string component is the (possibly SAS/presigned)
/// URL, so it is run through [`crate::log::sanitize_url`] to strip signature
/// query parameters. Upload keys hold a file signature and pass through
/// unchanged (no query string). Use this anywhere a key reaches a persisted log
/// (`Key`/`Warn`/`Error`) instead of `{:?}` on the raw key.
pub(crate) fn safe_key(key: &UniqueId) -> String {
    format!("({:?}, {})", key.0, safe_object_key(key))
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
#[cfg(test)]
pub(crate) mod test_support;
pub(crate) mod worker_event;

#[cfg(test)]
mod tests {
    use super::{safe_object_key, Direction};

    #[test]
    fn structured_download_key_redacts_secret_query_but_keeps_identity_context() {
        let key = (
            Direction::Download,
            "https://example.test/object?version=v1&sig=RC_SECRET_KEY_CANARY".to_string(),
        );
        let safe = safe_object_key(&key);
        assert_eq!(safe, "https://example.test/object?version=v1&sig=REDACTED");
        assert!(!safe.contains("RC_SECRET_KEY_CANARY"));
    }

    #[test]
    fn structured_upload_key_without_query_is_unchanged() {
        let key = (Direction::Upload, "0123456789abcdef".to_string());
        assert_eq!(safe_object_key(&key), key.1);
    }
}
