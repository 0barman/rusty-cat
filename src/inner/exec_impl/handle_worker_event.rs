use crate::transfer_status::TransferStatus;

use crate::inner::scheduler_state::SchedulerState;
use crate::inner::worker_event::WorkerEvent;

pub(crate) async fn handle_worker_event(event: WorkerEvent, state: &mut SchedulerState) {
    match event {
        WorkerEvent::Progress {
            key,
            next_offset,
            total_size,
        } => {
            crate::meow_trace_log!(
                "worker_event",
                "progress: key={} next_offset={} total_size={}",
                crate::inner::safe_key(&key),
                next_offset,
                total_size
            );
            // Persist/emit progress only for a still-live group. Gating the
            // insert on the live group prevents a late or out-of-order Progress
            // (possible once parts run concurrently) from re-creating an offsets
            // entry that a terminal event already removed. The single-sender
            // discipline in run_group already orders events, so this is
            // defense-in-depth and is a no-op for the serial path (where Progress
            // always precedes the terminal event).
            if state.groups().contains_key(&key) {
                state.offsets_mut().insert(key.clone(), next_offset);
                if let Some(group) = state.groups().get(&key) {
                    crate::inner::exec_impl::emit::emit_status(
                        state,
                        group.entry(),
                        TransferStatus::Transmission,
                        next_offset,
                        total_size,
                    );
                }
            } else {
                crate::log::emit_lazy(|| {
                    crate::log::Log::warn(
                        "worker_event",
                        format!(
                            "stray Progress for dead group; ignored next_offset={} total_size={}",
                            next_offset, total_size
                        ),
                    )
                    .with_key(key.1.as_str())
                    .with_offset(next_offset)
                });
            }
        }
        WorkerEvent::Completed {
            key,
            total_size,
            completion_payload,
        } => {
            crate::meow_key_log!(
                "worker_event",
                "completed: key={} total_size={}",
                crate::inner::safe_key(&key),
                total_size
            );
            state.active_mut().remove(&key);
            // 完成后无论此前是否 paused，都要清理 paused 标记。
            state.paused_set_mut().remove(&key);
            state.offsets_mut().insert(key.clone(), total_size);
            if let Some(group) = state.groups_mut().remove(&key) {
                state
                    .task_id_to_dedupe_mut()
                    .remove(&group.leader_inner().task_id());
                let task_id = group.entry().inner().task_id();
                crate::inner::exec_impl::emit::emit_status(
                    state,
                    group.entry(),
                    TransferStatus::Complete,
                    total_size,
                    total_size,
                );
                if let Some(cb) = group.entry().callbacks().complete_cb() {
                    crate::inner::exec_impl::emit::invoke_complete_cb(
                        state,
                        cb,
                        task_id,
                        completion_payload,
                    );
                }
            } else {
                crate::log::emit_lazy(|| {
                    crate::log::Log::warn(
                        "worker_event",
                        format!("Completed for unknown group; nothing to finalize total_size={}", total_size),
                    )
                    .with_key(key.1.as_str())
                });
            }
            state.offsets_mut().remove(&key);
        }
        WorkerEvent::Failed { key, error } => {
            state.active_mut().remove(&key);
            // 失败终态会结束任务生命周期，因此同步清理 paused 标记。
            state.paused_set_mut().remove(&key);
            if let Some(group) = state.groups_mut().remove(&key) {
                state
                    .task_id_to_dedupe_mut()
                    .remove(&group.leader_inner().task_id());
                let current = state.offsets().get(&key).copied().unwrap_or(0);
                // Core chunk/part-failure log: terminal task failure with the
                // contiguous offset reached and the underlying error detail.
                // Emitted before `error` is moved into `TransferStatus::Failed`.
                crate::log::emit_lazy(|| {
                    let mut log = crate::log::Log::error(
                        "worker_event",
                        format!("task failed: {}", crate::log::redact_secrets(&error.to_string())),
                    )
                    .with_key(key.1.as_str())
                    .with_offset(current)
                    .with_error_code(error.code());
                    if let Some(status) = error.http_status() {
                        log = log.with_http_status(status);
                    }
                    log
                });
                crate::inner::exec_impl::emit::emit_status(
                    state,
                    group.entry(),
                    TransferStatus::Failed(error),
                    current,
                    group.entry().inner().total_size(),
                );
            } else {
                crate::log::emit_lazy(|| {
                    crate::log::Log::warn(
                        "worker_event",
                        format!("Failed for unknown group; nothing to finalize error={}", crate::log::redact_secrets(&error.to_string())),
                    )
                    .with_key(key.1.as_str())
                    .with_error_code(error.code())
                });
            }
            state.offsets_mut().remove(&key);
        }
        WorkerEvent::Canceled { key } => {
            crate::meow_key_log!("worker_event", "canceled: key={}", crate::inner::safe_key(&key));
            state.active_mut().remove(&key);
            // 若 key 在 paused_set 中，表示该取消来自 pause 流程，仅收敛执行态，不销毁 group。
            if state.paused_set().contains(&key) {
                crate::meow_key_log!(
                    "worker_event",
                    "canceled from pause flow; keep group for resume: key={}",
                    crate::inner::safe_key(&key)
                );
                return;
            }
            if let Some(group) = state.groups_mut().remove(&key) {
                state
                    .task_id_to_dedupe_mut()
                    .remove(&group.leader_inner().task_id());
                let current = state.offsets().get(&key).copied().unwrap_or(0);
                crate::inner::exec_impl::emit::emit_status(
                    state,
                    group.entry(),
                    TransferStatus::Canceled,
                    current,
                    group.entry().inner().total_size(),
                );
            } else {
                crate::log::emit_lazy(|| {
                    crate::log::Log::warn(
                        "worker_event",
                        "Canceled for unknown group; nothing to finalize".to_string(),
                    )
                    .with_key(key.1.as_str())
                });
            }
            state.offsets_mut().remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use super::handle_worker_event;
    use crate::dflt::default_http_transfer::default_breakpoint_arcs;
    use crate::http_breakpoint::BreakpointDownloadHttpConfig;
    use crate::inner::cb_dispatcher;
    use crate::inner::group_state::{GroupState, RecordEntry};
    use crate::inner::inner_task::InnerTask;
    use crate::inner::scheduler_state::SchedulerState;
    use crate::inner::task_callbacks::TaskCallbacks;
    use crate::inner::worker_event::WorkerEvent;
    use crate::inner::UniqueId;
    use crate::up_pounce_builder::UploadPounceBuilder;

    /// Builds a SchedulerState holding one live upload group, exactly as the
    /// scheduler would after the task started (group + offsets + task_id map
    /// populated). Returns the state, the group key, its total size, and the
    /// dispatcher join guard (kept alive so callback submission stays connected).
    async fn live_upload_state() -> (SchedulerState, UniqueId, u64) {
        let mut path = std::env::temp_dir();
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        path.push(format!("rusty_cat_offsets_teardown_{ts}.bin"));
        std::fs::write(&path, vec![7u8; 4096]).expect("write fixture");

        let pounce = UploadPounceBuilder::new("teardown.bin", &path, 1024)
            .with_url("https://placeholder/upload")
            .build()
            .expect("build pounce");
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
        let _ = std::fs::remove_file(&path);

        let key = inner.dedupe_key();
        let total = inner.total_size();
        let (cb_submit, cb_join) = cb_dispatcher::start().expect("start dispatcher");
        // Detach the dispatcher join guard. Its `Drop` blocks on `join`, which
        // only returns once the channel's sole sender (held inside `state`) is
        // dropped. Relying on local drop order is fragile in a test that may
        // panic mid-assertion (the guard would join while the sender is still
        // alive — a deadlock). Forgetting it is safe: the dispatcher thread still
        // exits on its own when `state` drops and closes the channel.
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

        (state, key, total)
    }

    /// MISSED-D: once a terminal event has torn down a group, a late or
    /// out-of-order `Progress` (possible only once parts run concurrently) must
    /// NOT re-create an orphan `offsets` entry. The insert is gated on the group
    /// still being live.
    #[tokio::test]
    async fn stray_progress_after_complete_creates_no_orphan_offset() {
        let (mut state, key, total) = live_upload_state().await;
        assert!(state.groups().contains_key(&key));

        // Terminal Completed tears the group down and clears its offsets entry.
        handle_worker_event(
            WorkerEvent::Completed {
                key: key.clone(),
                total_size: total,
                completion_payload: None,
            },
            &mut state,
        )
        .await;
        assert!(
            !state.groups().contains_key(&key),
            "group must be removed after Completed"
        );
        assert!(
            state.offsets().get(&key).is_none(),
            "offsets entry must be cleared after Completed"
        );

        // A stray Progress for the now-dead group must be a no-op for offsets.
        handle_worker_event(
            WorkerEvent::Progress {
                key: key.clone(),
                next_offset: 512,
                total_size: total,
            },
            &mut state,
        )
        .await;
        assert!(
            state.offsets().get(&key).is_none(),
            "stray Progress after teardown must not resurrect an orphan offsets entry"
        );
    }

    /// A Progress for a STILL-LIVE group does persist (proves the guard is not
    /// over-broad and the serial path is unaffected).
    #[tokio::test]
    async fn progress_for_live_group_persists_offset() {
        let (mut state, key, total) = live_upload_state().await;
        handle_worker_event(
            WorkerEvent::Progress {
                key: key.clone(),
                next_offset: 1024,
                total_size: total,
            },
            &mut state,
        )
        .await;
        assert_eq!(
            state.offsets().get(&key).copied(),
            Some(1024),
            "progress for a live group must persist its contiguous offset"
        );
    }
}
