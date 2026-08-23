use std::sync::Arc;

use crate::file_transfer_record::FileTransferRecord;
use crate::ids::TaskId;
use crate::inner::group_state::RecordEntry;
use crate::inner::inner_task::InnerTask;
use crate::inner::scheduler_state::SchedulerState;
use crate::inner::task_callbacks::{CompleteCb, ProgressCb};
use crate::inner::UniqueId;
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
/// 在锁内只 clone 一个不可变 listener `Arc` 快照，立刻释放读锁；后续
/// 每个监听器各投递一次（`dto` 跨监听器 clone），因此监听器内部的
/// 注册/注销动作不会与这里产生重入死锁。
pub(crate) fn emit_global_progress(state: &SchedulerState, dto: FileTransferRecord) {
    let listeners = match state.global_progress_listener().read() {
        Ok(g) => Arc::clone(&g),
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
    for (_, cb) in listeners.iter() {
        invoke_progress_cb(state, cb, dto.clone());
    }
}

/// 汰选一个组当前最可信的文件总大小。
///
/// 运行期探测到的 total（来自 worker `Progress` 事件，经 prepare/分片响应得到，
/// 见 [`SchedulerState::known_totals`]）是权威真值，优先采用；仅当尚无运行期
/// 记录时，回退到构建期预设值（`with_total_size`，下载默认 0）。两者都缺失时
/// 返回 0，由 [`emit_status`] 按"总大小未知"处理（progress 记 0.0）。
///
/// **不用 `max(runtime, preset)`**：调用方可能把预设值设得比真实对象更大
/// （例如预设 10000 但远端 hint 为 4096）。若与运行期取 max，会把对外上报的
/// total 永久抬高到错误的预设值，使进度永远到不了 100%。因此运行期值一旦出现
/// 就无条件采信它，预设值只作为"运行期尚未知"时的兜底。
pub(crate) fn effective_total(state: &SchedulerState, key: &UniqueId, inner: &InnerTask) -> u64 {
    match state.known_totals().get(key).copied() {
        Some(v) if v > 0 => v,
        _ => inner.total_size(),
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

#[cfg(test)]
mod tests {
    use crate::inner::test_support::{live_download_state, live_download_state_with_preset};

    fn inner_of(
        state: &crate::inner::scheduler_state::SchedulerState,
        key: &crate::inner::UniqueId,
    ) -> crate::inner::inner_task::InnerTask {
        state
            .groups()
            .get(key)
            .expect("group")
            .entry()
            .inner()
            .clone()
    }

    /// 运行期值存在（下载常态：预设 0）→ 取运行期值。
    #[tokio::test]
    async fn effective_total_prefers_runtime_over_unset_preset() {
        let (mut state, key) = live_download_state("eff_runtime").await;
        state.known_totals_mut().insert(key.clone(), 4096);
        let inner = inner_of(&state, &key);
        assert_eq!(super::effective_total(&state, &key, &inner), 4096);
    }

    /// 双方都为 0 → 返回 0（total 未知，由 emit_status 按未知处理）。
    #[tokio::test]
    async fn effective_total_returns_zero_when_nothing_known() {
        let (state, key) = live_download_state("eff_zero").await;
        let inner = inner_of(&state, &key);
        assert_eq!(super::effective_total(&state, &key, &inner), 0);
    }

    /// 无运行期记录时回退到预设值（with_total_size）。
    #[tokio::test]
    async fn effective_total_falls_back_to_preset_when_no_runtime() {
        let (state, key) = live_download_state_with_preset("eff_fallback", 8192).await;
        let inner = inner_of(&state, &key);
        assert_eq!(super::effective_total(&state, &key, &inner), 8192);
    }

    /// 回归防护（finding #5）：运行期真值小于预设值时，必须采信运行期真值，
    /// 不得因取 max 把 total 永久抬高到错误的预设值。
    #[tokio::test]
    async fn effective_total_runtime_wins_over_larger_preset() {
        let (mut state, key) = live_download_state_with_preset("eff_shrink", 10000).await;
        state.known_totals_mut().insert(key.clone(), 4096);
        let inner = inner_of(&state, &key);
        assert_eq!(
            super::effective_total(&state, &key, &inner),
            4096,
            "运行期探测值必须压过更大的预设值"
        );
    }
}
