use crate::error::{InnerErrorCode, MeowError};
use crate::file_transfer_record::FileTransferRecord;
use crate::ids::{GlobalProgressListenerId, TaskId};
use crate::inner::group_state::{GroupState, RecordEntry};
use crate::inner::inner_task::InnerTask;
use crate::inner::scheduler_state::SchedulerState;
use crate::inner::task_callbacks::{ProgressCb, TaskCallbacks};
use crate::inner::worker_event::WorkerEvent;
use crate::inner::UniqueId;
use crate::meow_config::MeowConfig;
use crate::transfer_executor_trait::TransferTrait;
use crate::transfer_snapshot::TransferSnapshot;
use crate::transfer_status::TransferStatus;
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;

pub(crate) enum TransferCmd {
    Enqueue {
        inner: InnerTask,
        callbacks: TaskCallbacks,
    },
    Pause {
        task_id: TaskId,
        /// 控制命令应答通道：返回 pause 的最终结果。
        respond_to: tokio::sync::oneshot::Sender<Result<(), MeowError>>,
    },
    /// 恢复一个此前 pause 的任务；语义是“同 task_id 继续执行”。
    Resume {
        /// 外部暴露的任务 id，用于反查内部 dedupe key。
        task_id: TaskId,
        /// 控制命令应答通道：返回 resume 的最终结果。
        respond_to: tokio::sync::oneshot::Sender<Result<(), MeowError>>,
    },
    Cancel {
        task_id: TaskId,
        /// 控制命令应答通道：返回 cancel 的最终结果。
        respond_to: tokio::sync::oneshot::Sender<Result<(), MeowError>>,
    },
    Snapshot {
        respond_to: tokio::sync::oneshot::Sender<TransferSnapshot>,
    },
    Close {
        /// 控制命令应答通道：仅在 worker 完成清理后返回。
        respond_to: tokio::sync::oneshot::Sender<Result<(), MeowError>>,
    },
}

