use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_GLOBAL_PROGRESS_LISTENER_ID: AtomicU64 = AtomicU64::new(1);

fn next_non_zero(counter: &AtomicU64) -> u64 {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            Some(
                current
                    .checked_add(1)
                    .filter(|next| *next != 0)
                    .unwrap_or(1),
            )
        })
        .unwrap_or_else(|current| current)
}

/// Task ID returned by [`crate::MeowClient::try_enqueue`].
///
/// Use this ID for pause/resume/cancel operations on the same task.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskId(u64);

impl TaskId {
    /// Generates a process-local unique task ID.
    pub(crate) fn new() -> Self {
        Self(next_non_zero(&NEXT_TASK_ID))
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
pub struct GlobalProgressListenerId(u64);

impl GlobalProgressListenerId {
    /// Generates a process-local unique listener ID.
    pub(crate) fn new() -> Self {
        Self(next_non_zero(&NEXT_GLOBAL_PROGRESS_LISTENER_ID))
    }
}

impl fmt::Debug for GlobalProgressListenerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}
