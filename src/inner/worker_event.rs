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
