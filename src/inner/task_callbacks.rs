use crate::file_transfer_record::FileTransferRecord;
use std::sync::Arc;

pub(crate) type ProgressCb = Arc<dyn Fn(FileTransferRecord) + Send + Sync + 'static>;

pub(crate) struct TaskCallbacks {
    progress_cb: Option<ProgressCb>,
}

impl TaskCallbacks {
    pub(crate) fn new(progress_cb: Option<ProgressCb>) -> Self {
        Self { progress_cb }
    }

    pub(crate) fn empty() -> Self {
        Self { progress_cb: None }
    }

    pub fn progress_cb(&self) -> &Option<ProgressCb> {
        &self.progress_cb
    }
}
