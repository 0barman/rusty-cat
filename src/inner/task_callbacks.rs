use crate::file_transfer_record::FileTransferRecord;
use crate::ids::TaskId;
use std::sync::Arc;

pub(crate) type ProgressCb = Arc<dyn Fn(FileTransferRecord) + Send + Sync + 'static>;
pub(crate) type CompleteCb = Arc<dyn Fn(TaskId, Option<String>) + Send + Sync + 'static>;

pub(crate) struct TaskCallbacks {
    progress_cb: Option<ProgressCb>,
    complete_cb: Option<CompleteCb>,
}

impl TaskCallbacks {
    pub(crate) fn new(progress_cb: Option<ProgressCb>, complete_cb: Option<CompleteCb>) -> Self {
        Self {
            progress_cb,
            complete_cb,
        }
    }

    pub(crate) fn empty() -> Self {
        Self {
            progress_cb: None,
            complete_cb: None,
        }
    }

    pub fn progress_cb(&self) -> &Option<ProgressCb> {
        &self.progress_cb
    }

    pub fn complete_cb(&self) -> &Option<CompleteCb> {
        &self.complete_cb
    }
}
