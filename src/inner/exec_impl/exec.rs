use crate::direction::Direction;
use crate::error::{InnerErrorCode, MeowError};
use crate::inner::active_state::ActiveState;
use crate::inner::inner_task::InnerTask;
use crate::inner::scheduler_state::SchedulerState;
use crate::inner::worker_event::WorkerEvent;
use crate::inner::UniqueId;
use crate::prepare_outcome::PrepareOutcome;
use crate::transfer_executor_trait::TransferTrait;
use crate::transfer_status::TransferStatus;
use crate::transfer_task::TransferTask;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub(crate) async fn try_start_next(
    worker_tx: &mpsc::Sender<WorkerEvent>,
    state: &mut SchedulerState,
    executor: &Arc<dyn TransferTrait>,
) -> HashSet<UniqueId> {
    crate::meow_flow_log!(
        "scheduler",
        "try_start_next begin: queued={} active={} paused={}",
        state.queued().len(),
        state.active().len(),
        state.paused_set().len()
    );
    let mut started_keys = HashSet::new();
    loop {
        let queued_len = state.queued().len();
        if queued_len == 0 {
            crate::meow_flow_log!("scheduler", "try_start_next exit: queue empty");
            break;
        }

        let mut scheduled_in_this_round = false;
        for _ in 0..queued_len {
            let Some(key) = state.queued_mut().pop_front() else {
                crate::meow_flow_log!("scheduler", "try_start_next pop_front none; break round");
                break;
            };
            state.queued_set_mut().remove(&key);

            if state.active().contains_key(&key) {
                crate::meow_flow_log!("scheduler", "skip key already active: key={:?}", key);
                continue;
            }
            let Some(group) = state.groups().get(&key) else {
                crate::meow_flow_log!("scheduler", "skip key missing group state: key={:?}", key);
                continue;
            };
            let direction = key.0;
            if !can_start_direction(state, direction) {
                crate::meow_flow_log!(
                    "scheduler",
                    "direction concurrency full, requeue key={:?} dir={:?}",
                    key,
                    direction
                );
                state.queued_mut().push_back(key.clone());
                state.queued_set_mut().insert(key);
                continue;
            }

            let inner = group.leader_inner().clone();
            let current = state.offsets().get(&key).copied().unwrap_or(0);
            crate::inner::exec_impl::emit::emit_status(
                state,
                group.entry(),
                TransferStatus::Transmission,
                current,
                group.entry().inner().total_size(),
            );

            let cancel = CancellationToken::new();
            state
                .active_mut()
                .insert(key.clone(), ActiveState::new(cancel.clone()));
            started_keys.insert(key.clone());
            scheduled_in_this_round = true;

            let worker_tx_clone = worker_tx.clone();
            let executor = executor.clone();
            let start_offset = state.offsets().get(&key).copied().unwrap_or(0);
            crate::meow_flow_log!(
                "scheduler",
                "start key={:?} from offset={} chunk_size={}",
                key,
                start_offset,
                inner.chunk_size()
            );
            tokio::spawn(async move {
                let panic_key = key.clone();
                let panic_tx = worker_tx_clone.clone();
                let worker = tokio::spawn(async move {
                    run_group(key, inner, cancel, worker_tx_clone, executor, start_offset).await;
                });
                if let Err(join_err) = worker.await {
                    let err = MeowError::from_code(
                        InnerErrorCode::Unknown,
                        format!("run_group task panicked: {}", join_err),
                    );
                    let _ = panic_tx
                        .send(WorkerEvent::Failed {
                            key: panic_key,
                            error: err,
                        })
                        .await;
                }
            });
        }

        if !scheduled_in_this_round {
            crate::meow_flow_log!(
                "scheduler",
                "try_start_next break: no task scheduled in this round"
            );
            break;
        }
    }

    crate::meow_flow_log!(
        "scheduler",
        "try_start_next end: started_count={}",
        started_keys.len()
    );
    started_keys
}

