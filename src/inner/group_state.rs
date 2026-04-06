use crate::inner::inner_task::InnerTask;
use crate::inner::task_callbacks::TaskCallbacks;

pub(crate) struct RecordEntry {
    inner: InnerTask,
    callbacks: TaskCallbacks,
}

impl RecordEntry {
    pub(crate) fn new(inner: InnerTask, callbacks: TaskCallbacks) -> Self {
        Self { inner, callbacks }
    }

    pub(crate) fn inner(&self) -> &InnerTask {
        &self.inner
    }

    pub fn callbacks(&self) -> &TaskCallbacks {
        &self.callbacks
    }
}

pub(crate) struct GroupState {
    leader_inner: InnerTask,
    entry: RecordEntry,
}

impl GroupState {
    pub(crate) fn new(leader_inner: InnerTask, entry: RecordEntry) -> Self {
        Self {
            leader_inner,
            entry,
        }
    }

    pub(crate) fn leader_inner(&self) -> &InnerTask {
        &self.leader_inner
    }

    pub fn entry(&self) -> &RecordEntry {
        &self.entry
    }
}
