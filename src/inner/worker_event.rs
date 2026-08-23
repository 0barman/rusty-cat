use crate::error::MeowError;

use super::UniqueId;

#[derive(Debug)]
pub(crate) enum WorkerEvent {
    Progress {
        key: UniqueId,
        next_offset: u64,
        total_size: u64,
    },
    Completed {
        key: UniqueId,
        total_size: u64,
        completion_payload: Option<String>,
    },
    Failed {
        key: UniqueId,
        error: MeowError,
        /// 失败时 worker 已知的文件总大小；0 表示未知（例如 prepare 前失败）。
        /// 调度器侧会与 `known_totals`/预设值取 max 后再发射终态记录。
        total_size: u64,
    },
    Canceled {
        key: UniqueId,
        /// 取消时 worker 已知的文件总大小；0 表示未知。语义同 `Failed::total_size`。
        total_size: u64,
    },
}

impl WorkerEvent {
    /// Whether handling this event can release a file-level scheduler slot.
    ///
    /// Kept as an explicit classifier so adding a new event variant cannot
    /// silently inherit the wrong scheduling behavior in the worker loop.
    pub(crate) fn may_change_scheduler_readiness(&self) -> bool {
        !matches!(self, Self::Progress { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::WorkerEvent;
    use crate::direction::Direction;
    use crate::error::{InnerErrorCode, MeowError};

    fn key() -> crate::inner::UniqueId {
        (Direction::Download, "scheduler-event".to_string())
    }

    #[test]
    fn progress_does_not_change_scheduler_readiness_but_terminal_events_do() {
        assert!(
            !WorkerEvent::Progress {
                key: key(),
                next_offset: 1,
                total_size: 2,
            }
            .may_change_scheduler_readiness(),
            "Progress neither releases an active slot nor makes a queued group runnable"
        );

        assert!(WorkerEvent::Completed {
            key: key(),
            total_size: 2,
            completion_payload: None,
        }
        .may_change_scheduler_readiness());
        assert!(WorkerEvent::Failed {
            key: key(),
            error: MeowError::from_code_str(InnerErrorCode::Unknown, "failed"),
            total_size: 2,
        }
        .may_change_scheduler_readiness());
        assert!(WorkerEvent::Canceled {
            key: key(),
            total_size: 2,
        }
        .may_change_scheduler_readiness());
    }
}
