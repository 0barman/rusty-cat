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
use crate::upload_file::UploadFileSnapshot;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

const MAX_PARALLEL_PART_TASKS: u128 = 256;
const MAX_PARALLEL_WINDOW_BYTES: u128 = 512 * 1024 * 1024;

/// Releases the lazily opened upload descriptor on every run-group exit path
/// (success, failure, cancel and pause). Part tasks are drained before the
/// parallel driver returns, so no positioned read can still be using the slot.
struct UploadFileHandleRelease(Option<UploadFileSnapshot>);

impl Drop for UploadFileHandleRelease {
    fn drop(&mut self) {
        if let Some(snapshot) = self.0.as_ref() {
            snapshot.release_handle();
        }
    }
}

/// Checks task-count and byte-allocation bounds only after the protocol has
/// opted into the parallel driver. The configured window is capped by the
/// remaining file's real part grid, so serial fallbacks and tiny files are not
/// rejected just because the builder contains a large maximum.
fn validate_parallel_part_window(
    max_parts_in_flight: usize,
    chunk_size: u64,
    remaining_bytes: u64,
) -> Result<(), MeowError> {
    if remaining_bytes == 0 {
        return Ok(());
    }
    let part_count = remaining_bytes
        .checked_sub(1)
        .and_then(|last| last.checked_div(chunk_size))
        .and_then(|parts_before_last| parts_before_last.checked_add(1))
        .ok_or_else(|| {
            MeowError::from_code_str(
                InnerErrorCode::IoError,
                "parallel part count overflow or zero chunk size",
            )
        })?;
    let effective_parts = (max_parts_in_flight.max(1) as u128).min(part_count as u128);
    if effective_parts > MAX_PARALLEL_PART_TASKS {
        return Err(MeowError::from_code(
            InnerErrorCode::IoError,
            format!(
                "parallel part window exceeds task limit: requested={effective_parts} limit={MAX_PARALLEL_PART_TASKS}"
            ),
        ));
    }
    let max_part_bytes = remaining_bytes.min(chunk_size) as u128;
    let budget = effective_parts.checked_mul(max_part_bytes).ok_or_else(|| {
        MeowError::from_code_str(
            InnerErrorCode::IoError,
            "parallel part window size overflow",
        )
    })?;
    if budget > MAX_PARALLEL_WINDOW_BYTES {
        return Err(MeowError::from_code(
            InnerErrorCode::IoError,
            format!(
                "parallel part window exceeds memory limit: requested={budget} limit={MAX_PARALLEL_WINDOW_BYTES}"
            ),
        ));
    }
    Ok(())
}

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

            if state.paused_set().contains(&key) {
                crate::meow_flow_log!(
                    "scheduler",
                    "skip key paused (requeue): key={}",
                    crate::inner::safe_key(&key)
                );
                state.queued_mut().push_back(key.clone());
                state.queued_set_mut().insert(key.clone());
                continue;
            }
            if state.active().contains_key(&key) {
                crate::meow_flow_log!(
                    "scheduler",
                    "skip key already active: key={}",
                    crate::inner::safe_key(&key)
                );
                state.queued_mut().push_back(key.clone());
                state.queued_set_mut().insert(key.clone());
                continue;
            }
            let Some(group) = state.groups().get(&key) else {
                crate::meow_warn_log!(
                    "scheduler",
                    "skip key missing group state: key={}",
                    crate::inner::safe_key(&key)
                );
                continue;
            };
            let direction = key.0;
            if !can_start_direction(state, direction) {
                crate::meow_trace_log!(
                    "scheduler",
                    "direction concurrency full, requeue key={} dir={:?}",
                    crate::inner::safe_key(&key),
                    direction
                );
                state.queued_mut().push_back(key.clone());
                state.queued_set_mut().insert(key);
                continue;
            }

            let inner = group.leader_inner().clone();
            let current = state.offsets().get(&key).copied().unwrap_or(0);
            // 下载任务在 prepare 之前可能还不知道远端 total；优先取调度器记录的
            // 运行期值（pause→resume 场景仍可用），仍为 0 时不发送 Transmission，
            // 避免对外看到 total=0 的误导进度。
            let start_total =
                crate::inner::exec_impl::emit::effective_total(state, &key, group.entry().inner());
            if !(direction == Direction::Download && start_total == 0) {
                crate::inner::exec_impl::emit::emit_status(
                    state,
                    group.entry(),
                    TransferStatus::Transmission,
                    current,
                    start_total,
                );
            }

            let cancel = CancellationToken::new();
            state.insert_active(key.clone(), ActiveState::new(cancel.clone()));
            started_keys.insert(key.clone());
            scheduled_in_this_round = true;

            let worker_tx_clone = worker_tx.clone();
            let executor = executor.clone();
            let start_offset = state.offsets().get(&key).copied().unwrap_or(0);
            crate::meow_key_log!(
                "scheduler",
                "start key={} from offset={} chunk_size={}",
                crate::inner::safe_key(&key),
                start_offset,
                inner.chunk_size()
            );
            tokio::spawn(async move {
                let panic_key = key.clone();
                let panic_tx = worker_tx_clone.clone();
                let panic_total = inner.total_size();
                let worker = tokio::spawn(async move {
                    run_group(key, inner, cancel, worker_tx_clone, executor, start_offset).await;
                });
                if let Err(join_err) = worker.await {
                    let err = MeowError::from_code(
                        InnerErrorCode::Unknown,
                        format!("run_group task panicked: {}", join_err),
                    );
                    let err_code = err.code();
                    crate::log::emit_lazy(|| {
                        crate::log::Log::error(
                            "run_group",
                            format!(
                                "run_group task panicked: key={} err={}",
                                crate::inner::safe_key(&panic_key),
                                crate::log::redact_secrets(&err.to_string())
                            ),
                        )
                        .with_error_code(err_code)
                    });
                    let _ = panic_tx
                        .send(WorkerEvent::Failed {
                            key: panic_key,
                            error: err,
                            total_size: panic_total,
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
    let active = state.active_direction_count(direction);
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
    crate::meow_key_log!(
        "run_group",
        "run_group begin: key={} task_id={:?} start_offset={}",
        crate::inner::safe_key(&key),
        inner.task_id(),
        start_offset
    );
    let task = TransferTask::from_inner(&inner);
    let _upload_file_handle_release = UploadFileHandleRelease(task.upload_file_snapshot().cloned());
    // 上传 `prepare` 已在 `DefaultHttpTransfer::upload_prepare` 内按 `max_upload_prepare_retries` 重试；
    // 此处仅对下载 `prepare`（HEAD 等）做外层连接级重试，避免与上传语义叠加或改写错误码。
    let max_prep_retries = match inner.direction() {
        Direction::Upload => 0,
        Direction::Download => inner.max_chunk_retries(),
    };
    let mut prep_attempt: u32 = 0;
    let PrepareOutcome {
        next_offset,
        total_size: prep_total,
    } = loop {
        if cancel.is_cancelled() {
            let _ = worker_tx
                .send(WorkerEvent::Canceled {
                    key: key.clone(),
                    total_size: inner.total_size(),
                })
                .await;
            return;
        }
        match executor.prepare(&task, start_offset).await {
            Ok(v) => break v,
            Err(e) => {
                if cancel.is_cancelled() {
                    let _ = worker_tx
                        .send(WorkerEvent::Canceled {
                            key: key.clone(),
                            total_size: inner.total_size(),
                        })
                        .await;
                    return;
                }
                let retryable = matches!(inner.direction(), Direction::Download)
                    && crate::inner::exec_impl::retry::is_connection_layer_retryable(&e);
                let reached_limit = prep_attempt >= max_prep_retries;
                if !retryable || reached_limit {
                    crate::log::emit_lazy(|| {
                        let mut log = crate::log::Log::error(
                            "run_group",
                            format!(
                                "prepare failed: key={} err={}",
                                crate::inner::safe_key(&key),
                                crate::log::redact_secrets(&e.to_string())
                            ),
                        )
                        .with_task_id(inner.task_id().to_string())
                        .with_offset(start_offset)
                        .with_attempt(prep_attempt)
                        .with_max_retries(max_prep_retries)
                        .with_error_code(e.code());
                        if let Some(status) = e.http_status() {
                            log = log.with_http_status(status);
                        }
                        log
                    });
                    let _ = worker_tx
                        .send(WorkerEvent::Failed {
                            key,
                            error: e,
                            total_size: inner.total_size(),
                        })
                        .await;
                    return;
                }
                let delay_ms =
                    crate::inner::exec_impl::retry::calc_backoff_with_jitter_ms(prep_attempt);
                crate::meow_warn_log!(
                    "run_group",
                    "prepare retry scheduled: key={} task_id={:?} attempt={} delay_ms={} err={}",
                    crate::inner::safe_key(&key),
                    inner.task_id(),
                    prep_attempt + 1,
                    delay_ms,
                    crate::log::redact_secrets(&e.to_string())
                );
                tokio::select! {
                    _ = cancel.cancelled() => {
                        let _ = worker_tx
                            .send(WorkerEvent::Canceled {
                                key: key.clone(),
                                total_size: inner.total_size(),
                            })
                            .await;
                        return;
                    }
                    _ = sleep(Duration::from_millis(delay_ms)) => {}
                }
                prep_attempt += 1;
            }
        }
    };
    let mut offset = next_offset;
    let mut known_total = if prep_total > 0 {
        prep_total
    } else {
        inner.total_size()
    };
    // prepare 成功后先回报一次当前进度（通常是 0%，但 total 已准确），
    // 让回调层尽早拿到真实总大小，而不是等首个分片完成后才更新。
    let _ = worker_tx
        .send(WorkerEvent::Progress {
            key: key.clone(),
            next_offset: offset,
            total_size: known_total,
        })
        .await;
    // 单文件内多分片并发（opt-in，optimization ④）：仅当调用方放大了
    // `max_parts_in_flight` 且所选协议证明乱序安全时，走窗口化并发路径；否则
    // （默认 `==1` / 不支持的协议）落到下面逐字未变的串行 loop，行为字节一致。
    // A windowed download needs a known total to build the part grid and pre-size
    // the file; when the size is unknown (0), fall back to the serial loop.
    if inner.max_parts_in_flight() > 1 && known_total > 0 && executor.supports_parallel_parts(&task)
    {
        if let Err(error) = validate_parallel_part_window(
            inner.max_parts_in_flight(),
            inner.chunk_size(),
            known_total.saturating_sub(offset),
        ) {
            let _ = worker_tx
                .send(WorkerEvent::Failed {
                    key,
                    error,
                    total_size: known_total,
                })
                .await;
            return;
        }
        run_group_parallel(
            key,
            &inner,
            &task,
            &cancel,
            &worker_tx,
            &executor,
            offset,
            known_total,
        )
        .await;
        return;
    }
    loop {
        if cancel.is_cancelled() {
            crate::meow_key_log!(
                "run_group",
                "cancellation observed: key={} task_id={:?} offset={}",
                crate::inner::safe_key(&key),
                inner.task_id(),
                offset
            );
            let _ = worker_tx
                .send(WorkerEvent::Canceled {
                    key,
                    total_size: known_total,
                })
                .await;
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
            crate::inner::exec_impl::retry::ChunkTransferMode::Whole,
        )
        .await
        {
            crate::inner::exec_impl::retry::ChunkRetryResult::Done(v) => v,
            crate::inner::exec_impl::retry::ChunkRetryResult::Cancelled => {
                crate::meow_key_log!(
                    "run_group",
                    "chunk retry interrupted by cancellation: key={} task_id={:?} offset={}",
                    crate::inner::safe_key(&key),
                    inner.task_id(),
                    offset
                );
                let _ = worker_tx
                    .send(WorkerEvent::Canceled {
                        key,
                        total_size: known_total,
                    })
                    .await;
                return;
            }
            crate::inner::exec_impl::retry::ChunkRetryResult::Failed(e) => {
                crate::log::emit_lazy(|| {
                    let mut log = crate::log::Log::error(
                        "run_group",
                        format!(
                            "chunk retry exhausted or non-retryable: key={} offset={} err={}",
                            crate::inner::safe_key(&key),
                            offset,
                            crate::log::redact_secrets(&e.to_string())
                        ),
                    )
                    .with_task_id(inner.task_id().to_string())
                    .with_offset(offset)
                    .with_byte_len(inner.chunk_size())
                    .with_error_code(e.code());
                    if let Some(status) = e.http_status() {
                        log = log.with_http_status(status);
                    }
                    log
                });
                let _ = worker_tx
                    .send(WorkerEvent::Failed {
                        key,
                        error: e,
                        total_size: known_total,
                    })
                    .await;
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
            crate::meow_key_log!(
                "run_group",
                "run_group completed: key={} task_id={:?} final_offset={} total={}",
                crate::inner::safe_key(&key),
                inner.task_id(),
                offset,
                known_total
            );
            let _ = worker_tx
                .send(WorkerEvent::Completed {
                    key,
                    total_size: known_total,
                    completion_payload: outcome.completion_payload,
                })
                .await;
            return;
        }
    }
}

#[cfg(test)]
mod resource_budget_tests {
    use super::validate_parallel_part_window;

    #[test]
    fn parallel_window_uses_real_grid_and_rejects_task_or_memory_exhaustion() {
        validate_parallel_part_window(usize::MAX, 1024 * 1024, 2 * 1024 * 1024)
            .expect("a huge configured maximum is harmless for a two-part file");
        assert!(validate_parallel_part_window(257, 1, 257).is_err());
        assert!(
            validate_parallel_part_window(3, 256 * 1024 * 1024, 3 * 256 * 1024 * 1024,).is_err()
        );
        assert!(validate_parallel_part_window(2, 0, 1).is_err());
    }
}

/// Spawns one in-flight part (upload of a single chunk at `offset`) onto the
/// JoinSet, driven through the shared per-chunk retry loop in `Part` mode so it
/// never finalizes the upload. Each part owns cheap clones of the executor Arc,
/// the task (its `Arc` fields — file slot, protocol — are shared so accounting
/// is consistent), the dedupe key, and the shared cancellation token.
#[allow(clippy::too_many_arguments)]
fn spawn_part(
    set: &mut tokio::task::JoinSet<(u64, crate::inner::exec_impl::retry::ChunkRetryResult)>,
    executor: &Arc<dyn TransferTrait>,
    task: &TransferTask,
    key: &UniqueId,
    cancel: &CancellationToken,
    offset: u64,
    chunk_size: u64,
    known_total: u64,
    max_chunk_retries: u32,
) {
    let executor = executor.clone();
    let task = task.clone();
    let key = key.clone();
    let cancel = cancel.clone();
    set.spawn(async move {
        let result = crate::inner::exec_impl::retry::transfer_chunk_with_retry(
            &executor,
            &task,
            &key,
            &cancel,
            offset,
            chunk_size,
            known_total,
            max_chunk_retries,
            crate::inner::exec_impl::retry::ChunkTransferMode::Part,
        )
        .await;
        (offset, result)
    });
}

/// Windowed concurrent driver for one file's parts (optimization ④, opt-in).
///
/// Confines ALL intra-file concurrency here: it keeps `run_group` the sole
/// `worker_tx` sender for its group, emits only the contiguous-prefix watermark
/// as Progress (so `SchedulerState.offsets` never sees a hole), and finalizes
/// the upload exactly once after the join barrier. The scheduler,
/// `SchedulerState`, `handle_worker_event`, the cancellation plane, and the wire
/// protocols are all untouched.
#[allow(clippy::too_many_arguments)]
async fn run_group_parallel(
    key: UniqueId,
    inner: &InnerTask,
    task: &TransferTask,
    cancel: &CancellationToken,
    worker_tx: &mpsc::Sender<WorkerEvent>,
    executor: &Arc<dyn TransferTrait>,
    start_offset: u64,
    known_total: u64,
) {
    use crate::inner::exec_impl::part_window::PartWindow;
    use crate::inner::exec_impl::retry::ChunkRetryResult;

    crate::meow_key_log!(
        "run_group_parallel",
        "begin: key={} task_id={:?} start_offset={} total={} max_parts={}",
        crate::inner::safe_key(&key),
        inner.task_id(),
        start_offset,
        known_total,
        inner.max_parts_in_flight()
    );

    // Resume already at total: every part is already durably written.
    //
    // For a DOWNLOAD, `start_offset` is the sidecar's contiguous watermark, so
    // `watermark == total` means the whole file is on disk but the `.rcdl`
    // sidecar may still exist. Finalize here so `complete()` can validate the
    // final length and delete the sidecar; otherwise a `.rcdl` that survived a
    // crash-before-cleanup leaks forever and a later serial re-download trips
    // the cross-mode guard. `download_prepare` has already populated
    // `download_progress()`, so `complete()` has what it needs to validate.
    //
    // For an UPLOAD (and any non-download) keep the existing behavior exactly:
    // emit Completed WITHOUT re-finalizing, preserving the "already-complete
    // upload resume emits Completed without re-running complete" semantics.
    if start_offset >= known_total {
        if task.direction() == Direction::Download {
            match executor.complete(task).await {
                Ok(payload) => {
                    let _ = worker_tx
                        .send(WorkerEvent::Completed {
                            key,
                            total_size: known_total,
                            completion_payload: payload,
                        })
                        .await;
                }
                Err(e) => {
                    crate::log::emit_lazy(|| {
                        let mut log = crate::log::Log::error(
                            "run_group_parallel",
                            format!(
                                "download finalize at total failed: key={} err={}",
                                crate::inner::safe_key(&key),
                                crate::log::redact_secrets(&e.to_string())
                            ),
                        )
                        .with_task_id(inner.task_id().to_string())
                        .with_error_code(e.code());
                        if let Some(status) = e.http_status() {
                            log = log.with_http_status(status);
                        }
                        log
                    });
                    let _ = worker_tx
                        .send(WorkerEvent::Failed {
                            key,
                            error: e,
                            total_size: known_total,
                        })
                        .await;
                }
            }
        } else {
            let _ = worker_tx
                .send(WorkerEvent::Completed {
                    key,
                    total_size: known_total,
                    completion_payload: None,
                })
                .await;
        }
        return;
    }

    let chunk = inner.chunk_size();
    let max_retries = inner.max_chunk_retries();
    let mut window = PartWindow::new(
        start_offset,
        chunk,
        known_total,
        inner.max_parts_in_flight(),
    );
    let mut set: tokio::task::JoinSet<(u64, ChunkRetryResult)> = tokio::task::JoinSet::new();
    let mut cancelled = false;
    let mut failed: Option<MeowError> = None;

    // Fill the window with the first batch of parts.
    while let Some(off) = window.take_dispatch() {
        spawn_part(
            &mut set,
            executor,
            task,
            &key,
            cancel,
            off,
            chunk,
            known_total,
            max_retries,
        );
    }

    // Drain the JoinSet to empty: this loop IS the join barrier. A single
    // terminal event is emitted only after every part has settled.
    while let Some(joined) = set.join_next().await {
        match joined {
            Err(join_err) => {
                // A part task panicked. Stop siblings, drain to quiescence, and
                // fail the whole file — a panicked part's bytes are unverified,
                // so the object must never be completed. (The outer panic guard
                // in `try_start_next` does not cover JoinSet children.)
                cancel.cancel();
                while set.join_next().await.is_some() {}
                let err = MeowError::from_code(
                    InnerErrorCode::Unknown,
                    format!("upload part task panicked: {join_err}"),
                );
                crate::log::emit_lazy(|| {
                    crate::log::Log::error(
                        "run_group_parallel",
                        format!(
                            "part task panicked: key={} err={}",
                            crate::inner::safe_key(&key),
                            crate::log::redact_secrets(&err.to_string())
                        ),
                    )
                    .with_task_id(inner.task_id().to_string())
                    .with_error_code(err.code())
                });
                let _ = worker_tx
                    .send(WorkerEvent::Failed {
                        key,
                        error: err,
                        total_size: known_total,
                    })
                    .await;
                return;
            }
            Ok((off, ChunkRetryResult::Done(_))) => {
                match window.on_done(off) {
                    // Emit ONE coalesced Progress only when the contiguous
                    // prefix advances — never a raw per-part offset.
                    Ok(Some(watermark)) => {
                        let _ = worker_tx
                            .send(WorkerEvent::Progress {
                                key: key.clone(),
                                next_offset: watermark,
                                total_size: known_total,
                            })
                            .await;
                    }
                    Ok(None) => {}
                    // Internal accounting violation: record it as a failure and
                    // stop dispatching; keep draining in-flight siblings so a
                    // single terminal event is still emitted after the barrier.
                    Err(e) => {
                        crate::log::emit_lazy(|| {
                            crate::log::Log::error(
                                "run_group_parallel",
                                format!(
                                    "part window on_done invalid task state: key={} part_offset={} err={}",
                                    crate::inner::safe_key(&key),
                                    off,
                                    crate::log::redact_secrets(&e.to_string())
                                ),
                            )
                            .with_task_id(inner.task_id().to_string())
                            .with_offset(off)
                            .with_error_code(e.code())
                        });
                        if failed.is_none() {
                            failed = Some(e);
                        }
                    }
                }
                // Top up the window only while still healthy.
                if !cancelled && failed.is_none() {
                    while let Some(next_off) = window.take_dispatch() {
                        spawn_part(
                            &mut set,
                            executor,
                            task,
                            &key,
                            cancel,
                            next_off,
                            chunk,
                            known_total,
                            max_retries,
                        );
                    }
                }
            }
            Ok((_off, ChunkRetryResult::Cancelled)) => {
                cancelled = true;
                window.on_settled_without_progress();
                // Stop dispatching; keep draining the rest to quiescence.
            }
            Ok((_off, ChunkRetryResult::Failed(e))) => {
                if failed.is_none() {
                    failed = Some(e);
                }
                window.on_settled_without_progress();
                // Stop dispatching new parts; let in-flight siblings settle
                // naturally (do NOT cancel the token — that would masquerade a
                // genuine failure as a user cancel).
            }
        }
    }

    // Join barrier reached (JoinSet empty). Emit exactly one terminal event,
    // prioritizing a genuine failure over a cancel (the retry layer already maps
    // user-cancel in-flight errors to Cancelled, so `failed` means a real error).
    if let Some(e) = failed {
        if task.direction() == Direction::Download {
            if let Err(checkpoint_error) = task.force_download_checkpoint().await {
                crate::meow_warn_log!(
                    "run_group_parallel",
                    "checkpoint while settling failed download also failed: {}",
                    crate::log::redact_secrets(&checkpoint_error.to_string())
                );
            }
        }
        crate::log::emit_lazy(|| {
            let mut log = crate::log::Log::error(
                "run_group_parallel",
                format!(
                    "failed after drain: key={} err={}",
                    crate::inner::safe_key(&key),
                    crate::log::redact_secrets(&e.to_string())
                ),
            )
            .with_task_id(inner.task_id().to_string())
            .with_error_code(e.code());
            if let Some(status) = e.http_status() {
                log = log.with_http_status(status);
            }
            log
        });
        let _ = worker_tx
            .send(WorkerEvent::Failed {
                key,
                error: e,
                total_size: known_total,
            })
            .await;
    } else if cancelled || cancel.is_cancelled() {
        if task.direction() == Direction::Download {
            if let Err(error) = task.force_download_checkpoint().await {
                let _ = worker_tx
                    .send(WorkerEvent::Failed {
                        key,
                        error,
                        total_size: known_total,
                    })
                    .await;
                return;
            }
        }
        // FLAW-1: re-check cancel right before finalizing so `complete` can never
        // race the `abort_upload` that `cancel_group` issues; treat a late cancel
        // as Canceled even if every part already finished.
        crate::meow_key_log!(
            "run_group_parallel",
            "canceled after drain: key={} task_id={:?} watermark={}",
            crate::inner::safe_key(&key),
            inner.task_id(),
            window.watermark()
        );
        let _ = worker_tx
            .send(WorkerEvent::Canceled {
                key,
                total_size: known_total,
            })
            .await;
    } else {
        if !window.is_complete() {
            // Internal invariant: the success path must finalize only a fully
            // contiguous prefix. If somehow not complete, fail the file instead
            // of completing a possibly-incomplete remote object (a debug_assert
            // here would be stripped in release and silently complete bad data).
            let err = MeowError::from_code(
                InnerErrorCode::Unknown,
                format!(
                    "internal: complete fired before contiguous prefix reached total (watermark={}, total={})",
                    window.watermark(),
                    known_total
                ),
            );
            crate::log::emit_lazy(|| {
                let mut log = crate::log::Log::error(
                    "run_group_parallel",
                    format!(
                        "invariant violation before complete: key={} err={}",
                        crate::inner::safe_key(&key),
                        crate::log::redact_secrets(&err.to_string())
                    ),
                )
                .with_task_id(inner.task_id().to_string())
                .with_error_code(err.code());
                if let Some(status) = err.http_status() {
                    log = log.with_http_status(status);
                }
                log
            });
            let _ = worker_tx
                .send(WorkerEvent::Failed {
                    key,
                    error: err,
                    total_size: known_total,
                })
                .await;
            return;
        }
        match executor.complete(task).await {
            Ok(completion_payload) => {
                crate::meow_key_log!(
                    "run_group_parallel",
                    "completed: key={} task_id={:?} total={}",
                    crate::inner::safe_key(&key),
                    inner.task_id(),
                    known_total
                );
                let _ = worker_tx
                    .send(WorkerEvent::Completed {
                        key,
                        total_size: known_total,
                        completion_payload,
                    })
                    .await;
            }
            Err(e) => {
                crate::log::emit_lazy(|| {
                    let mut log = crate::log::Log::error(
                        "run_group_parallel",
                        format!(
                            "complete call failed: key={} err={}",
                            crate::inner::safe_key(&key),
                            crate::log::redact_secrets(&e.to_string())
                        ),
                    )
                    .with_task_id(inner.task_id().to_string())
                    .with_error_code(e.code());
                    if let Some(status) = e.http_status() {
                        log = log.with_http_status(status);
                    }
                    log
                });
                let _ = worker_tx
                    .send(WorkerEvent::Failed {
                        key,
                        error: e,
                        total_size: known_total,
                    })
                    .await;
            }
        }
    }
}
