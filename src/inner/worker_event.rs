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
    },
    Canceled {
        key: UniqueId,
    },
}
