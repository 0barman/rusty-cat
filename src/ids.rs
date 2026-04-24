use std::fmt;

use uuid::Uuid;

/// Task ID returned by [`crate::MeowClient::try_enqueue`].
///
/// Use this ID for pause/resume/cancel operations on the same task.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskId(Uuid);

impl TaskId {
    /// Generates a random v4 task ID.
    pub(crate) fn new_v4() -> Self {
        Self(Uuid::new_v4())
    }
}

impl fmt::Debug for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// Listener ID returned by
/// [`crate::MeowClient::register_global_progress_listener`].
///
/// Use this ID to unregister the listener later.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlobalProgressListenerId(Uuid);

impl GlobalProgressListenerId {
    /// Generates a random v4 listener ID.
    pub(crate) fn new_v4() -> Self {
        Self(Uuid::new_v4())
    }
}

impl fmt::Debug for GlobalProgressListenerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}
