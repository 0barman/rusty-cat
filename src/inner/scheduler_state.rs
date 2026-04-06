use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, RwLock};

use crate::ids::{GlobalProgressListenerId, TaskId};

use super::active_state::ActiveState;
use super::group_state::GroupState;
use super::task_callbacks::ProgressCb;
use super::UniqueId;

pub(crate) struct SchedulerState {
    /// 上传方向的并发上限；调度器在尝试拉起队列任务时据此限制同时运行的上传组数量。
    max_upload_concurrency: usize,
    /// 下载方向的并发上限；与 `max_upload_concurrency` 对称，控制下载组的并行度。
    max_download_concurrency: usize,
    /// 全局进度监听器集合（监听器 id + 回调）；每次状态变更时会广播到这里注册的回调。
    global_progress_listener: Arc<RwLock<Vec<(GlobalProgressListenerId, ProgressCb)>>>,

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
    /// 每个去重键的当前传输偏移（断点进度）；用于进度回调、失败恢复与重启续传。
    offsets: HashMap<UniqueId, u64>,
}

impl SchedulerState {
    pub(crate) fn new(
        max_upload_concurrency: usize,
        max_download_concurrency: usize,
        global_progress_listener: Arc<RwLock<Vec<(GlobalProgressListenerId, ProgressCb)>>>,
    ) -> Self {
        Self {
            max_upload_concurrency,
            max_download_concurrency,
            global_progress_listener,
            groups: HashMap::new(),
            task_id_to_dedupe: HashMap::new(),
            queued: VecDeque::new(),
            queued_set: HashSet::new(),
            paused_set: HashSet::new(),
            active: HashMap::new(),
            offsets: HashMap::new(),
        }
    }

    pub(crate) fn max_upload_concurrency(&self) -> usize {
        self.max_upload_concurrency
    }

    pub(crate) fn max_download_concurrency(&self) -> usize {
        self.max_download_concurrency
    }

    pub(crate) fn global_progress_listener(
        &self,
    ) -> &Arc<RwLock<Vec<(GlobalProgressListenerId, ProgressCb)>>> {
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

    pub(crate) fn active_mut(&mut self) -> &mut HashMap<UniqueId, ActiveState> {
        &mut self.active
    }

    pub(crate) fn offsets(&self) -> &HashMap<UniqueId, u64> {
        &self.offsets
    }

    pub(crate) fn offsets_mut(&mut self) -> &mut HashMap<UniqueId, u64> {
        &mut self.offsets
    }

    pub(crate) fn task_id_to_dedupe(&self) -> &HashMap<TaskId, UniqueId> {
        &self.task_id_to_dedupe
    }

    pub(crate) fn task_id_to_dedupe_mut(&mut self) -> &mut HashMap<TaskId, UniqueId> {
        &mut self.task_id_to_dedupe
    }
}