fn worker_loop(
    mut cmd_rx: mpsc::Receiver<TransferCmd>,
    mut worker_rx: mpsc::Receiver<WorkerEvent>,
    worker_tx: mpsc::Sender<WorkerEvent>,
    mut state: SchedulerState,
    executor: Arc<dyn TransferTrait>,
) -> Result<(), MeowError> {
    crate::meow_flow_log!("worker_loop", "starting scheduler worker thread");
    let (startup_tx, startup_rx) = std::sync::mpsc::sync_channel::<Result<(), MeowError>>(1);
    std::thread::spawn(move || {
        let runtime_ret = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build();
        match runtime_ret {
            Ok(runtime) => {
                crate::meow_flow_log!("worker_loop", "runtime created successfully");
                let _ = startup_tx.send(Ok(()));
                runtime.block_on(async move {
                    loop {
                        tokio::select! {
                            biased;
                            maybe_cmd = cmd_rx.recv() => {
                                let Some(cmd) = maybe_cmd else { break; };
                                match cmd {
                                    TransferCmd::Enqueue { inner, callbacks } => {
                                        let key = inner.dedupe_key();
                                        crate::meow_flow_log!(
                                            "cmd_enqueue",
                                            "received enqueue: task_id={:?} key={:?} chunk_size={}",
                                            inner.task_id(),
                                            key,
                                            inner.chunk_size()
                                        );
                                        if let Some(existing) = state.groups().get(&key) {
                                            let leader = existing.leader_inner();
                                            let dup_dto = to_record_inner(
                                                leader,
                                                TransferStatus::Failed(MeowError::from_code1(
                                                    InnerErrorCode::DuplicateTaskError,
                                                )),
                                                0,
                                                leader.total_size(),
                                            );
                                            if let Some(cb) = &callbacks.progress_cb() {
                                                crate::inner::exec_impl::emit::invoke_progress_cb(
                                                    cb,
                                                    dup_dto.clone(),
                                                );
                                            }
                                            crate::inner::exec_impl::emit::emit_global_progress(
                                                &state,
                                                dup_dto,
                                            );
                                            crate::meow_flow_log!(
                                                "cmd_enqueue",
                                                "duplicate key rejected: key={:?}",
                                                key
                                            );
                                            continue;
                                        }

                                        state
                                            .task_id_to_dedupe_mut()
                                            .insert(inner.task_id(), key.clone());

                                        let should_send_running = state.active().contains_key(&key);
                                        let entry = RecordEntry::new(inner.clone(), callbacks);
                                        state.groups_mut().insert(
                                            key.clone(),
                                            GroupState::new(inner.clone(), entry),
                                        );

                                        if let Some(group) = state.groups().get(&key) {
                                            let current = state.offsets().get(&key).copied().unwrap_or(0);
                                            crate::inner::exec_impl::emit::emit_status(
                                                &state,
                                                group.entry(),
                                                TransferStatus::Pending,
                                                current,
                                                group.entry().inner().total_size(),
                                            );
                                            if should_send_running {
                                                crate::inner::exec_impl::emit::emit_status(
                                                    &state,
                                                    group.entry(),
                                                    TransferStatus::Transmission,
                                                    current,
                                                    group.entry().inner().total_size(),
                                                );
                                            }
                                        }

                                        if !state.active().contains_key(&key) && !state.queued_set().contains(&key) {
                                            state.queued_mut().push_back(key.clone());
                                            state.queued_set_mut().insert(key.clone());
                                            crate::meow_flow_log!(
                                                "cmd_enqueue",
                                                "queued new key: key={:?} queued_len={}",
                                                key,
                                                state.queued().len()
                                            );
                                        }
                                        let _ = crate::inner::exec_impl::exec::try_start_next(
                                            &worker_tx,
                                            &mut state,
                                            &executor,
                                        )
                                        .await;
                                    }
                                    TransferCmd::Pause { task_id, respond_to } => {
                                        crate::meow_flow_log!(
                                            "cmd_pause",
                                            "pause requested: task_id={:?}",
                                            task_id
                                        );
                                        if let Some(key) = state.task_id_to_dedupe().get(&task_id).cloned()
                                        {
                                            pause_group(&mut state, &key).await;
                                            let _ = respond_to.send(Ok(()));
                                            crate::meow_flow_log!(
                                                "cmd_pause",
                                                "pause accepted: task_id={:?} key={:?}",
                                                task_id,
                                                key
                                            );
                                        } else {
                                            crate::meow_flow_log!(
                                                "cmd_pause",
                                                "pause failed task not found: task_id={:?}",
                                                task_id
                                            );
                                            let _ = respond_to.send(Err(task_not_found_error(task_id)));
                                        }
                                        let _ = crate::inner::exec_impl::exec::try_start_next(
                                            &worker_tx,
                                            &mut state,
                                            &executor,
                                        )
                                        .await;
                                    }
                                    TransferCmd::Resume { task_id, respond_to } => {
                                        crate::meow_flow_log!(
                                            "cmd_resume",
                                            "resume requested: task_id={:?}",
                                            task_id
                                        );
                                        if let Some(key) = state.task_id_to_dedupe().get(&task_id).cloned()
                                        {
                                            let resume_ret = resume_group(&mut state, &key).await;
                                            if let Err(e) = &resume_ret {
                                                crate::meow_flow_log!(
                                                    "cmd_resume",
                                                    "resume rejected: task_id={:?} key={:?} err={}",
                                                    task_id,
                                                    key,
                                                    e
                                                );
                                            } else {
                                                crate::meow_flow_log!(
                                                    "cmd_resume",
                                                    "resume accepted: task_id={:?} key={:?}",
                                                    task_id,
                                                    key
                                                );
                                            }
                                            let _ = respond_to.send(resume_ret);
                                        } else {
                                            crate::meow_flow_log!(
                                                "cmd_resume",
                                                "resume failed task not found: task_id={:?}",
                                                task_id
                                            );
                                            let _ = respond_to.send(Err(task_not_found_error(task_id)));
                                        }
                                        let _ = crate::inner::exec_impl::exec::try_start_next(
                                            &worker_tx,
                                            &mut state,
                                            &executor,
                                        )
                                        .await;
                                    }
                                    TransferCmd::Cancel { task_id, respond_to } => {
                                        crate::meow_flow_log!(
                                            "cmd_cancel",
                                            "cancel requested: task_id={:?}",
                                            task_id
                                        );
                                        if let Some(key) = state.task_id_to_dedupe().get(&task_id).cloned()
                                        {
                                            cancel_group(&mut state, &key).await;
                                            let _ = respond_to.send(Ok(()));
                                            crate::meow_flow_log!(
                                                "cmd_cancel",
                                                "cancel accepted: task_id={:?} key={:?}",
                                                task_id,
                                                key
                                            );
                                        } else {
                                            crate::meow_flow_log!(
                                                "cmd_cancel",
                                                "cancel failed task not found: task_id={:?}",
                                                task_id
                                            );
                                            let _ = respond_to.send(Err(task_not_found_error(task_id)));
                                        }
                                        let _ = crate::inner::exec_impl::exec::try_start_next(
                                            &worker_tx,
                                            &mut state,
                                            &executor,
                                        )
                                        .await;
                                    }
                                    TransferCmd::Snapshot { respond_to } => {
                                        crate::meow_flow_log!(
                                            "cmd_snapshot",
                                            "snapshot requested: queued={} active={} paused={}",
                                            state.queued().len(),
                                            state.active().len(),
                                            state.paused_set().len()
                                        );
                                        let _ = respond_to.send(TransferSnapshot {
                                            queued_groups: state.queued().len(),
                                            active_groups: state.active().len(),
                                            active_keys: state.active().keys().cloned().collect(),
                                        });
                                    }
                                    TransferCmd::Close { respond_to } => {
                                        crate::meow_flow_log!(
                                            "cmd_close",
                                            "close requested: active={} groups={} queued={} paused={}",
                                            state.active().len(),
                                            state.groups().len(),
                                            state.queued().len(),
                                            state.paused_set().len()
                                        );
                                        for (_, active) in state.active().iter() {
                                            active.cancel().cancel();
                                        }
                                        for (key, group) in state.groups().iter() {
                                            let current = state.offsets().get(key).copied().unwrap_or(0);
                                            crate::inner::exec_impl::emit::emit_status(
                                                &state,
                                                group.entry(),
                                                TransferStatus::Paused,
                                                current,
                                                group.entry().inner().total_size(),
                                            );
                                        }
                                        state.active_mut().clear();
                                        state.groups_mut().clear();
                                        state.task_id_to_dedupe_mut().clear();
                                        state.queued_mut().clear();
                                        state.queued_set_mut().clear();
                                        state.paused_set_mut().clear();
                                        state.offsets_mut().clear();
                                        crate::meow_flow_log!("cmd_close", "close finished, worker loop exiting");
                                        let _ = respond_to.send(Ok(()));
                                        break;
                                    }
                                }
                            }
                            maybe_event = worker_rx.recv() => {
                                let Some(event) = maybe_event else { continue; };
                                crate::meow_flow_log!("worker_loop", "worker event received");
                                crate::inner::exec_impl::handle_worker_event::handle_worker_event(
                                    event,
                                    &mut state,
                                )
                                .await;
                                let _ = crate::inner::exec_impl::exec::try_start_next(
                                    &worker_tx,
                                    &mut state,
                                    &executor,
                                )
                                .await;
                            }
                        }
                    }
                });
            }
            Err(e) => {
                crate::meow_flow_log!("worker_loop", "runtime creation failed: {}", e);
                let _ = startup_tx.send(Err(MeowError::from_code(
                    InnerErrorCode::RuntimeCreationFailedError,
                    format!("runtime build failed: {}", e),
                )));
            }
        }
    });
    startup_rx.recv().map_err(|e| {
        MeowError::from_code(
            InnerErrorCode::RuntimeCreationFailedError,
            format!("runtime startup handshake failed: {}", e),
        )
    })?
}
async fn pause_group(state: &mut SchedulerState, key: &UniqueId) {
    crate::meow_flow_log!(
        "pause_group",
        "pause begin: key={:?} active={} queued={} paused={}",
        key,
        state.active().contains_key(key),
        state.queued_set().contains(key),
        state.paused_set().contains(key)
    );
    // 暂停语义要求“可恢复”，因此先从可运行集合移除，避免继续调度/执行。
    if let Some(active) = state.active().get(key) {
        // 对正在执行的组发出取消信号，让 worker 退出循环并回到可恢复状态。
        // 注意：这里不立刻从 active 移除，避免 resume 与 Canceled 事件发生竞态。
        active.cancel().cancel();
    }
    // 若任务尚在等待队列中，暂停后应立刻从队列剔除。
    state.queued_mut().retain(|k| k != key);
    // 同步更新队列镜像集合，确保状态一致。
    state.queued_set_mut().remove(key);
    // 关键：标记为 paused，而不是删除 group/mapping，这样 resume 才能找到原任务。
    state.paused_set_mut().insert(key.clone());

    if let Some(group) = state.groups().get(key) {
        // 发送 Paused 事件给回调层，告诉调用方任务已进入可恢复暂停态。
        let entry = group.entry();
        // 使用当前 offset 作为暂停进度，保持对外可观测进度连续。
        let current = state.offsets().get(key).copied().unwrap_or(0);
        crate::inner::exec_impl::emit::emit_status(
            state,
            entry,
            TransferStatus::Paused,
            current,
            entry.inner().total_size(),
        );
        crate::meow_flow_log!(
            "pause_group",
            "pause status emitted: key={:?} offset={}",
            key,
            current
        );
    }
}

