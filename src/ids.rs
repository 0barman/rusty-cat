use std::fmt;

use uuid::Uuid;

/// 由 [`crate::MeowClient::enqueue`] 返回，用于后续暂停、取消等操作关联同一传输任务。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskId(Uuid);

impl TaskId {
    pub(crate) fn new_v4() -> Self {
        Self(Uuid::new_v4())
    }
}

impl fmt::Debug for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

/// 通过 [`crate::MeowClient::register_global_progress_listener`] 注册后返回，用于 [`crate::MeowClient::unregister_global_progress_listener`]。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlobalProgressListenerId(Uuid);

impl GlobalProgressListenerId {
    pub(crate) fn new_v4() -> Self {
        Self(Uuid::new_v4())
    }
}

impl fmt::Debug for GlobalProgressListenerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}
