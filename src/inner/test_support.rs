//! 单元测试共享基建：构造"未预设 total 的下载组"调度器状态、
//! 采集全局监听器收到的 FileTransferRecord、提供轮询等待工具。
//! 仅在 `cfg(test)` 下编译，不进入发布产物。

use std::sync::{Arc, Mutex, RwLock};

use crate::dflt::default_http_transfer::default_breakpoint_arcs;
use crate::down_pounce_builder::DownloadPounceBuilder;
use crate::file_transfer_record::FileTransferRecord;
use crate::http_breakpoint::BreakpointDownloadHttpConfig;
use crate::ids::GlobalProgressListenerId;
use crate::inner::cb_dispatcher;
use crate::inner::group_state::{GroupState, RecordEntry};
use crate::inner::inner_task::InnerTask;
use crate::inner::scheduler_state::SchedulerState;
use crate::inner::task_callbacks::{ProgressCb, TaskCallbacks};
use crate::inner::UniqueId;

/// 构造持有一个"未预设 total"下载组的调度器状态，模拟真实用户最常见的
/// 用法（DownloadPounceBuilder 默认 total_size=0，真实大小靠 prepare 探测）。
/// `url_tag` 用于隔离不同用例的去重键（下载 dedupe key 即 URL）。
pub(crate) async fn live_download_state(url_tag: &str) -> (SchedulerState, UniqueId) {
    live_download_state_inner(url_tag, None).await
}

/// 同 [`live_download_state`]，但通过 `with_total_size` 预设一个构建期 total，
/// 用于验证运行期真值与预设值不一致时的汰选行为。
pub(crate) async fn live_download_state_with_preset(
    url_tag: &str,
    preset: u64,
) -> (SchedulerState, UniqueId) {
    live_download_state_inner(url_tag, Some(preset)).await
}

async fn live_download_state_inner(
    url_tag: &str,
    preset: Option<u64>,
) -> (SchedulerState, UniqueId) {
    let mut path = std::env::temp_dir();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    path.push(format!("rusty_cat_known_totals_{url_tag}_{ts}.bin"));

    let mut builder = DownloadPounceBuilder::new(
        "known-totals.bin",
        &path,
        1024,
        format!("https://placeholder.invalid/{url_tag}/{ts}"),
    );
    if let Some(preset) = preset {
        builder = builder.with_total_size(preset);
    }
    let pounce = builder.build();
    let (def_up, def_down) = default_breakpoint_arcs();
    let inner = InnerTask::from_pounce(
        pounce,
        BreakpointDownloadHttpConfig::default(),
        None,
        def_up,
        def_down,
    )
    .await
    .expect("from_pounce");

    let key = inner.dedupe_key();
    let (cb_submit, cb_join) = cb_dispatcher::start().expect("start dispatcher");
    // 与 handle_worker_event 既有测试相同的理由：detach join 守卫，
    // 避免断言 panic 时 sender 尚存导致 Drop 阻塞死锁。
    std::mem::forget(cb_join);
    let mut state = SchedulerState::new(1, 1, Arc::new(RwLock::new(Vec::new())), cb_submit);

    state
        .task_id_to_dedupe_mut()
        .insert(inner.task_id(), key.clone());
    state.offsets_mut().insert(key.clone(), 0);
    let entry = RecordEntry::new(inner.clone(), TaskCallbacks::empty());
    state
        .groups_mut()
        .insert(key.clone(), GroupState::new(inner.clone(), entry));

    (state, key)
}

/// 向调度状态的全局监听器列表挂一个采集器，把每条记录写入共享 Vec。
/// 记录经独立分发线程投递，测试侧用 [`wait_for_record`] 轮询同步。
pub(crate) fn attach_capture(state: &SchedulerState) -> Arc<Mutex<Vec<FileTransferRecord>>> {
    let records: Arc<Mutex<Vec<FileTransferRecord>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = records.clone();
    let cb: ProgressCb = Arc::new(move |rec: FileTransferRecord| {
        sink.lock().expect("records lock").push(rec);
    });
    state
        .global_progress_listener()
        .write()
        .expect("listener lock")
        .push((GlobalProgressListenerId::new(), cb));
    records
}

/// 轮询等待首条满足条件的记录（2 秒超时 panic，避免测试静默挂起）。
pub(crate) async fn wait_for_record<F>(
    records: &Arc<Mutex<Vec<FileTransferRecord>>>,
    mut pred: F,
) -> FileTransferRecord
where
    F: FnMut(&FileTransferRecord) -> bool,
{
    for _ in 0..200 {
        if let Some(rec) = records
            .lock()
            .expect("records lock")
            .iter()
            .find(|r| pred(r))
            .cloned()
        {
            return rec;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("expected record did not arrive within 2s");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// harness 冒烟：构造出的下载组预设 total 必须为 0（这是全套用例的前提）。
    #[tokio::test]
    async fn download_harness_builds_group_with_zero_preset_total() {
        let (state, key) = live_download_state("smoke").await;
        let group = state.groups().get(&key).expect("group exists");
        assert_eq!(group.entry().inner().total_size(), 0);
        let records = attach_capture(&state);
        assert!(records.lock().expect("lock").is_empty());
    }
}