async fn resume_group(state: &mut SchedulerState, key: &UniqueId) -> Result<(), MeowError> {
    crate::meow_flow_log!(
        "resume_group",
        "resume begin: key={:?} active={} queued={} paused={}",
        key,
        state.active().contains_key(key),
        state.queued_set().contains(key),
        state.paused_set().contains(key)
    );
    // 非 paused 任务不允许 resume，防止错误状态转换。
    if !state.paused_set().contains(key) {
        crate::meow_flow_log!("resume_group", "resume rejected not paused: key={:?}", key);
        return Err(MeowError::from_code(
            InnerErrorCode::InvalidTaskState,
            "resume target is not paused".to_string(),
        ));
    }
    // 仍在 active 说明 pause 正在收敛中，先拒绝本次 resume，避免和取消事件竞态。
    if state.active().contains_key(key) {
        crate::meow_flow_log!(
            "resume_group",
            "resume rejected still stopping: key={:?}",
            key
        );
        return Err(MeowError::from_code(
            InnerErrorCode::InvalidTaskState,
            "resume target is still stopping, retry later".to_string(),
        ));
    }
    // paused 标记在恢复时移除，表示该任务重新进入调度生命周期。
    state.paused_set_mut().remove(key);
    // 若组已不存在则属于内部状态异常，直接报错而不是静默忽略。
    let Some(group) = state.groups().get(key) else {
        crate::meow_flow_log!(
            "resume_group",
            "resume rejected missing group: key={:?}",
            key
        );
        return Err(MeowError::from_code(
            InnerErrorCode::InvalidTaskState,
            "resume target group missing".to_string(),
        ));
    };
    // 当前 offset 继续作为恢复起点，通知外部进入 Pending（待调度）状态。
    let current = state.offsets().get(key).copied().unwrap_or(0);
    crate::inner::exec_impl::emit::emit_status(
        state,
        group.entry(),
        TransferStatus::Pending,
        current,
        group.entry().inner().total_size(),
    );
    // 仅当不在 active 且不在 queued 时重新入队，避免重复排队。
    if !state.active().contains_key(key) && !state.queued_set().contains(key) {
        state.queued_mut().push_back(key.clone());
        state.queued_set_mut().insert(key.clone());
        crate::meow_flow_log!(
            "resume_group",
            "resume requeued key={:?} queued_len={}",
            key,
            state.queued().len()
        );
    }
    crate::meow_flow_log!("resume_group", "resume success: key={:?}", key);
    Ok(())
}