fn can_start_direction(state: &SchedulerState, direction: Direction) -> bool {
    let active = state
        .active()
        .keys()
        .filter(|(d, _)| *d == direction)
        .count();
    match direction {
        Direction::Upload => active < state.max_upload_concurrency(),
        Direction::Download => active < state.max_download_concurrency(),
    }
}

async fn run_group(
    key: UniqueId,
    inner: InnerTask,
    cancel: CancellationToken,
    worker_tx: mpsc::Sender<WorkerEvent>,
    executor: Arc<dyn TransferTrait>,
    start_offset: u64,
) {
    crate::meow_flow_log!(
        "run_group",
        "run_group begin: key={:?} task_id={:?} start_offset={}",
        key,
        inner.task_id(),
        start_offset
    );
    let task = TransferTask::from_inner(&inner);
    let offset_ret = executor.prepare(&task, start_offset).await;
    let PrepareOutcome {
        next_offset,
        total_size: prep_total,
    } = match offset_ret {
        Ok(v) => v,
        Err(e) => {
            crate::meow_flow_log!(
                "run_group",
                "prepare failed: key={:?} task_id={:?} err={}",
                key,
                inner.task_id(),
                e
            );
            let _ = worker_tx.send(WorkerEvent::Failed { key, error: e }).await;
            return;
        }
    };
    let mut offset = next_offset;
    let mut known_total = if prep_total > 0 {
        prep_total
    } else {
        inner.total_size()
    };
    loop {
        if cancel.is_cancelled() {
            crate::meow_flow_log!(
                "run_group",
                "cancellation observed: key={:?} task_id={:?} offset={}",
                key,
                inner.task_id(),
                offset
            );
            let _ = worker_tx.send(WorkerEvent::Canceled { key }).await;
            return;
        }
        // 分片传输通过独立 retry 模块执行：
        // - 将重试判定、退避计算、取消协作都封装在模块内；
        // - exec.rs 只消费“成功/取消/失败”三态结果，保持主流程清晰且低耦合。
        let outcome = match crate::inner::exec_impl::retry::transfer_chunk_with_retry(
            &executor,
            &task,
            &key,
            &cancel,
            offset,
            inner.chunk_size(),
            known_total,
            inner.max_chunk_retries(),
        )
        .await
        {
            crate::inner::exec_impl::retry::ChunkRetryResult::Done(v) => v,
            crate::inner::exec_impl::retry::ChunkRetryResult::Cancelled => {
                crate::meow_flow_log!(
                    "run_group",
                    "chunk retry interrupted by cancellation: key={:?} task_id={:?} offset={}",
                    key,
                    inner.task_id(),
                    offset
                );
                let _ = worker_tx.send(WorkerEvent::Canceled { key }).await;
                return;
            }
            crate::inner::exec_impl::retry::ChunkRetryResult::Failed(e) => {
                crate::meow_flow_log!(
                    "run_group",
                    "chunk retry exhausted or non-retryable: key={:?} task_id={:?} offset={} err={}",
                    key,
                    inner.task_id(),
                    offset,
                    e
                );
                let _ = worker_tx.send(WorkerEvent::Failed { key, error: e }).await;
                return;
            }
        };
        if outcome.total_size > 0 {
            known_total = outcome.total_size;
        }
        offset = outcome.next_offset;
        let _ = worker_tx
            .send(WorkerEvent::Progress {
                key: key.clone(),
                next_offset: outcome.next_offset,
                total_size: known_total,
            })
            .await;
        if outcome.done {
            crate::meow_flow_log!(
                "run_group",
                "run_group completed: key={:?} task_id={:?} final_offset={} total={}",
                key,
                inner.task_id(),
                offset,
                known_total
            );
            let _ = worker_tx
                .send(WorkerEvent::Completed {
                    key,
                    total_size: known_total,
                })
                .await;
            return;
        }
    }
}
