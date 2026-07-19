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
                // 记录运行期发现的总大小；0 表示上游未知，不得覆盖已记录的真实值。
                // 与 offsets 同享"仅组存活时写入"的防御（防止迟到事件复活孤儿条目）。
                if total_size > 0 {
                    state.known_totals_mut().insert(key.clone(), total_size);
                }
                if let Some(group) = state.groups().get(&key) {
                    // 发射侧防回退：total=0 的中间帧不得把已知总大小的记录打回 0
                    // （与上面存储侧 known_totals 的写入守卫呼应）。
                    let total = total_size.max(crate::inner::exec_impl::emit::effective_total(
                        state,
                        &key,
                        group.entry().inner(),
                    ));
                    crate::inner::exec_impl::emit::emit_status(
                        state,
                        group.entry(),
                        TransferStatus::Transmission,
                        next_offset,
                        total,
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
                // Complete 语义即 100%：total 取事件值与运行期/预设值的较大者，
                // 防御自定义执行器以 total=0 完成时把终态记录写成 0/0。
                let total = total_size.max(crate::inner::exec_impl::emit::effective_total(
                    state,
                    &key,
                    group.entry().inner(),
                ));
                crate::inner::exec_impl::emit::emit_status(
                    state,
                    group.entry(),
                    TransferStatus::Complete,
                    total,
                    total,
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
            state.known_totals_mut().remove(&key);
        }
        WorkerEvent::Failed {
            key,
            error,
            total_size,
        } => {
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
                // 终态 total 汰选：事件携带值 / 调度器运行期记录 / 预设值三者取最大，
                // 避免未预设 total 的下载在终态把已知总大小回退成 0（清零消费端进度）。
                let total = total_size.max(crate::inner::exec_impl::emit::effective_total(
                    state,
                    &key,
                    group.entry().inner(),
                ));
                crate::inner::exec_impl::emit::emit_status(
                    state,
                    group.entry(),
                    TransferStatus::Failed(error),
                    current,
                    total,
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
            state.known_totals_mut().remove(&key);
        }
        WorkerEvent::Canceled { key, total_size } => {
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
                // 终态 total 汰选（同 Failed 臂）：事件值 / 运行期记录 / 预设值取最大。
                let total = total_size.max(crate::inner::exec_impl::emit::effective_total(
                    state,
                    &key,
                    group.entry().inner(),
                ));
                crate::inner::exec_impl::emit::emit_status(
                    state,
                    group.entry(),
                    TransferStatus::Canceled,
                    current,
                    total,
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
            state.known_totals_mut().remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use super::handle_worker_event;
    use crate::dflt::default_http_transfer::default_breakpoint_arcs;
    use crate::error::{InnerErrorCode, MeowError};
    use crate::http_breakpoint::BreakpointDownloadHttpConfig;
    use crate::inner::cb_dispatcher;
    use crate::inner::group_state::{GroupState, RecordEntry};
    use crate::inner::inner_task::InnerTask;
    use crate::inner::scheduler_state::SchedulerState;
    use crate::inner::task_callbacks::TaskCallbacks;
    use crate::inner::test_support::{attach_capture, live_download_state, wait_for_record};
    use crate::inner::worker_event::WorkerEvent;
    use crate::inner::UniqueId;
    use crate::transfer_status::TransferStatus;
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

    /// 复现开发者反馈的线上问题：未预设 total 的下载有进度后失败，终态 Failed
    /// 记录把 total/progress 清零（消费端据此覆盖 DB 会丢进度）。
    ///
    /// 修复前（当前代码）：Progress 携带 prepare 探测到的真实 total=4096，
    /// 传输中记录正常显示 512/4096=0.125；但 Failed 终态回读 `inner.total_size()`
    /// (=0)，emit_status 在 total==0 时强制 progress=0.0 → 终态记录 total=0/
    /// progress=0.0。本测试的两条终态断言当前会 FAIL，正是复现该缺陷。
    /// 修复后（known_totals + effective_total）：终态保留 4096/0.125。
    #[tokio::test]
    async fn repro_download_failed_after_progress_zeroes_total() {
        let (mut state, key) = live_download_state("repro_failed").await;
        let records = attach_capture(&state);

        // Progress 事件携带 prepare 探测到的真实 total（模拟已下载 512/4096）。
        handle_worker_event(
            WorkerEvent::Progress {
                key: key.clone(),
                next_offset: 512,
                total_size: 4096,
            },
            &mut state,
        )
        .await;
        let transmission = wait_for_record(&records, |r| {
            matches!(r.status(), TransferStatus::Transmission)
        })
        .await;
        assert_eq!(transmission.total_size(), 4096, "传输中记录应带真实 total");
        assert!((transmission.progress() - 0.125).abs() < f32::EPSILON);

        // 终态失败：事件刻意携带 total_size=0（模拟 worker 未上报），验证方案 A
        // （known_totals 兜底）单独就能把终态记录恢复成 total=4096/progress=0.125。
        handle_worker_event(
            WorkerEvent::Failed {
                key: key.clone(),
                error: MeowError::from_code(InnerErrorCode::Unknown, "boom".to_string()),
                total_size: 0,
            },
            &mut state,
        )
        .await;
        let failed = wait_for_record(&records, |r| {
            matches!(r.status(), TransferStatus::Failed(_))
        })
        .await;
        assert_eq!(
            failed.total_size(),
            4096,
            "终态必须保留运行期 total（复现点）"
        );
        assert!(
            (failed.progress() - 0.125).abs() < f32::EPSILON,
            "终态必须保留 512/4096 进度（复现点）"
        );
    }

    /// Progress 事件必须把运行期发现的 total 记入调度器（生命周期与 offsets 对齐）。
    #[tokio::test]
    async fn progress_records_runtime_total_into_scheduler() {
        let (mut state, key) = live_download_state("record_total").await;
        handle_worker_event(
            WorkerEvent::Progress {
                key: key.clone(),
                next_offset: 512,
                total_size: 4096,
            },
            &mut state,
        )
        .await;
        assert_eq!(state.known_totals().get(&key).copied(), Some(4096));
    }

    /// total=0 的 Progress（上游未知大小）不得覆盖已记录的真实 total。
    #[tokio::test]
    async fn zero_total_progress_does_not_clobber_known_total() {
        let (mut state, key) = live_download_state("no_clobber").await;
        handle_worker_event(
            WorkerEvent::Progress {
                key: key.clone(),
                next_offset: 512,
                total_size: 4096,
            },
            &mut state,
        )
        .await;
        handle_worker_event(
            WorkerEvent::Progress {
                key: key.clone(),
                next_offset: 600,
                total_size: 0,
            },
            &mut state,
        )
        .await;
        assert_eq!(state.known_totals().get(&key).copied(), Some(4096));
    }

    /// 终态 Completed 必须清理 known_totals（与 offsets 同步）。
    #[tokio::test]
    async fn terminal_completed_cleans_known_totals() {
        let (mut state, key) = live_download_state("complete_cleans").await;
        handle_worker_event(
            WorkerEvent::Progress {
                key: key.clone(),
                next_offset: 512,
                total_size: 4096,
            },
            &mut state,
        )
        .await;
        handle_worker_event(
            WorkerEvent::Completed {
                key: key.clone(),
                total_size: 4096,
                completion_payload: None,
            },
            &mut state,
        )
        .await;
        assert!(state.known_totals().get(&key).is_none());
        assert!(state.offsets().get(&key).is_none());
    }

    /// 终态 Failed 必须清理 known_totals。
    #[tokio::test]
    async fn terminal_failed_cleans_known_totals() {
        let (mut state, key) = live_download_state("failed_cleans").await;
        handle_worker_event(
            WorkerEvent::Progress {
                key: key.clone(),
                next_offset: 512,
                total_size: 4096,
            },
            &mut state,
        )
        .await;
        handle_worker_event(
            WorkerEvent::Failed {
                key: key.clone(),
                error: MeowError::from_code(InnerErrorCode::Unknown, "boom".to_string()),
                total_size: 0,
            },
            &mut state,
        )
        .await;
        assert!(state.known_totals().get(&key).is_none());
        assert!(state.offsets().get(&key).is_none());
    }

    /// 非 pause 流的终态 Canceled 必须清理 known_totals。
    #[tokio::test]
    async fn terminal_canceled_cleans_known_totals() {
        let (mut state, key) = live_download_state("canceled_cleans").await;
        handle_worker_event(
            WorkerEvent::Progress {
                key: key.clone(),
                next_offset: 512,
                total_size: 4096,
            },
            &mut state,
        )
        .await;
        handle_worker_event(
            WorkerEvent::Canceled {
                key: key.clone(),
                total_size: 0,
            },
            &mut state,
        )
        .await;
        assert!(state.known_totals().get(&key).is_none());
    }

    /// pause 流的 Canceled 只收敛执行态：known_totals 必须保留（供 resume 后使用），
    /// 与 offsets 在 pause 期间保留的既有语义一致。
    #[tokio::test]
    async fn pause_flow_canceled_keeps_known_total() {
        let (mut state, key) = live_download_state("pause_keeps").await;
        handle_worker_event(
            WorkerEvent::Progress {
                key: key.clone(),
                next_offset: 512,
                total_size: 4096,
            },
            &mut state,
        )
        .await;
        state.paused_set_mut().insert(key.clone());
        handle_worker_event(
            WorkerEvent::Canceled {
                key: key.clone(),
                total_size: 0,
            },
            &mut state,
        )
        .await;
        assert!(state.groups().contains_key(&key), "pause 流不得销毁 group");
        assert_eq!(state.known_totals().get(&key).copied(), Some(4096));
    }

    /// MISSED-D 同款防御：终态拆组后迟到的 Progress 不得复活 known_totals 孤儿条目。
    #[tokio::test]
    async fn stray_progress_after_teardown_creates_no_orphan_known_total() {
        let (mut state, key) = live_download_state("stray_progress").await;
        handle_worker_event(
            WorkerEvent::Completed {
                key: key.clone(),
                total_size: 4096,
                completion_payload: None,
            },
            &mut state,
        )
        .await;
        handle_worker_event(
            WorkerEvent::Progress {
                key: key.clone(),
                next_offset: 512,
                total_size: 4096,
            },
            &mut state,
        )
        .await;
        assert!(state.known_totals().get(&key).is_none());
    }

    /// 方案 B 单独生效（Failed 臂，finding #9）：调度器无 known_totals 记录
    /// （失败发生在首个 Progress 前），终态 Failed 记录使用事件携带的 total。
    #[tokio::test]
    async fn download_failed_prefers_event_total_when_scheduler_has_none() {
        let (mut state, key) = live_download_state("failed_event").await;
        let records = attach_capture(&state);
        handle_worker_event(
            WorkerEvent::Failed {
                key: key.clone(),
                error: MeowError::from_code(InnerErrorCode::Unknown, "boom".to_string()),
                total_size: 8192,
            },
            &mut state,
        )
        .await;
        let failed = wait_for_record(&records, |r| {
            matches!(r.status(), TransferStatus::Failed(_))
        })
        .await;
        assert_eq!(failed.total_size(), 8192, "无 known_totals 时终态用事件携带的 total");
    }

    /// 方案 B 单独生效：调度器无记录（取消发生在首个 Progress 前），
    /// 终态记录使用事件携带的 total。
    #[tokio::test]
    async fn download_canceled_prefers_event_total_when_scheduler_has_none() {
        let (mut state, key) = live_download_state("canceled_event").await;
        let records = attach_capture(&state);
        handle_worker_event(
            WorkerEvent::Canceled {
                key: key.clone(),
                total_size: 8192,
            },
            &mut state,
        )
        .await;
        let canceled =
            wait_for_record(&records, |r| matches!(r.status(), TransferStatus::Canceled)).await;
        assert_eq!(canceled.total_size(), 8192);
        assert!(!state.groups().contains_key(&key));
    }

    /// 三来源（事件/运行期/预设）皆 0 时钉住既有行为：total=0、progress=0.0。
    #[tokio::test]
    async fn download_failed_with_no_total_anywhere_keeps_zero() {
        let (mut state, key) = live_download_state("failed_zero").await;
        let records = attach_capture(&state);
        handle_worker_event(
            WorkerEvent::Failed {
                key: key.clone(),
                error: MeowError::from_code(InnerErrorCode::Unknown, "boom".to_string()),
                total_size: 0,
            },
            &mut state,
        )
        .await;
        let failed = wait_for_record(&records, |r| {
            matches!(r.status(), TransferStatus::Failed(_))
        })
        .await;
        assert_eq!(failed.total_size(), 0);
        assert_eq!(failed.progress(), 0.0);
    }

    /// 上传回归：预设 total（=文件大小）的任务终态取值不变。
    #[tokio::test]
    async fn upload_failed_keeps_preset_total() {
        let (mut state, key, total) = live_upload_state().await;
        let records = attach_capture(&state);
        handle_worker_event(
            WorkerEvent::Failed {
                key: key.clone(),
                error: MeowError::from_code(InnerErrorCode::Unknown, "boom".to_string()),
                total_size: 0,
            },
            &mut state,
        )
        .await;
        let failed = wait_for_record(&records, |r| {
            matches!(r.status(), TransferStatus::Failed(_))
        })
        .await;
        assert_eq!(
            failed.total_size(),
            total,
            "上传终态 total 必须保持预设值(文件大小)"
        );
    }

    /// 防御自定义执行器以 total=0 结束：Complete 终态回退到运行期 total，进度 1.0。
    #[tokio::test]
    async fn completed_with_zero_event_total_falls_back_to_known_total() {
        let (mut state, key) = live_download_state("complete_fallback").await;
        let records = attach_capture(&state);
        handle_worker_event(
            WorkerEvent::Progress {
                key: key.clone(),
                next_offset: 4096,
                total_size: 4096,
            },
            &mut state,
        )
        .await;
        handle_worker_event(
            WorkerEvent::Completed {
                key: key.clone(),
                total_size: 0,
                completion_payload: None,
            },
            &mut state,
        )
        .await;
        let complete =
            wait_for_record(&records, |r| matches!(r.status(), TransferStatus::Complete)).await;
        assert_eq!(complete.total_size(), 4096);
        assert!((complete.progress() - 1.0).abs() < f32::EPSILON);
    }

    /// total=0 的中间帧 Progress 不得把已知 total 的 Transmission 记录打回 0
    /// （发射侧与存储侧同款防御）。
    #[tokio::test]
    async fn zero_total_progress_emit_keeps_known_total() {
        let (mut state, key) = live_download_state("emit_no_clobber").await;
        let records = attach_capture(&state);
        handle_worker_event(
            WorkerEvent::Progress {
                key: key.clone(),
                next_offset: 512,
                total_size: 4096,
            },
            &mut state,
        )
        .await;
        handle_worker_event(
            WorkerEvent::Progress {
                key: key.clone(),
                next_offset: 600,
                total_size: 0,
            },
            &mut state,
        )
        .await;
        // 600/4096 ≈ 0.146 > 512/4096 = 0.125
        let second = wait_for_record(&records, |r| {
            matches!(r.status(), TransferStatus::Transmission) && r.progress() > 0.13
        })
        .await;
        assert_eq!(second.total_size(), 4096);
    }

    /// 终态拆组后迟到的 Failed 命中 unknown-group 分支：不得 panic、
    /// 不得复活 known_totals 条目。
    #[tokio::test]
    async fn stray_failed_after_teardown_creates_no_orphan_known_total() {
        let (mut state, key) = live_download_state("stray_failed").await;
        handle_worker_event(
            WorkerEvent::Completed {
                key: key.clone(),
                total_size: 4096,
                completion_payload: None,
            },
            &mut state,
        )
        .await;
        handle_worker_event(
            WorkerEvent::Failed {
                key: key.clone(),
                error: MeowError::from_code(InnerErrorCode::Unknown, "late".to_string()),
                total_size: 9999,
            },
            &mut state,
        )
        .await;
        assert!(state.known_totals().get(&key).is_none());
    }
}
