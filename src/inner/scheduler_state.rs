use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, RwLock};

use crate::direction::Direction;
use crate::ids::{GlobalProgressListenerId, TaskId};

use super::active_state::ActiveState;
use super::cb_dispatcher::CbSubmit;
use super::group_state::GroupState;
use super::task_callbacks::ProgressCb;
use super::UniqueId;

pub(crate) type GlobalProgressSnapshot = Arc<[(GlobalProgressListenerId, ProgressCb)]>;
pub(crate) type GlobalProgressStore = Arc<RwLock<GlobalProgressSnapshot>>;

pub(crate) struct SchedulerState {
    /// 上传方向的并发上限；调度器在尝试拉起队列任务时据此限制同时运行的上传组数量。
    max_upload_concurrency: usize,
    /// 下载方向的并发上限；与 `max_upload_concurrency` 对称，控制下载组的并行度。
    max_download_concurrency: usize,
    /// Byte-level backpressure shared by parallel parts from every active file
    /// owned by this client executor.
    parallel_memory_budget: crate::inner::exec_impl::memory_budget::ParallelMemoryBudget,
    /// 全局进度监听器集合（监听器 id + 回调）；每次状态变更时会广播到这里注册的回调。
    global_progress_listener: GlobalProgressStore,

    /// 去重键 -> 任务组状态；同一去重键（同 URL 下载或同签名上传）在任意时刻只保留一个组入口。
    groups: HashMap<UniqueId, GroupState>,
    /// 任务实例 [`TaskId`] -> 去重键；用于把外部 task_id（如 pause/cancel 入参）反查到内部分组键。
    task_id_to_dedupe: HashMap<TaskId, UniqueId>,
    /// 待调度队列（先进先出）；保存尚未启动但允许后续尝试拉起的去重键。
    queued: VecDeque<UniqueId>,
    /// `queued` 的集合镜像；用于 O(1) 判断某个键是否已在队列中，避免重复入队。
    queued_set: HashSet<UniqueId>,
    /// 已暂停但仍可恢复的任务组键集合；`resume` 会从这里取回并重新入队。
    paused_set: HashSet<UniqueId>,
    /// 正在执行中的任务组；值里持有取消令牌等运行态信息，供取消与并发统计使用。
    active: HashMap<UniqueId, ActiveState>,
    /// 已接受用户 cancel、正等待 worker 收敛的执行中组。
    ///
    /// 组和 active 状态在 worker 终态事件到达前必须保留：这样 Canceled
    /// 回调才不会早于文件 checkpoint、实际文件锁和 path lease 释放。
    canceling_set: HashSet<UniqueId>,
    /// Active group counts by direction. These are updated atomically with
    /// `active` through the methods below so scheduler capacity checks are O(1).
    active_uploads: usize,
    active_downloads: usize,
    /// 每个去重键的当前传输偏移（断点进度）；用于进度回调、失败恢复与重启续传。
    offsets: HashMap<UniqueId, u64>,
    /// 每个去重键在运行期发现的文件总大小（来自 worker 的 Progress 事件）。
    /// 下载任务构建时 total 往往未预设（为 0），真实值由 prepare/分片响应探测得到；
    /// 终态与控制面事件发射时优先读这里（见 `exec_impl::emit::effective_total`），
    /// 避免把已知总大小回退成未预设的 0。生命周期与 `offsets` 逐条对齐：
    /// Progress 时写入、终态事件清理、Close 时清空、pause 期间保留。
    known_totals: HashMap<UniqueId, u64>,
    /// 用户回调投递句柄；调度路径只通过它把 `(cb, dto)` 推到独立的回调线程，
    /// 不再在调度循环中同步调用闭包。
    /// 在 `Close` 命令处理路径中会被 `take()` 后 drop，从而关闭分发 channel
    /// 并触发分发线程退出，实现 close 与回调回放的同步。
    cb_submit: Option<CbSubmit>,
}

