use crate::file_transfer_record::FileTransferRecord;
use crate::inner::group_state::RecordEntry;
use crate::inner::scheduler_state::SchedulerState;
use crate::inner::task_callbacks::ProgressCb;
use crate::transfer_status::TransferStatus;
use std::panic::{catch_unwind, AssertUnwindSafe};

pub(crate) fn invoke_progress_cb(cb: &ProgressCb, dto: FileTransferRecord) {
    // 用户回调 panic 不应冲击调度核心；吞掉 panic，继续调度流程。
    let ret = catch_unwind(AssertUnwindSafe(|| cb(dto)));
    if ret.is_err() {
        crate::meow_flow_log!(
            "callback",
            "progress callback panicked; panic suppressed to protect scheduler"
        );
    }
}

pub(crate) fn emit_global_progress(state: &SchedulerState, dto: FileTransferRecord) {
    // 在锁内只 clone 各监听器的 Arc，释放读锁后再逐个调用，避免回调里 register/unregister 死锁。
    let listeners: Vec<ProgressCb> = match state.global_progress_listener().read() {
        Ok(g) => g.iter().map(|(_, cb)| cb.clone()).collect(),
        Err(_) => {
            crate::meow_flow_log!(
                "emit_global_progress",
                "global listener lock poisoned; skip progress broadcast"
            );
            return;
        }
    };
    crate::meow_flow_log!(
        "emit_global_progress",
        "broadcast start: listener_count={} task_id={:?}",
        listeners.len(),
        dto.task_id()
    );
    for cb in listeners {
        invoke_progress_cb(&cb, dto.clone());
    }
}

pub(crate) fn emit_status(
    state: &SchedulerState,
    entry: &RecordEntry,
    status: TransferStatus,
    transferred: u64,
    total: u64,
) {
    crate::meow_flow_log!(
        "emit_status",
        "status emit start: task_id={:?} status={:?} transferred={} total={}",
        entry.inner().task_id(),
        status,
        transferred,
        total
    );
    let inner = entry.inner();
    let dto = FileTransferRecord::new(
        inner.task_id(),
        inner.file_sign().to_string(),
        inner.file_name().to_string(),
        total,
        if total == 0 {
            0.0
        } else {
            transferred as f32 / total as f32
        },
        status,
        inner.direction(),
    );
    if let Some(cb) = &entry.callbacks().progress_cb() {
        invoke_progress_cb(cb, dto.clone());
    }
    emit_global_progress(state, dto);
}