async fn cancel_group(state: &mut SchedulerState, key: &UniqueId) {
    crate::meow_flow_log!(
        "cancel_group",
        "cancel begin: key={:?} active={} queued={} paused={}",
        key,
        state.active().contains_key(key),
        state.queued_set().contains(key),
        state.paused_set().contains(key)
    );
    // 取消优先终止执行态。
    if let Some(active) = state.active_mut().remove(key) {
        active.cancel().cancel();
    }
    // 取消后不应继续排队。
    state.queued_mut().retain(|k| k != key);
    state.queued_set_mut().remove(key);
    // 取消语义会彻底结束任务，因此需要清掉 paused 标记。
    state.paused_set_mut().remove(key);
    if let Some(group) = state.groups_mut().remove(key) {
        // 取消后删除 task_id 映射，防止继续通过旧 id 控制。
        state
            .task_id_to_dedupe_mut()
            .remove(&group.leader_inner().task_id());
        let entry = group.entry();
        let current = state.offsets().get(key).copied().unwrap_or(0);
        crate::inner::exec_impl::emit::emit_status(
            state,
            entry,
            TransferStatus::Canceled,
            current,
            entry.inner().total_size(),
        );
        crate::meow_flow_log!(
            "cancel_group",
            "cancel status emitted: key={:?} offset={}",
            key,
            current
        );
    }
}

