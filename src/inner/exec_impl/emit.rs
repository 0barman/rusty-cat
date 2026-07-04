use crate::file_transfer_record::FileTransferRecord;
use crate::ids::TaskId;
use crate::inner::group_state::RecordEntry;
use crate::inner::scheduler_state::SchedulerState;
use crate::inner::task_callbacks::{CompleteCb, ProgressCb};
use crate::transfer_status::TransferStatus;

/// 投递一次进度回调到分发线程。
///
/// 调用方依然以"事件已通过回调送达"的语义对待这条调用，但实际执行被搬到了
/// 独立线程：调度循环不会被用户回调阻塞。`Transmission` 中间帧在分发队列满
/// 时可能被丢弃；其他状态保证送达。详见
/// [`crate::inner::cb_dispatcher`] 模块文档。
pub(crate) fn invoke_progress_cb(state: &SchedulerState, cb: &ProgressCb, dto: FileTransferRecord) {
    let Some(submit) = state.cb_submit() else {
        crate::meow_trace_log!(
            "callback",
            "progress callback skipped: dispatcher already taken (closing)"
        );
        return;
    };
    submit.submit_progress(cb.clone(), dto);
}

/// 投递一次完成回调到分发线程。
///
/// 完成回调始终视为终态事件，使用阻塞投递确保送达；只有在调度器关闭后
/// 才会被吞掉（仅记录 debug 日志）。
pub(crate) fn invoke_complete_cb(
    state: &SchedulerState,
    cb: &CompleteCb,
    task_id: TaskId,
    payload: Option<String>,
) {
    let Some(submit) = state.cb_submit() else {
        crate::meow_warn_log!(
            "callback",
            "complete callback skipped: dispatcher already taken (closing)"
        );
        return;
    };
    submit.submit_complete(cb.clone(), task_id, payload);
}

/// 把一条进度记录广播到所有全局监听器。
///
/// 在锁内只 clone 各监听器的 `Arc<dyn Fn>`，立刻释放读锁；后续每个监听器
/// 各投递一次（`dto` 跨监听器 clone），因此监听器内部的注册/注销动作不会与
/// 这里产生重入死锁。
pub(crate) fn emit_global_progress(state: &SchedulerState, dto: FileTransferRecord) {
    let listeners: Vec<ProgressCb> = match state.global_progress_listener().read() {
        Ok(g) => g.iter().map(|(_, cb)| cb.clone()).collect(),
        Err(_) => {
            crate::meow_warn_log!(
                "emit_global_progress",
                "global listener lock poisoned; skip progress broadcast"
            );
            return;
        }
    };
    crate::meow_trace_log!(
        "emit_global_progress",
        "broadcast start: listener_count={} task_id={:?}",
        listeners.len(),
        dto.task_id()
    );
    for cb in listeners {
        invoke_progress_cb(state, &cb, dto.clone());
    }
}

pub(crate) fn emit_status(
    state: &SchedulerState,
    entry: &RecordEntry,
    status: TransferStatus,
    transferred: u64,
    total: u64,
) {
    crate::meow_trace_log!(
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
        inner.file_sign_arc(),
        inner.file_name_arc(),
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
        invoke_progress_cb(state, cb, dto.clone());
    }
    emit_global_progress(state, dto);
}
