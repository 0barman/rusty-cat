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
            crate::meow_flow_log!(
                "worker_event",
                "progress: key={:?} next_offset={} total_size={}",
                key,
                next_offset,
                total_size
            );
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
        }
        WorkerEvent::Completed { key, total_size } => {
            crate::meow_flow_log!(
                "worker_event",
                "completed: key={:?} total_size={}",
                key,
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
                crate::inner::exec_impl::emit::emit_status(
                    state,
                    group.entry(),
                    TransferStatus::Complete,
                    total_size,
                    total_size,
                );
            }
            state.offsets_mut().remove(&key);
        }
        WorkerEvent::Failed { key, error } => {
            crate::meow_flow_log!("worker_event", "failed: key={:?} error={}", key, error);
            state.active_mut().remove(&key);
            // 失败终态会结束任务生命周期，因此同步清理 paused 标记。
            state.paused_set_mut().remove(&key);
            if let Some(group) = state.groups_mut().remove(&key) {
                state
                    .task_id_to_dedupe_mut()
                    .remove(&group.leader_inner().task_id());
                let current = state.offsets().get(&key).copied().unwrap_or(0);
                crate::inner::exec_impl::emit::emit_status(
                    state,
                    group.entry(),
                    TransferStatus::Failed(error),
                    current,
                    group.entry().inner().total_size(),
                );
            }
            state.offsets_mut().remove(&key);
        }
        WorkerEvent::Canceled { key } => {
            crate::meow_flow_log!("worker_event", "canceled: key={:?}", key);
            state.active_mut().remove(&key);
            // 若 key 在 paused_set 中，表示该取消来自 pause 流程，仅收敛执行态，不销毁 group。
            if state.paused_set().contains(&key) {
                crate::meow_flow_log!(
                    "worker_event",
                    "canceled from pause flow; keep group for resume: key={:?}",
                    key
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
            }
            state.offsets_mut().remove(&key);
        }
    }
}