fn task_not_found_error(task_id: TaskId) -> MeowError {
    MeowError::from_code(
        InnerErrorCode::TaskNotFound,
        format!("task not found: {:?}", task_id),
    )
}

#[derive(Debug, Clone)]
pub(crate) struct Executor {
    cmd_tx: tokio::sync::mpsc::Sender<TransferCmd>,
}

impl Executor {
    pub(crate) fn new(
        config: MeowConfig,
        executor: Arc<dyn TransferTrait>,
        global_progress_listener: Arc<RwLock<Vec<(GlobalProgressListenerId, ProgressCb)>>>,
    ) -> Result<Self, MeowError> {
        crate::meow_flow_log!(
            "executor",
            "executor new: max_upload={} max_download={}",
            config.max_upload_concurrency(),
            config.max_download_concurrency()
        );
        let (cmd_tx, cmd_rx) = mpsc::channel::<TransferCmd>(256);
        let (worker_tx, worker_rx) = mpsc::channel::<WorkerEvent>(1024);
        worker_loop(
            cmd_rx,
            worker_rx,
            worker_tx,
            SchedulerState::new(
                config.max_upload_concurrency(),
                config.max_download_concurrency(),
                global_progress_listener,
            ),
            executor,
        )?;
        crate::meow_flow_log!("executor", "executor worker started");
        Ok(Self { cmd_tx })
    }
}

impl Executor {
    pub(crate) fn enqueue(
        &self,
        inner: InnerTask,
        callbacks: TaskCallbacks,
    ) -> Result<TaskId, MeowError> {
        let id = inner.task_id();
        crate::meow_flow_log!(
            "executor_api",
            "enqueue send: task_id={:?} key={:?}",
            id,
            inner.dedupe_key()
        );
        self.cmd_tx
            .try_send(TransferCmd::Enqueue { inner, callbacks })
            .map_err(|e| {
                crate::meow_flow_log!(
                    "executor_api",
                    "enqueue send failed: task_id={:?} err={}",
                    id,
                    e
                );
                MeowError::from_code(
                    InnerErrorCode::CommandSendFailed,
                    format!("enqueue_failed: {}", e),
                )
            })?;
        crate::meow_flow_log!("executor_api", "enqueue send ok: task_id={:?}", id);
        Ok(id)
    }

    pub(crate) async fn pause(&self, task_id: TaskId) -> Result<(), MeowError> {
        crate::meow_flow_log!("executor_api", "pause send: task_id={:?}", task_id);
        // 为 pause 建立一次性应答通道，确保可以拿到“是否找到任务”的明确结果。
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(TransferCmd::Pause {
                task_id,
                respond_to: tx,
            })
            .await
            .map_err(|e| {
                crate::meow_flow_log!(
                    "executor_api",
                    "pause send failed: task_id={:?} err={}",
                    task_id,
                    e
                );
                MeowError::from_code(
                    InnerErrorCode::CommandSendFailed,
                    format!("pause_failed: {}", e),
                )
            })?;
        crate::meow_flow_log!("executor_api", "pause send ok: task_id={:?}", task_id);
        rx.await.map_err(|e| {
            crate::meow_flow_log!(
                "executor_api",
                "pause response await failed: task_id={:?} err={}",
                task_id,
                e
            );
            MeowError::from_code(
                InnerErrorCode::CommandResponseFailed,
                format!("pause response failed: {}", e),
            )
        })?
    }

