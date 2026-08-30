use crate::error::{InnerErrorCode, MeowError};
use crate::file_transfer_record::FileTransferRecord;
use crate::ids::TaskId;
use crate::inner::cb_dispatcher;
use crate::inner::group_state::{GroupState, RecordEntry};
use crate::inner::inner_task::{InnerTask, StopRequestDisposition};
use crate::inner::scheduler_state::SchedulerState;
use crate::inner::task_callbacks::TaskCallbacks;
use crate::inner::worker_event::WorkerEvent;
use crate::inner::UniqueId;
use crate::meow_config::MeowConfig;
use crate::transfer_executor_trait::TransferTrait;
use crate::transfer_snapshot::TransferSnapshot;
use crate::transfer_status::TransferStatus;
use crate::transfer_task::TransferTask;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use tokio::sync::mpsc;

pub(crate) enum TransferCmd {
    Enqueue {
        inner: InnerTask,
        callbacks: TaskCallbacks,
    },
    /// Imports a task in the "paused" state: registered into the scheduler but
    /// not queued and triggering no I/O. A later Resume on the same task_id
    /// moves it into the normal scheduling lifecycle.
    EnqueuePaused {
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

/// Handles a worker event after enforcing protocol cleanup that must happen at
/// the worker-quiescence boundary. Active cancel never calls provider abort in
/// the command path: doing so could race an in-flight final completion request.
async fn handle_quiescent_worker_event(
    event: WorkerEvent,
    state: &mut SchedulerState,
    executor: &Arc<dyn TransferTrait>,
) {
    let terminal_key = match &event {
        WorkerEvent::Progress { .. } => None,
        WorkerEvent::Completed { key, .. }
        | WorkerEvent::Failed { key, .. }
        | WorkerEvent::Canceled { key, .. } => Some(key.clone()),
    };
    // WorkerEvent is the part-drain acknowledgement. Explicitly close the
    // shared upload descriptor before protocol cleanup or any user callback;
    // the run_group RAII guard may not have dropped yet because channel send
    // and scheduler receive can overlap.
    if let Some(key) = terminal_key.as_ref() {
        if let Some(snapshot) = state
            .groups()
            .get(key)
            .and_then(|group| group.leader_inner().upload_file_snapshot())
        {
            snapshot.release_handle();
        }
    }
    let abort_key = match &event {
        WorkerEvent::Canceled { key, .. } | WorkerEvent::Failed { key, .. }
            if key.0 == crate::direction::Direction::Upload
                && state.canceling_set().contains(key) =>
        {
            Some(key.clone())
        }
        _ => None,
    };
    if let Some(key) = abort_key {
        if let Some(group) = state.groups().get(&key) {
            let task = TransferTask::from_inner(group.leader_inner());
            if let Err(err) = executor.cancel(&task).await {
                crate::meow_warn_log!(
                    "cancel_group",
                    "protocol abort failed after worker quiesced: key={} err={}",
                    crate::inner::safe_key(&key),
                    crate::log::redact_secrets(&err.to_string())
                );
            }
        }
    }
    crate::inner::exec_impl::handle_worker_event::handle_worker_event(event, state).await;
}

fn worker_loop(
    mut cmd_rx: mpsc::Receiver<TransferCmd>,
    mut worker_rx: mpsc::Receiver<WorkerEvent>,
    worker_tx: mpsc::Sender<WorkerEvent>,
    mut state: SchedulerState,
    executor: Arc<dyn TransferTrait>,
    mut cb_join: cb_dispatcher::CbDispatcherJoin,
) -> Result<JoinHandle<()>, MeowError> {
    fn shutdown_callback_dispatcher(
        state: &mut SchedulerState,
        cb_join: &mut cb_dispatcher::CbDispatcherJoin,
    ) {
        // Drop the scheduler's only callback sender before joining. This is
        // required on every worker exit path; otherwise the dispatcher can
        // wait forever for more jobs while the worker waits in join().
        drop(state.take_cb_submit());
        cb_join.join();
    }

    crate::meow_key_log!("worker_loop", "starting scheduler worker thread");
    let (startup_tx, startup_rx) = std::sync::mpsc::sync_channel::<Result<(), MeowError>>(1);
    let handle = std::thread::spawn(move || {
        let runtime_ret = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build();
        match runtime_ret {
            Ok(runtime) => {
                crate::meow_key_log!("worker_loop", "runtime created successfully");
                let _ = startup_tx.send(Ok(()));
                runtime.block_on(async move {
                    loop {
                        tokio::select! {
                            biased;
                            maybe_cmd = cmd_rx.recv() => {
                                let Some(cmd) = maybe_cmd else {
                                    shutdown_callback_dispatcher(&mut state, &mut cb_join);
                                    break;
                                };
                                match cmd {
                                    TransferCmd::Enqueue { inner, callbacks } => {
                                        let key = inner.dedupe_key();
                                        crate::meow_key_log!(
                                            "cmd_enqueue",
                                            "received enqueue: task_id={:?} key={} chunk_size={}",
                                            inner.task_id(),
                                            crate::inner::safe_key(&key),
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
                                                    &state,
                                                    cb,
                                                    dup_dto.clone(),
                                                );
                                            }
                                            crate::inner::exec_impl::emit::emit_global_progress(
                                                &state,
                                                dup_dto,
                                            );
                                            crate::meow_warn_log!(
                                                "cmd_enqueue",
                                                "duplicate key rejected: key={}",
                                                crate::inner::safe_key(&key)
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
                                            let total = crate::inner::exec_impl::emit::effective_total(
                                                &state,
                                                &key,
                                                group.entry().inner(),
                                            );
                                            crate::inner::exec_impl::emit::emit_status(
                                                &state,
                                                group.entry(),
                                                TransferStatus::Pending,
                                                current,
                                                total,
                                            );
                                            if should_send_running {
                                                crate::inner::exec_impl::emit::emit_status(
                                                    &state,
                                                    group.entry(),
                                                    TransferStatus::Transmission,
                                                    current,
                                                    total,
                                                );
                                            }
                                        }

                                        if !state.active().contains_key(&key) && !state.queued_set().contains(&key) {
                                            state.queued_mut().push_back(key.clone());
                                            state.queued_set_mut().insert(key.clone());
                                            crate::meow_key_log!(
                                                "cmd_enqueue",
                                                "queued new key: key={} queued_len={}",
                                                crate::inner::safe_key(&key),
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
                                    TransferCmd::EnqueuePaused { inner, callbacks } => {
                                        let key = inner.dedupe_key();
                                        crate::meow_key_log!(
                                            "cmd_enqueue_paused",
                                            "received enqueue_paused: task_id={:?} key={} chunk_size={}",
                                            inner.task_id(),
                                            crate::inner::safe_key(&key),
                                            inner.chunk_size()
                                        );
                                        // Same duplicate detection as Enqueue so repeated imports stay idempotent.
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
                                                    &state,
                                                    cb,
                                                    dup_dto.clone(),
                                                );
                                            }
                                            crate::inner::exec_impl::emit::emit_global_progress(
                                                &state,
                                                dup_dto,
                                            );
                                            crate::meow_warn_log!(
                                                "cmd_enqueue_paused",
                                                "duplicate key rejected: key={}",
                                                crate::inner::safe_key(&key)
                                            );
                                            continue;
                                        }

                                        state
                                            .task_id_to_dedupe_mut()
                                            .insert(inner.task_id(), key.clone());

                                        let entry = RecordEntry::new(inner.clone(), callbacks);
                                        state.groups_mut().insert(
                                            key.clone(),
                                            GroupState::new(inner.clone(), entry),
                                        );

                                        // Register directly into paused_set: no queueing and no
                                        // try_start_next, so an imported task performs zero network
                                        // or file I/O until it is explicitly resumed.
                                        state.paused_set_mut().insert(key.clone());
                                        if let Some(group) = state.groups().get(&key) {
                                            // offset has not been computed by prepare yet, so this
                                            // reports 0; callers should render paused-list progress
                                            // from their own persisted record.
                                            let current =
                                                state.offsets().get(&key).copied().unwrap_or(0);
                                            let total = crate::inner::exec_impl::emit::effective_total(
                                                &state,
                                                &key,
                                                group.entry().inner(),
                                            );
                                            crate::inner::exec_impl::emit::emit_status(
                                                &state,
                                                group.entry(),
                                                TransferStatus::Paused,
                                                current,
                                                total,
                                            );
                                        }
                                        crate::meow_key_log!(
                                            "cmd_enqueue_paused",
                                            "task registered as paused: key={}",
                                            crate::inner::safe_key(&key)
                                        );
                                    }
                                    TransferCmd::Pause { task_id, respond_to } => {
                                        crate::meow_key_log!(
                                            "cmd_pause",
                                            "pause requested: task_id={:?}",
                                            task_id
                                        );
                                        if let Some(key) = state.task_id_to_dedupe().get(&task_id).cloned()
                                        {
                                            let pause_ret = pause_group(&mut state, &key).await;
                                            if pause_ret.is_ok() {
                                                crate::meow_key_log!(
                                                    "cmd_pause",
                                                    "pause accepted: task_id={:?} key={}",
                                                    task_id,
                                                    crate::inner::safe_key(&key)
                                                );
                                            }
                                            let _ = respond_to.send(pause_ret);
                                        } else {
                                            crate::meow_warn_log!(
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
                                        crate::meow_key_log!(
                                            "cmd_resume",
                                            "resume requested: task_id={:?}",
                                            task_id
                                        );
                                        if let Some(key) = state.task_id_to_dedupe().get(&task_id).cloned()
                                        {
                                            let resume_ret = resume_group(&mut state, &key).await;
                                            if let Err(e) = &resume_ret {
                                                crate::meow_warn_log!(
                                                    "cmd_resume",
                                                    "resume rejected: task_id={:?} key={} err={}",
                                                    task_id,
                                                    crate::inner::safe_key(&key),
                                                    e
                                                );
                                            } else {
                                                crate::meow_key_log!(
                                                    "cmd_resume",
                                                    "resume accepted: task_id={:?} key={}",
                                                    task_id,
                                                    crate::inner::safe_key(&key)
                                                );
                                            }
                                            let _ = respond_to.send(resume_ret);
                                        } else {
                                            crate::meow_warn_log!(
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
                                        crate::meow_key_log!(
                                            "cmd_cancel",
                                            "cancel requested: task_id={:?}",
                                            task_id
                                        );
                                        if let Some(key) = state.task_id_to_dedupe().get(&task_id).cloned()
                                        {
                                            let cancel_ret =
                                                cancel_group(&mut state, &key, &executor).await;
                                            if cancel_ret.is_ok() {
                                                crate::meow_key_log!(
                                                    "cmd_cancel",
                                                    "cancel accepted: task_id={:?} key={}",
                                                    task_id,
                                                    crate::inner::safe_key(&key)
                                                );
                                            }
                                            let _ = respond_to.send(cancel_ret);
                                        } else {
                                            crate::meow_warn_log!(
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
                                        crate::meow_key_log!(
                                            "cmd_close",
                                            "close requested: active={} groups={} queued={} paused={}",
                                            state.active().len(),
                                            state.groups().len(),
                                            state.queued().len(),
                                            state.paused_set().len()
                                        );
                                        let active_keys: Vec<_> =
                                            state.active().keys().cloned().collect();
                                        // Close is a resumable stop, not an upload abort. Claim the
                                        // pause boundary before canceling each worker. A task that
                                        // already claimed Completing is allowed to finish accurately.
                                        for key in &active_keys {
                                            if state.canceling_set().contains(key) {
                                                if let Some(active) = state.active().get(key) {
                                                    active.cancel().cancel();
                                                }
                                                continue;
                                            }
                                            let disposition = state
                                                .groups()
                                                .get(key)
                                                .map(|group| {
                                                    group.leader_inner().begin_terminal_pause()
                                                })
                                                .unwrap_or(
                                                    StopRequestDisposition::InterruptNow,
                                                );
                                            state.paused_set_mut().insert(key.clone());
                                            if disposition
                                                == StopRequestDisposition::InterruptNow
                                            {
                                                if let Some(active) = state.active().get(key) {
                                                    active.cancel().cancel();
                                                }
                                            }
                                        }

                                        // Groups with no worker own no live target handle, so their
                                        // Paused event is immediately safe. Active groups emit only
                                        // from their quiescent Canceled acknowledgement below.
                                        let inactive_keys: Vec<_> = state
                                            .groups()
                                            .keys()
                                            .filter(|key| !state.active().contains_key(*key))
                                            .cloned()
                                            .collect();
                                        for key in inactive_keys {
                                            if let Some(group) = state.groups().get(&key) {
                                                let current =
                                                    state.offsets().get(&key).copied().unwrap_or(0);
                                                let total = crate::inner::exec_impl::emit::effective_total(
                                                    &state,
                                                    &key,
                                                    group.entry().inner(),
                                                );
                                                crate::inner::exec_impl::emit::emit_status(
                                                    &state,
                                                    group.entry(),
                                                    TransferStatus::Paused,
                                                    current,
                                                    total,
                                                );
                                            }
                                        }

                                        // Keep consuming the bounded worker channel while closing;
                                        // otherwise workers could block trying to acknowledge cleanup
                                        // and close would deadlock. No new scheduler work is started.
                                        while !state.active().is_empty() {
                                            let Some(event) = worker_rx.recv().await else {
                                                crate::meow_error_log!(
                                                    "cmd_close",
                                                    "worker event channel closed with {} active groups",
                                                    state.active().len()
                                                );
                                                break;
                                            };
                                            handle_quiescent_worker_event(
                                                event,
                                                &mut state,
                                                &executor,
                                            )
                                            .await;
                                        }

                                        state.clear_active();
                                        state.groups_mut().clear();
                                        state.task_id_to_dedupe_mut().clear();
                                        state.queued_mut().clear();
                                        state.queued_set_mut().clear();
                                        state.paused_set_mut().clear();
                                        state.canceling_set_mut().clear();
                                        state.offsets_mut().clear();
                                        state.known_totals_mut().clear();
                                        // 关闭回调 channel：drop 唯一持有的 sender。
                                        // 之后阻塞 join 分发线程，确保所有终态回调（包括上面刚 emit
                                        // 的 Paused）在 close().await 返回前已经被回放完毕。
                                        shutdown_callback_dispatcher(&mut state, &mut cb_join);
                                        crate::meow_key_log!("cmd_close", "close finished, worker loop exiting");
                                        let _ = respond_to.send(Ok(()));
                                        break;
                                    }
                                }
                            }
                            maybe_event = worker_rx.recv() => {
                                let Some(event) = maybe_event else { continue; };
                                crate::meow_trace_log!("worker_loop", "worker event received");
                                let should_try_start_next =
                                    event.may_change_scheduler_readiness();
                                handle_quiescent_worker_event(event, &mut state, &executor).await;
                                if should_try_start_next {
                                    let _ = crate::inner::exec_impl::exec::try_start_next(
                                        &worker_tx,
                                        &mut state,
                                        &executor,
                                    )
                                    .await;
                                }
                            }
                        }
                    }
                });
            }
            Err(e) => {
                crate::meow_error_log!("worker_loop", "runtime creation failed: {}", e);
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
    })??;
    Ok(handle)
}
async fn pause_group(state: &mut SchedulerState, key: &UniqueId) -> Result<(), MeowError> {
    crate::meow_flow_log!(
        "pause_group",
        "pause begin: key={} active={} queued={} paused={}",
        crate::inner::safe_key(key),
        state.active().contains_key(key),
        state.queued_set().contains(key),
        state.paused_set().contains(key)
    );
    // 暂停语义要求“可恢复”，因此先从可运行集合移除，避免继续调度/执行。
    let was_active = state.active().contains_key(key);
    let pause_disposition = if was_active {
        state
            .groups()
            .get(key)
            .map(|group| group.leader_inner().begin_terminal_pause())
            .unwrap_or(StopRequestDisposition::InterruptNow)
    } else {
        StopRequestDisposition::InterruptNow
    };
    if pause_disposition == StopRequestDisposition::InterruptNow {
        if let Some(active) = state.active().get(key) {
            // 对正在执行的组发出取消信号，让 worker 退出循环并回到可恢复状态。
            // 注意：这里不立刻从 active 移除，避免 resume 与 Canceled 事件发生竞态。
            active.cancel().cancel();
        }
    }
    // 若任务尚在等待队列中，暂停后应立刻从队列剔除。
    state.queued_mut().retain(|k| k != key);
    // 同步更新队列镜像集合，确保状态一致。
    state.queued_set_mut().remove(key);
    // 关键：标记为 paused，而不是删除 group/mapping，这样 resume 才能找到原任务。
    state.paused_set_mut().insert(key.clone());

    if !was_active {
        if let Some(group) = state.groups().get(key) {
            // 发送 Paused 事件给回调层，告诉调用方任务已进入可恢复暂停态。
            let entry = group.entry();
            // 使用当前 offset 作为暂停进度，保持对外可观测进度连续。
            let current = state.offsets().get(key).copied().unwrap_or(0);
            let total = crate::inner::exec_impl::emit::effective_total(state, key, entry.inner());
            crate::inner::exec_impl::emit::emit_status(
                state,
                entry,
                TransferStatus::Paused,
                current,
                total,
            );
            crate::meow_flow_log!(
                "pause_group",
                "pause status emitted: key={} offset={}",
                crate::inner::safe_key(key),
                current
            );
        }
    }
    Ok(())
}

async fn resume_group(state: &mut SchedulerState, key: &UniqueId) -> Result<(), MeowError> {
    crate::meow_flow_log!(
        "resume_group",
        "resume begin: key={} active={} queued={} paused={}",
        crate::inner::safe_key(key),
        state.active().contains_key(key),
        state.queued_set().contains(key),
        state.paused_set().contains(key)
    );
    // 非 paused 任务不允许 resume，防止错误状态转换。
    if !state.paused_set().contains(key) {
        crate::meow_warn_log!(
            "resume_group",
            "resume rejected not paused: key={}",
            crate::inner::safe_key(key)
        );
        return Err(MeowError::from_code(
            InnerErrorCode::InvalidTaskState,
            "resume target is not paused".to_string(),
        ));
    }
    // 仍在 active 说明 pause 正在收敛中，先拒绝本次 resume，避免和取消事件竞态。
    if state.active().contains_key(key) {
        crate::meow_warn_log!(
            "resume_group",
            "resume rejected still stopping: key={}",
            crate::inner::safe_key(key)
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
        crate::meow_warn_log!(
            "resume_group",
            "resume rejected missing group: key={}",
            crate::inner::safe_key(key)
        );
        return Err(MeowError::from_code(
            InnerErrorCode::InvalidTaskState,
            "resume target group missing".to_string(),
        ));
    };
    // 当前 offset 继续作为恢复起点，通知外部进入 Pending（待调度）状态。
    let current = state.offsets().get(key).copied().unwrap_or(0);
    let total = crate::inner::exec_impl::emit::effective_total(state, key, group.entry().inner());
    crate::inner::exec_impl::emit::emit_status(
        state,
        group.entry(),
        TransferStatus::Pending,
        current,
        total,
    );
    // 仅当不在 active 且不在 queued 时重新入队，避免重复排队。
    if !state.active().contains_key(key) && !state.queued_set().contains(key) {
        state.queued_mut().push_back(key.clone());
        state.queued_set_mut().insert(key.clone());
        crate::meow_flow_log!(
            "resume_group",
            "resume requeued key={} queued_len={}",
            crate::inner::safe_key(key),
            state.queued().len()
        );
    }
    crate::meow_flow_log!(
        "resume_group",
        "resume success: key={}",
        crate::inner::safe_key(key)
    );
    Ok(())
}

async fn cancel_group(
    state: &mut SchedulerState,
    key: &UniqueId,
    executor: &Arc<dyn TransferTrait>,
) -> Result<(), MeowError> {
    crate::meow_flow_log!(
        "cancel_group",
        "cancel begin: key={} active={} queued={} paused={}",
        crate::inner::safe_key(key),
        state.active().contains_key(key),
        state.queued_set().contains(key),
        state.paused_set().contains(key)
    );
    let was_active = state.active().contains_key(key);
    if was_active {
        // Upload completion and cancel have one atomic linearization point. If
        // completion already claimed the task, reject this late cancel instead
        // of reporting it accepted and then silently completing/aborting.
        let cancel_disposition = state
            .groups()
            .get(key)
            .map(|group| group.leader_inner().begin_terminal_cancel())
            .unwrap_or(StopRequestDisposition::InterruptNow);
        if cancel_disposition == StopRequestDisposition::InterruptNow {
            if let Some(active) = state.active().get(key) {
                active.cancel().cancel();
            }
        }
    } else {
        // No worker exists, so the cancel transition can be completed
        // synchronously below.
    }
    // Active state stays registered until the worker's quiescence event. This
    // makes the user-visible terminal callback happen strictly after download
    // checkpoint/file-lock/path-lease cleanup and after all part tasks drain.
    // 取消后不应继续排队。
    state.queued_mut().retain(|k| k != key);
    state.queued_set_mut().remove(key);
    // 取消语义会彻底结束任务，因此需要清掉 paused 标记。
    state.paused_set_mut().remove(key);
    if was_active {
        state.canceling_set_mut().insert(key.clone());
        if let Some(task_id) = state
            .groups()
            .get(key)
            .map(|group| group.leader_inner().task_id())
        {
            // Invalidate further controls immediately while retaining the group
            // itself for worker-event cleanup and terminal callback delivery.
            state.task_id_to_dedupe_mut().remove(&task_id);
        }
        return Ok(());
    }
    if let Some(group) = state.groups_mut().remove(key) {
        let _ = group.leader_inner().begin_terminal_cancel();
        // 对上传协议触发可选的远端取消语义（例如 OSS AbortMultipartUpload）。
        let task_view = TransferTask::from_inner(group.leader_inner());
        if let Err(err) = executor.cancel(&task_view).await {
            // Abort/cleanup failure is billing-relevant: orphaned multipart parts
            // or uncommitted blocks may keep accruing storage cost. Surface it at
            // WARN (not debug) so consumers filtering out debug noise still see it.
            // The error message carries the provider session id (e.g. OSS uploadId)
            // for out-of-band cleanup. We continue local teardown regardless.
            crate::meow_warn_log!(
                "cancel_group",
                "protocol abort failed but continue cleanup: key={} err={}",
                crate::inner::safe_key(key),
                crate::log::redact_secrets(&err.to_string())
            );
        }
        // 取消后删除 task_id 映射，防止继续通过旧 id 控制。
        state
            .task_id_to_dedupe_mut()
            .remove(&group.leader_inner().task_id());
        let entry = group.entry();
        let current = state.offsets().get(key).copied().unwrap_or(0);
        let total = crate::inner::exec_impl::emit::effective_total(state, key, entry.inner());
        crate::inner::exec_impl::emit::emit_status(
            state,
            entry,
            TransferStatus::Canceled,
            current,
            total,
        );
        crate::meow_flow_log!(
            "cancel_group",
            "cancel status emitted: key={} offset={}",
            crate::inner::safe_key(key),
            current
        );
        // 控制面取消自成闭环：同步清理断点/总大小记录，不再依赖一个可能永远
        // 不会到达的 worker Canceled 事件（对已 paused 的任务，run_group 早已退出，
        // 没有 worker 再发终态事件）。否则 offsets/known_totals 会残留成孤儿，
        // 并被后续复用同一去重键(URL)的新任务读到陈旧值。
        state.offsets_mut().remove(key);
        state.known_totals_mut().remove(key);
    }
    state.canceling_set_mut().remove(key);
    Ok(())
}

fn task_not_found_error(task_id: TaskId) -> MeowError {
    MeowError::from_code(
        InnerErrorCode::TaskNotFound,
        format!("task not found: {:?}", task_id),
    )
}

/// Handle to the background scheduler worker.
///
/// The worker itself lives on a dedicated [`std::thread`] which in turn
/// drives a dedicated Tokio multi-thread runtime. The join handle is retained
/// so explicit [`Executor::close`] can wait until that thread has fully
/// exited. The worker only shuts down when one of the following happens:
///
/// 1. An explicit [`Executor::close`] command is processed and the loop
///    breaks cleanly (preferred path, drains state and emits `Paused`
///    events).
/// 2. All [`TransferCmd`] senders are dropped, in which case `cmd_rx.recv()`
///    returns `None` and the loop breaks naturally (fallback path).
///
/// Because path (1) is the only way to guarantee clean shutdown, deliver
/// terminal status events to user callbacks, drain the callback dispatcher,
/// and join the scheduler thread, `close()` is treated as a **required** part
/// of the lifecycle. The [`Drop`] impl below performs a best-effort shutdown
/// (plus a warning log) for the case where users forget it, but must not be
/// relied upon for correctness.
#[derive(Debug)]
pub(crate) struct Executor {
    cmd_tx: tokio::sync::mpsc::Sender<TransferCmd>,
    /// Scheduler thread join handle. Explicit `close()` takes and joins it so
    /// close becomes a hard resource-release barrier.
    worker_join: Mutex<Option<JoinHandle<()>>>,
    /// Set to `true` by [`Executor::close`] after the worker acknowledges
    /// shutdown. Read by [`Drop`] to decide whether a best-effort shutdown
    /// is still required.
    close_invoked: AtomicBool,
}

impl Executor {
    pub(crate) fn new(
        config: MeowConfig,
        executor: Arc<dyn TransferTrait>,
        global_progress_listener: crate::inner::scheduler_state::GlobalProgressStore,
        callback_dispatcher_owner: cb_dispatcher::CallbackDispatcherOwner,
    ) -> Result<Self, MeowError> {
        crate::meow_key_log!(
            "executor",
            "executor new: max_upload={} max_download={}",
            config.max_upload_concurrency(),
            config.max_download_concurrency()
        );
        let command_queue_capacity = config.command_queue_capacity();
        let worker_event_queue_capacity = config.worker_event_queue_capacity();
        let (cmd_tx, cmd_rx) = mpsc::channel::<TransferCmd>(command_queue_capacity);
        let (worker_tx, worker_rx) = mpsc::channel::<WorkerEvent>(worker_event_queue_capacity);
        // 在调度循环启动前就把回调分发线程拉起来，并把 sender 注入 SchedulerState。
        // 这样 worker_loop 一开始就处于"回调物理隔离"的状态，第一条进度也走分发线程。
        // 队列容量在 cb_dispatcher 内部锁定（CALLBACK_QUEUE_CAPACITY），不对外暴露。
        let (cb_submit, cb_join) = cb_dispatcher::start_for(callback_dispatcher_owner)?;
        let worker_join = worker_loop(
            cmd_rx,
            worker_rx,
            worker_tx,
            SchedulerState::new(
                config.max_upload_concurrency(),
                config.max_download_concurrency(),
                global_progress_listener,
                cb_submit,
            ),
            executor,
            cb_join,
        )?;
        crate::meow_key_log!("executor", "executor worker started");
        Ok(Self {
            cmd_tx,
            worker_join: Mutex::new(Some(worker_join)),
            close_invoked: AtomicBool::new(false),
        })
    }
}

impl Drop for Executor {
    /// Best-effort shutdown path for when the user forgot to call
    /// [`crate::meow_client::MeowClient::close`].
    ///
    /// We intentionally keep this path simple and non-blocking:
    ///
    /// - Emits a `Warn`-level log so the misuse is observable.
    /// - Attempts a non-blocking `try_send` of [`TransferCmd::Close`] so the
    ///   worker, if still draining commands, can clean up and emit `Paused`
    ///   events to user callbacks.
    /// - Does **not** `await`, does **not** join the scheduler thread, and
    ///   does **not** attempt to translate in-flight failures: even if the
    ///   `try_send` fails (channel full or worker already gone), dropping
    ///   `cmd_tx` immediately afterwards closes the command channel, which
    ///   makes `cmd_rx.recv()` return `None` and lets the worker loop exit
    ///   on its own.
    ///
    /// This is an aid, not a substitute: callers must still invoke
    /// `close().await` for deterministic shutdown semantics (final status
    /// callbacks, flushed resources, etc.).
    fn drop(&mut self) {
        if self.close_invoked.load(Ordering::SeqCst) {
            crate::meow_flow_log!(
                "executor_drop",
                "executor dropped after explicit close() -- nothing to do"
            );
            return;
        }
        // Surface a Warn-level entry via the debug log listener (if any)
        // so embedding applications can detect the misuse pattern.
        crate::log::emit(crate::log::Log::new(
            crate::log::LogLevel::Warn,
            "executor_drop",
            "MeowClient dropped without calling close().await; performing \
             best-effort shutdown. The background scheduler thread will exit \
             on its own, but terminal status events may be skipped. Always \
             call MeowClient::close().await for deterministic shutdown.",
        ));
        let (tx, _rx) = tokio::sync::oneshot::channel();
        match self.cmd_tx.try_send(TransferCmd::Close { respond_to: tx }) {
            Ok(()) => {
                crate::meow_flow_log!(
                    "executor_drop",
                    "best-effort Close command queued during Drop"
                );
            }
            Err(e) => {
                crate::meow_warn_log!(
                    "executor_drop",
                    "best-effort Close try_send skipped: {}; relying on \
                     cmd_tx drop to unblock worker loop",
                    e
                );
            }
        }
        // `cmd_tx` is dropped right after this returns. That closes the
        // command channel: if the Close command above never reached the
        // worker, the naked `recv() -> None` branch in `worker_loop` still
        // breaks the loop and tears the runtime down.
    }
}

impl Executor {
    async fn join_worker_thread(&self) -> Result<(), MeowError> {
        let handle = {
            let mut guard = self.worker_join.lock().map_err(|e| {
                MeowError::from_code(
                    InnerErrorCode::LockPoisoned,
                    format!("worker join handle lock poisoned: {}", e),
                )
            })?;
            guard.take()
        };
        let Some(handle) = handle else {
            return Ok(());
        };
        tokio::task::spawn_blocking(move || handle.join())
            .await
            .map_err(|e| {
                crate::meow_error_log!(
                    "executor_close",
                    "join_worker_thread join task failed: err={}",
                    e
                );
                MeowError::from_code(
                    InnerErrorCode::Unknown,
                    format!("worker thread join task failed: {}", e),
                )
            })?
            .map_err(|_| {
                crate::meow_error_log!("executor_close", "worker thread panicked during shutdown");
                MeowError::from_code_str(
                    InnerErrorCode::Unknown,
                    "worker thread panicked during shutdown",
                )
            })
    }

    /// Non-blocking command submission for an `Enqueue` request.
    ///
    /// This deliberately uses [`tokio::sync::mpsc::Sender::try_send`] rather
    /// than `send().await`. The caller gets an immediate error if the command
    /// channel is full (back-pressure signal) instead of being silently
    /// suspended until the worker drains enough slots.
    ///
    /// The public-facing entry point [`crate::meow_client::MeowClient::try_enqueue`]
    /// reflects this by carrying the `try_` prefix in its name; do not change
    /// this into an `await`ing `send` without renaming the public API as
    /// well.
    pub(crate) fn try_enqueue(
        &self,
        inner: InnerTask,
        callbacks: TaskCallbacks,
    ) -> Result<TaskId, MeowError> {
        let id = inner.task_id();
        crate::meow_flow_log!(
            "executor_api",
            "try_enqueue send: task_id={:?} key={}",
            id,
            crate::inner::safe_key(&inner.dedupe_key())
        );
        self.cmd_tx
            .try_send(TransferCmd::Enqueue { inner, callbacks })
            .map_err(|e| {
                crate::meow_warn_log!(
                    "executor_api",
                    "try_enqueue send failed: task_id={:?} err={}",
                    id,
                    e
                );
                MeowError::from_code(
                    InnerErrorCode::CommandSendFailed,
                    format!("try_enqueue_failed: {}", e),
                )
            })?;
        crate::meow_flow_log!("executor_api", "try_enqueue send ok: task_id={:?}", id);
        Ok(id)
    }

    /// Non-blocking submission of an `EnqueuePaused` request.
    ///
    /// Mirrors [`Self::try_enqueue`] (fail-fast `try_send`, `CommandSendFailed`
    /// on a full command queue) but registers the task directly in the paused
    /// set without queueing it. The task only enters the running lifecycle once
    /// the caller issues a matching [`Self::resume`].
    pub(crate) fn try_enqueue_paused(
        &self,
        inner: InnerTask,
        callbacks: TaskCallbacks,
    ) -> Result<TaskId, MeowError> {
        let id = inner.task_id();
        crate::meow_flow_log!(
            "executor_api",
            "try_enqueue_paused send: task_id={:?} key={}",
            id,
            crate::inner::safe_key(&inner.dedupe_key())
        );
        self.cmd_tx
            .try_send(TransferCmd::EnqueuePaused { inner, callbacks })
            .map_err(|e| {
                crate::meow_warn_log!(
                    "executor_api",
                    "try_enqueue_paused send failed: task_id={:?} err={}",
                    id,
                    e
                );
                MeowError::from_code(
                    InnerErrorCode::CommandSendFailed,
                    format!("try_enqueue_paused_failed: {}", e),
                )
            })?;
        crate::meow_flow_log!(
            "executor_api",
            "try_enqueue_paused send ok: task_id={:?}",
            id
        );
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
                crate::meow_warn_log!(
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
            crate::meow_warn_log!(
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
                crate::meow_warn_log!(
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
            crate::meow_warn_log!(
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
                crate::meow_warn_log!(
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
            crate::meow_warn_log!(
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
        crate::meow_trace_log!("executor_api", "snapshot send");
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(TransferCmd::Snapshot { respond_to: tx })
            .await
            .map_err(|e| {
                crate::meow_warn_log!("executor_api", "snapshot send failed: err={}", e);
                MeowError::from_code(
                    InnerErrorCode::CommandSendFailed,
                    format!("snapshot cmd_tx send failed: {}", e),
                )
            })?;
        rx.await.map_err(|e| {
            crate::meow_warn_log!("executor_api", "snapshot response await failed: err={}", e);
            MeowError::from_code(
                InnerErrorCode::CommandResponseFailed,
                format!("snapshot rx.await failed: {}", e),
            )
        })
    }

    pub(crate) async fn close(&self) -> Result<(), MeowError> {
        crate::meow_key_log!("executor_api", "close send");
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(TransferCmd::Close { respond_to: tx })
            .await
            .map_err(|e| {
                crate::meow_warn_log!("executor_api", "close send failed: err={}", e);
                MeowError::from_code(
                    InnerErrorCode::CommandSendFailed,
                    format!("close_failed: {}", e),
                )
            })?;
        let result = rx.await.map_err(|e| {
            crate::meow_warn_log!("executor_api", "close response await failed: err={}", e);
            MeowError::from_code(
                InnerErrorCode::CommandResponseFailed,
                format!("close response failed: {}", e),
            )
        })?;
        // Only mark as fully closed after the worker acknowledges. If the
        // handshake above failed, keep `close_invoked = false` so `Drop`
        // still performs its best-effort shutdown path.
        if result.is_ok() {
            self.join_worker_thread().await?;
            self.close_invoked.store(true, Ordering::SeqCst);
        }
        result
    }

    pub(crate) fn is_close_complete(&self) -> bool {
        self.close_invoked.load(Ordering::SeqCst)
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
        inner.file_sign_arc(),
        inner.file_name_arc(),
        file_size_u64,
        progress,
        status,
        inner.direction(),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;

    use super::{cancel_group, handle_quiescent_worker_event, pause_group, resume_group};
    use crate::chunk_outcome::ChunkOutcome;
    use crate::error::MeowError;
    use crate::inner::test_support::{attach_capture, live_download_state, wait_for_record};
    use crate::prepare_outcome::PrepareOutcome;
    use crate::transfer_executor_trait::TransferTrait;
    use crate::transfer_status::TransferStatus;
    use crate::transfer_task::TransferTask;

    /// 什么都不做的传输执行器：cancel_group 需要一个 `Arc<dyn TransferTrait>`
    /// 来触发（可选的）远端 abort，测试里用默认的 `cancel()=Ok(())` 即可。
    struct NoopTransfer;

    #[async_trait]
    impl TransferTrait for NoopTransfer {
        async fn prepare(
            &self,
            _task: &TransferTask,
            local_offset: u64,
        ) -> Result<PrepareOutcome, MeowError> {
            Ok(PrepareOutcome {
                next_offset: local_offset,
                total_size: 0,
            })
        }

        async fn transfer_chunk(
            &self,
            _task: &TransferTask,
            offset: u64,
            _chunk_size: u64,
            remote_total_size: u64,
        ) -> Result<ChunkOutcome, MeowError> {
            Ok(ChunkOutcome {
                next_offset: offset,
                total_size: remote_total_size,
                done: true,
                completion_payload: None,
            })
        }
    }

    /// pause 中途的下载：Paused 记录必须携带运行期 total 与真实进度
    /// （修复前为 total=0/progress=0，暂停期间消费端 DB 进度被清零）。
    #[tokio::test]
    async fn pause_group_emits_paused_with_runtime_total() {
        let (mut state, key) = live_download_state("pause_total").await;
        let records = attach_capture(&state);
        state.offsets_mut().insert(key.clone(), 512);
        state.known_totals_mut().insert(key.clone(), 4096);

        pause_group(&mut state, &key).await.expect("pause");

        let paused =
            wait_for_record(&records, |r| matches!(r.status(), TransferStatus::Paused)).await;
        assert_eq!(paused.total_size(), 4096);
        assert!((paused.progress() - 0.125).abs() < f32::EPSILON);
    }

    /// resume 发出的 Pending 记录同样携带运行期 total（known_totals 在
    /// pause 期间保留，见 handle_worker_event 的 pause 流测试）。
    #[tokio::test]
    async fn resume_group_emits_pending_with_runtime_total() {
        let (mut state, key) = live_download_state("resume_total").await;
        let records = attach_capture(&state);
        state.offsets_mut().insert(key.clone(), 512);
        state.known_totals_mut().insert(key.clone(), 4096);
        pause_group(&mut state, &key).await.expect("pause");

        resume_group(&mut state, &key).await.expect("resume");

        let pending =
            wait_for_record(&records, |r| matches!(r.status(), TransferStatus::Pending)).await;
        assert_eq!(pending.total_size(), 4096);
    }

    /// 主动 cancel 的终态 Canceled 记录必须携带运行期 total。
    #[tokio::test]
    async fn cancel_group_emits_canceled_with_runtime_total() {
        let (mut state, key) = live_download_state("cancel_total").await;
        let records = attach_capture(&state);
        state.offsets_mut().insert(key.clone(), 512);
        state.known_totals_mut().insert(key.clone(), 4096);
        let executor: Arc<dyn TransferTrait> = Arc::new(NoopTransfer);

        cancel_group(&mut state, &key, &executor)
            .await
            .expect("cancel");

        let canceled =
            wait_for_record(&records, |r| matches!(r.status(), TransferStatus::Canceled)).await;
        assert_eq!(canceled.total_size(), 4096);
        assert!((canceled.progress() - 0.125).abs() < f32::EPSILON);
        assert!(!state.groups().contains_key(&key));
        // finding #1: 控制面取消必须自清理 offsets/known_totals，不留孤儿条目
        // （否则复用同一 URL 的新任务会读到陈旧值）。
        assert!(
            state.offsets().get(&key).is_none(),
            "cancel_group 必须清理 offsets"
        );
        assert!(
            state.known_totals().get(&key).is_none(),
            "cancel_group 必须清理 known_totals"
        );
    }

    /// Active cancel is acknowledged by the API immediately, but its terminal
    /// callback and group teardown wait for the worker's quiescence event.
    #[tokio::test]
    async fn active_cancel_defers_terminal_until_worker_quiesces() {
        use crate::inner::active_state::ActiveState;
        use crate::inner::worker_event::WorkerEvent;
        use tokio_util::sync::CancellationToken;

        let (mut state, key) = live_download_state("active_cancel_quiescence").await;
        let records = attach_capture(&state);
        state.offsets_mut().insert(key.clone(), 512);
        state.known_totals_mut().insert(key.clone(), 4096);
        let token = CancellationToken::new();
        state.insert_active(key.clone(), ActiveState::new(token.clone()));
        let executor: Arc<dyn TransferTrait> = Arc::new(NoopTransfer);

        cancel_group(&mut state, &key, &executor)
            .await
            .expect("active cancel accepted");

        assert!(token.is_cancelled());
        assert!(state.active().contains_key(&key));
        assert!(state.groups().contains_key(&key));
        assert!(state.canceling_set().contains(&key));
        assert!(
            !records
                .lock()
                .expect("records")
                .iter()
                .any(|record| matches!(record.status(), TransferStatus::Canceled)),
            "Canceled must not be visible before the worker cleanup ack"
        );

        handle_quiescent_worker_event(
            WorkerEvent::Canceled {
                key: key.clone(),
                total_size: 4096,
            },
            &mut state,
            &executor,
        )
        .await;

        let canceled =
            wait_for_record(&records, |r| matches!(r.status(), TransferStatus::Canceled)).await;
        assert_eq!(canceled.total_size(), 4096);
        assert!(!state.active().contains_key(&key));
        assert!(!state.groups().contains_key(&key));
        assert!(!state.canceling_set().contains(&key));
    }

    #[tokio::test]
    async fn active_pause_defers_paused_until_worker_quiesces_then_can_resume() {
        use crate::inner::active_state::ActiveState;
        use crate::inner::worker_event::WorkerEvent;
        use tokio_util::sync::CancellationToken;

        let (mut state, key) = live_download_state("active_pause_quiescence").await;
        let records = attach_capture(&state);
        state.offsets_mut().insert(key.clone(), 512);
        state.known_totals_mut().insert(key.clone(), 4096);
        let token = CancellationToken::new();
        state.insert_active(key.clone(), ActiveState::new(token.clone()));

        pause_group(&mut state, &key)
            .await
            .expect("active pause accepted");
        assert!(token.is_cancelled());
        assert!(state.active().contains_key(&key));
        assert!(state.paused_set().contains(&key));
        assert!(
            !records
                .lock()
                .expect("records")
                .iter()
                .any(|record| matches!(record.status(), TransferStatus::Paused)),
            "Paused must not be visible before the worker cleanup ack"
        );

        crate::inner::exec_impl::handle_worker_event::handle_worker_event(
            WorkerEvent::Canceled {
                key: key.clone(),
                total_size: 4096,
            },
            &mut state,
        )
        .await;
        let paused =
            wait_for_record(&records, |r| matches!(r.status(), TransferStatus::Paused)).await;
        assert_eq!(paused.total_size(), 4096);
        assert!(!state.active().contains_key(&key));
        assert!(state.groups().contains_key(&key));

        resume_group(&mut state, &key)
            .await
            .expect("resume after quiescent pause");
        assert!(!state.paused_set().contains(&key));
        assert!(state.queued_set().contains(&key));
    }

    /// Once completion owns the lifecycle gate, a late cancel preserves the
    /// historical Ok response but cannot preempt the linearized Complete.
    #[tokio::test]
    async fn completion_claim_wins_late_cancel_without_canceling_worker() {
        use crate::inner::active_state::ActiveState;
        use crate::inner::worker_event::WorkerEvent;
        use tokio_util::sync::CancellationToken;

        let (mut state, key) = live_download_state("completion_wins").await;
        let records = attach_capture(&state);
        assert!(state
            .groups()
            .get(&key)
            .expect("group")
            .leader_inner()
            .transfer_lifecycle()
            .begin_completion());
        let token = CancellationToken::new();
        state.insert_active(key.clone(), ActiveState::new(token.clone()));
        let executor: Arc<dyn TransferTrait> = Arc::new(NoopTransfer);

        cancel_group(&mut state, &key, &executor)
            .await
            .expect("live task cancel keeps its established Ok response");
        assert!(!token.is_cancelled());
        assert!(state.canceling_set().contains(&key));
        assert!(state.active().contains_key(&key));

        handle_quiescent_worker_event(
            WorkerEvent::Completed {
                key: key.clone(),
                total_size: 1,
                completion_payload: None,
            },
            &mut state,
            &executor,
        )
        .await;
        let complete =
            wait_for_record(&records, |r| matches!(r.status(), TransferStatus::Complete)).await;
        assert_eq!(complete.total_size(), 1);
        assert!(!state.canceling_set().contains(&key));
        assert!(!state.active().contains_key(&key));
    }

    /// finding #8: resume 后 try_start_next 重启下载时，启动 Transmission 必须用
    /// known_totals 里保留的运行期 total（而非被 `Download && total==0` 守卫抑制）。
    /// 直接驱动 try_start_next：它在 spawn worker 前同步 emit 启动 Transmission，
    /// 而 spawn 出的 NoopTransfer worker 只把事件发往 worker_rx（不经回调），
    /// 因此监听器只会收到这条启动 Transmission，断言是确定性的。
    #[tokio::test]
    async fn resumed_download_start_transmission_carries_known_total() {
        use tokio::sync::mpsc;

        let (mut state, key) = live_download_state("restart_total").await;
        let records = attach_capture(&state);
        state.offsets_mut().insert(key.clone(), 512);
        state.known_totals_mut().insert(key.clone(), 4096);
        // 模拟 resume：重新进入待调度队列，且不在 active/paused 中。
        state.queued_mut().push_back(key.clone());
        state.queued_set_mut().insert(key.clone());

        let (worker_tx, _worker_rx) = mpsc::channel(8);
        let executor: Arc<dyn TransferTrait> = Arc::new(NoopTransfer);
        let _ =
            crate::inner::exec_impl::exec::try_start_next(&worker_tx, &mut state, &executor).await;

        let transmission = wait_for_record(&records, |r| {
            matches!(r.status(), TransferStatus::Transmission)
        })
        .await;
        assert_eq!(
            transmission.total_size(),
            4096,
            "resume 重启的 Transmission 必须带运行期 total，而非被 total=0 守卫抑制"
        );
        assert!((transmission.progress() - 0.125).abs() < f32::EPSILON);
    }
}
