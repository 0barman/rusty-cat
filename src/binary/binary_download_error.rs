use crate::error::{InnerErrorCode, MeowError};

pub(crate) fn task_canceled() -> MeowError {
    MeowError::from_code_str(InnerErrorCode::TaskCanceled, "binary task was canceled")
}

pub(crate) fn client_closed() -> MeowError {
    MeowError::from_code_str(
        InnerErrorCode::ClientClosed,
        "meow client closed before binary task completed",
    )
}

pub(crate) fn task_not_found() -> MeowError {
    MeowError::from_code_str(InnerErrorCode::TaskNotFound, "binary task was not found")
}