    pub(crate) async fn resume(&self, task_id: TaskId) -> Result<(), MeowError> {
        crate::meow_flow_log!("executor_api", "resume send: task_id={:?}", task_id);
        // 为 resume 建立一次性应答通道，避免“命令已发送但无结果”的静默行为。
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(TransferCmd::Resume {
                task_id,
                respond_to: tx,
            })
            .await
            .map_err(|e| {
                crate::meow_flow_log!(
                    "executor_api",
                    "resume send failed: task_id={:?} err={}",
                    task_id,
                    e
                );
                MeowError::from_code(
                    InnerErrorCode::CommandSendFailed,
                    format!("resume_failed: {}", e),
                )
            })?;
        crate::meow_flow_log!("executor_api", "resume send ok: task_id={:?}", task_id);
        rx.await.map_err(|e| {
            crate::meow_flow_log!(
                "executor_api",
                "resume response await failed: task_id={:?} err={}",
                task_id,
                e
            );
            MeowError::from_code(
                InnerErrorCode::CommandResponseFailed,
                format!("resume response failed: {}", e),
            )
        })?
    }

    pub(crate) async fn cancel(&self, task_id: TaskId) -> Result<(), MeowError> {
        crate::meow_flow_log!("executor_api", "cancel send: task_id={:?}", task_id);
        // 为 cancel 建立一次性应答通道，确保未知 task_id 能返回明确错误。
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(TransferCmd::Cancel {
                task_id,
                respond_to: tx,
            })
            .await
            .map_err(|e| {
                crate::meow_flow_log!(
                    "executor_api",
                    "cancel send failed: task_id={:?} err={}",
                    task_id,
                    e
                );
                MeowError::from_code(
                    InnerErrorCode::CommandSendFailed,
                    format!("cancel_failed: {}", e),
                )
            })?;
        crate::meow_flow_log!("executor_api", "cancel send ok: task_id={:?}", task_id);
        rx.await.map_err(|e| {
            crate::meow_flow_log!(
                "executor_api",
                "cancel response await failed: task_id={:?} err={}",
                task_id,
                e
            );
            MeowError::from_code(
                InnerErrorCode::CommandResponseFailed,
                format!("cancel response failed: {}", e),
            )
        })?
    }

    pub(crate) async fn snapshot(&self) -> Result<TransferSnapshot, MeowError> {
        crate::meow_flow_log!("executor_api", "snapshot send");
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(TransferCmd::Snapshot { respond_to: tx })
            .await
            .map_err(|e| {
                crate::meow_flow_log!("executor_api", "snapshot send failed: err={}", e);
                MeowError::from_code(
                    InnerErrorCode::CommandSendFailed,
                    format!("snapshot cmd_tx send failed: {}", e),
                )
            })?;
        rx.await.map_err(|e| {
            crate::meow_flow_log!("executor_api", "snapshot response await failed: err={}", e);
            MeowError::from_code(
                InnerErrorCode::CommandResponseFailed,
                format!("snapshot rx.await failed: {}", e),
            )
        })
    }

    pub(crate) async fn close(&self) -> Result<(), MeowError> {
        crate::meow_flow_log!("executor_api", "close send");
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(TransferCmd::Close { respond_to: tx })
            .await
            .map_err(|e| {
                crate::meow_flow_log!("executor_api", "close send failed: err={}", e);
                MeowError::from_code(
                    InnerErrorCode::CommandSendFailed,
                    format!("close_failed: {}", e),
                )
            })?;
        rx.await.map_err(|e| {
            MeowError::from_code(
                InnerErrorCode::CommandResponseFailed,
                format!("close response failed: {}", e),
            )
        })?
    }
}

fn to_record_inner(
    inner: &InnerTask,
    status: TransferStatus,
    transferred: u64,
    file_size_u64: u64,
) -> FileTransferRecord {
    let progress = if file_size_u64 == 0 {
        0.0
    } else {
        transferred as f32 / file_size_u64 as f32
    };
    FileTransferRecord::new(
        inner.task_id(),
        inner.file_sign().to_string(),
        inner.file_name().to_string(),
        file_size_u64,
        progress,
        status,
        inner.direction(),
    )
}