impl SchedulerState {
    pub(crate) fn new(
        max_upload_concurrency: usize,
        max_download_concurrency: usize,
        global_progress_listener: GlobalProgressStore,
        cb_submit: CbSubmit,
    ) -> Self {
        Self {
            max_upload_concurrency,
            max_download_concurrency,
            parallel_memory_budget:
                crate::inner::exec_impl::memory_budget::ParallelMemoryBudget::for_current_target(),
            global_progress_listener,
            groups: HashMap::new(),
            task_id_to_dedupe: HashMap::new(),
            queued: VecDeque::new(),
            queued_set: HashSet::new(),
            paused_set: HashSet::new(),
            active: HashMap::new(),
            canceling_set: HashSet::new(),
            active_uploads: 0,
            active_downloads: 0,
            offsets: HashMap::new(),
            known_totals: HashMap::new(),
            cb_submit: Some(cb_submit),
        }
    }

    pub(crate) fn max_upload_concurrency(&self) -> usize {
        self.max_upload_concurrency
    }

    pub(crate) fn max_download_concurrency(&self) -> usize {
        self.max_download_concurrency
    }

    pub(crate) fn parallel_memory_budget(
        &self,
    ) -> &crate::inner::exec_impl::memory_budget::ParallelMemoryBudget {
        &self.parallel_memory_budget
    }

    pub(crate) fn global_progress_listener(&self) -> &GlobalProgressStore {
        &self.global_progress_listener
    }

    pub(crate) fn groups(&self) -> &HashMap<UniqueId, GroupState> {
        &self.groups
    }

    pub(crate) fn groups_mut(&mut self) -> &mut HashMap<UniqueId, GroupState> {
        &mut self.groups
    }

    pub(crate) fn queued(&self) -> &VecDeque<UniqueId> {
        &self.queued
    }

    pub(crate) fn queued_mut(&mut self) -> &mut VecDeque<UniqueId> {
        &mut self.queued
    }

    pub(crate) fn queued_set(&self) -> &HashSet<UniqueId> {
        &self.queued_set
    }

    pub(crate) fn queued_set_mut(&mut self) -> &mut HashSet<UniqueId> {
        &mut self.queued_set
    }

    pub(crate) fn paused_set(&self) -> &HashSet<UniqueId> {
        &self.paused_set
    }

    pub(crate) fn paused_set_mut(&mut self) -> &mut HashSet<UniqueId> {
        &mut self.paused_set
    }

    pub(crate) fn active(&self) -> &HashMap<UniqueId, ActiveState> {
        &self.active
    }

    pub(crate) fn canceling_set(&self) -> &HashSet<UniqueId> {
        &self.canceling_set
    }

    pub(crate) fn canceling_set_mut(&mut self) -> &mut HashSet<UniqueId> {
        &mut self.canceling_set
    }

    pub(crate) fn active_direction_count(&self, direction: Direction) -> usize {
        match direction {
            Direction::Upload => self.active_uploads,
            Direction::Download => self.active_downloads,
        }
    }

    pub(crate) fn insert_active(
        &mut self,
        key: UniqueId,
        active: ActiveState,
    ) -> Option<ActiveState> {
        let direction = key.0;
        let previous = self.active.insert(key, active);
        if previous.is_none() {
            match direction {
                Direction::Upload => self.active_uploads += 1,
                Direction::Download => self.active_downloads += 1,
            }
        }
        self.debug_assert_active_counts();
        previous
    }

    pub(crate) fn remove_active(&mut self, key: &UniqueId) -> Option<ActiveState> {
        let previous = self.active.remove(key);
        if previous.is_some() {
            let count = match key.0 {
                Direction::Upload => &mut self.active_uploads,
                Direction::Download => &mut self.active_downloads,
            };
            debug_assert!(
                *count > 0,
                "active direction count must match the active map"
            );
            *count = count.saturating_sub(1);
        }
        self.debug_assert_active_counts();
        previous
    }

    pub(crate) fn clear_active(&mut self) {
        self.active.clear();
        self.active_uploads = 0;
        self.active_downloads = 0;
        self.debug_assert_active_counts();
    }

