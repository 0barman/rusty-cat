use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{InnerErrorCode, MeowError};

pub(crate) fn now_unix_secs() -> Result<u64, MeowError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|e| {
            MeowError::from_code(
                InnerErrorCode::InvalidTaskState,
                format!("system time before unix epoch: {e}"),
            )
        })
}