    #[cfg(debug_assertions)]
    fn debug_assert_active_counts(&self) {
        let uploads = self
            .active
            .keys()
            .filter(|key| key.0 == Direction::Upload)
            .count();
        let downloads = self
            .active
            .keys()
            .filter(|key| key.0 == Direction::Download)
            .count();
        debug_assert_eq!(self.active_uploads, uploads);
        debug_assert_eq!(self.active_downloads, downloads);
    }

    #[cfg(not(debug_assertions))]
    fn debug_assert_active_counts(&self) {}

    pub(crate) fn offsets(&self) -> &HashMap<UniqueId, u64> {
        &self.offsets
    }

    pub(crate) fn offsets_mut(&mut self) -> &mut HashMap<UniqueId, u64> {
        &mut self.offsets
    }

    pub(crate) fn known_totals(&self) -> &HashMap<UniqueId, u64> {
        &self.known_totals
    }

    pub(crate) fn known_totals_mut(&mut self) -> &mut HashMap<UniqueId, u64> {
        &mut self.known_totals
    }

    pub(crate) fn task_id_to_dedupe(&self) -> &HashMap<TaskId, UniqueId> {
        &self.task_id_to_dedupe
    }

    pub(crate) fn task_id_to_dedupe_mut(&mut self) -> &mut HashMap<TaskId, UniqueId> {
        &mut self.task_id_to_dedupe
    }

    /// 借用回调投递句柄。
    ///
    /// 在 `Close` 命令处理后半段（[`Self::take_cb_submit`] 之后）会返回 `None`，
    /// 调用方此时不应再发任何回调（事实上也不再有用户可见事件需要投递）。
    pub(crate) fn cb_submit(&self) -> Option<&CbSubmit> {
        self.cb_submit.as_ref()
    }

    /// 取出回调投递句柄并立即 drop。
    ///
    /// 调用后调度状态对该 channel 的所有 sender 引用都会被释放，分发线程在
    /// drain 完队列后自然退出。`worker_loop` 紧接着 `join` 该线程即可获得
    /// "close 时所有终态回调已完成"的同步语义。
    pub(crate) fn take_cb_submit(&mut self) -> Option<CbSubmit> {
        self.cb_submit.take()
    }
}

#[cfg(test)]
mod tests {
    use super::SchedulerState;
    use crate::direction::Direction;
    use crate::inner::active_state::ActiveState;
    use std::sync::{Arc, RwLock};
    use tokio_util::sync::CancellationToken;

    fn state() -> SchedulerState {
        let (cb_submit, cb_join) = crate::inner::cb_dispatcher::start().expect("dispatcher");
        std::mem::forget(cb_join);
        SchedulerState::new(3, 4, Arc::new(RwLock::new(Arc::from([]))), cb_submit)
    }

    fn key(direction: Direction, suffix: &str) -> crate::inner::UniqueId {
        (direction, suffix.to_string())
    }

    fn active() -> ActiveState {
        ActiveState::new(CancellationToken::new())
    }

    #[test]
    fn active_direction_counts_follow_insert_remove_and_clear_exactly_once() {
        let mut state = state();
        let upload = key(Direction::Upload, "upload");
        let download = key(Direction::Download, "download");

        assert_eq!(state.active_direction_count(Direction::Upload), 0);
        assert_eq!(state.active_direction_count(Direction::Download), 0);

        assert!(state.insert_active(upload.clone(), active()).is_none());
        assert!(state.insert_active(download.clone(), active()).is_none());
        assert_eq!(state.active_direction_count(Direction::Upload), 1);
        assert_eq!(state.active_direction_count(Direction::Download), 1);

        // Replacing the same key is not a second active group.
        assert!(state.insert_active(upload.clone(), active()).is_some());
        assert_eq!(state.active_direction_count(Direction::Upload), 1);

        assert!(state.remove_active(&upload).is_some());
        assert!(state.remove_active(&upload).is_none());
        assert_eq!(state.active_direction_count(Direction::Upload), 0);
        assert_eq!(state.active_direction_count(Direction::Download), 1);

        state.clear_active();
        assert!(state.active().is_empty());
        assert_eq!(state.active_direction_count(Direction::Upload), 0);
        assert_eq!(state.active_direction_count(Direction::Download), 0);
    }
}
