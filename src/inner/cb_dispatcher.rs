//! 用户回调分发器：把所有任务/全局/完成回调从调度核心路径物理隔离到独立线程。
//!
//! # 背景
//!
//! 调度器在 `worker_loop` 中通过单一 `tokio::select!` 同时处理 `cmd_rx`
//! （控制平面）和 `worker_rx`（事件平面）。在重构前，`emit_status` /
//! `invoke_progress_cb` / `invoke_complete_cb` 直接在该 task 中 **同步**
//! 调用接入方闭包，导致：
//!
//! - 任意一个慢回调会卡住 `worker_rx` 的 drain；
//! - `worker_tx`（bounded mpsc）会被反压，进而堵住每个 worker 协程；
//! - `cmd_rx` 同时也无法被 drain，pause/resume/cancel/snapshot/close 全部
//!   被推迟。
//!
//! # 隔离方案
//!
//! 启动一个 **专用的 `std::thread`**，配套一个 bounded `std::sync::mpsc`
//! 队列（容量见 [`CALLBACK_QUEUE_CAPACITY`]，库内常量、不对外暴露）。
//! `emit_*` 不再直接调闭包，而是把 `(cb, dto)` 投递到该队列；线程串行
//! drain、串行执行用户代码，并用 `catch_unwind` 隔离 panic。
//!
//! ## 对外可观测语义（重要）
//!
//! 一帧 = 一次 `progress_cb(record)` 或一次全局 listener 调用。按 `status`
//! 划分两类，分别采取不同投递策略：
//!
//! | 类别       | 包含 status                                            | 业务含义           | 投递策略                         |
//! |------------|--------------------------------------------------------|--------------------|----------------------------------|
//! | 终态相关帧 | `Pending` / `Paused` / `Complete` / `Failed` / `Canceled` | 任务生命周期变化   | 阻塞 `send`，**永远不丢**        |
//! | 中间帧     | `Transmission`                                         | 仅是进度数值的滚动 | `try_send`，**队列满则丢弃**     |
//!
//! 关键约束（接入方据此即可推理）：
//!
//! 1. **被丢的"中间帧"永远只是 `Transmission`**：所有 `Pending` / `Paused`
//!    / `Complete` / `Failed` / `Canceled` 帧都不会少；完成回调
//!    `complete_cb` 也保证送达。
//! 2. **被丢的进度数据"不会回头"**：被丢的旧 `progress` 数值会被更新一帧的
//!    `Transmission` 自然覆盖；最新进度在接入方视角下始终单调向前。
//! 3. **顺序保证**：分发线程严格单线程 FIFO，不会出现乱序到达。
//! 4. **业务最终一致性不变**：100% 完成时的 `Complete` 帧一定能拿到，
//!    `progress` 字段也一定是 1.0。
//!
//! ## 何时会真的丢帧
//!
//! 只在"事件产出速率持续 > 接入方回调消费速率"且 [`CALLBACK_QUEUE_CAPACITY`]
//! 已被填满时，下一帧的 `Transmission` 才会被 `try_send` 丢掉（终态帧仍
//! 阻塞投递）。一般场景（回调只是更新 UI / 写日志 / 累加计数）几乎不会触发。
//!
//! 如果接入方有"必须为每一帧 `Transmission` 做一次副作用"的强诉求，应改为
//! 基于终态做一次，或自己定时轮询 —— 旧实现中这种用法也会拖死调度器，本就
//! 不应当依赖。
//!
//! ## 关闭语义
//!
//! - 只要所有 [`CbSubmit`] 句柄被 drop，channel 就会关闭，分发线程在 drain
//!   完所有已入队 job 后退出。
//! - [`CbDispatcherJoin`] 在 drop 时 `join` 线程，确保不泄漏。
//! - `worker_loop` 的 `Close` 路径会在回应 `respond_to` 前显式 drop 句柄并
//!   `join` 线程，从而保证 `close().await` 返回时所有终态回调已执行完毕。

use std::cell::Cell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::error::{InnerErrorCode, MeowError};
use crate::file_transfer_record::FileTransferRecord;
use crate::ids::TaskId;
use crate::inner::task_callbacks::{CompleteCb, ProgressCb};
use crate::transfer_status::TransferStatus;

/// 回调分发队列容量（库内常量、不对外暴露）。
///
/// 选择 `2048` 作为默认值，是基于以下权衡：
///
/// - 容量越小 → `Transmission` 中间帧越容易被丢，调度器越不会被回调延时拖累；
/// - 容量越大 → 中间帧丢失更少，但回调慢时内存峰值越高、终态延迟越大；
/// - `2048` 在常见 chunk 规模 + 接入方"轻量回调"假设下基本不会触发丢帧路径，
///   同时在回调被异常拖慢时也能在 O(MB) 级别内自然反压。
///
/// 该值有意不通过 `MeowConfig` 暴露：
///
/// - 它属于"反压策略调参"，与对外行为契约无关（行为契约见模块文档"对外可
///   观测语义"小节，固定不变）；
/// - 暴露后接入方很容易把它调到极大或极小，反而破坏隔离效果或语义预期。
///
/// 如确有必要调整，应在库内通过测试论证后修改本常量，而不是放给接入方。
pub(crate) const CALLBACK_QUEUE_CAPACITY: usize = 2048;

std::thread_local! {
    /// Identifies the client that owns the current callback dispatcher. A
    /// callback may close an independent client, but closing its own owner
    /// would wait for this same thread to return and therefore self-deadlock.
    static CALLBACK_DISPATCHER_OWNER: Cell<Option<*const CallbackDispatcherOwnerMarker>> =
        const { Cell::new(None) };
}

#[derive(Clone)]
pub(crate) struct CallbackDispatcherOwner(Arc<CallbackDispatcherOwnerMarker>);

struct CallbackDispatcherOwnerMarker;

impl CallbackDispatcherOwner {
    pub(crate) fn new() -> Self {
        Self(Arc::new(CallbackDispatcherOwnerMarker))
    }
}

pub(crate) fn is_callback_dispatcher_thread_for(owner: &CallbackDispatcherOwner) -> bool {
    let expected = Arc::as_ptr(&owner.0);
    CALLBACK_DISPATCHER_OWNER.with(|current| current.get() == Some(expected))
}

struct CallbackDispatcherThreadMarker;

impl CallbackDispatcherThreadMarker {
    fn enter(owner: &CallbackDispatcherOwner) -> Self {
        CALLBACK_DISPATCHER_OWNER.with(|current| {
            debug_assert!(
                current.get().is_none(),
                "callback dispatcher marker entered twice"
            );
            current.set(Some(Arc::as_ptr(&owner.0)));
        });
        Self
    }
}

impl Drop for CallbackDispatcherThreadMarker {
    fn drop(&mut self) {
        CALLBACK_DISPATCHER_OWNER.with(|current| current.set(None));
    }
}

/// 投递给分发线程的回调任务。
///
/// 闭包 `Arc<dyn Fn>` 自身已经 `Send + Sync`，可安全跨线程移动。
pub(crate) enum CbJob {
    /// 进度类回调（task 自带 progress_cb 或全局 listener 的一次调用）。
    Progress {
        cb: ProgressCb,
        dto: FileTransferRecord,
    },
    /// 完成回调（task 自带 complete_cb 的一次调用）。
    Complete {
        cb: CompleteCb,
        task_id: TaskId,
        payload: Option<String>,
    },
}

/// 投递句柄：调度路径只持有该类型，不直接接触线程或闭包执行。
///
/// 通过 [`SyncSender`] 的 `clone` 语义可以多处共享，但当前实现里只在
/// [`crate::inner::scheduler_state::SchedulerState`] 内持有一份；新增 clone
/// 时务必保证生命周期被精确管理，否则会延迟 channel 关闭和分发线程退出。
#[derive(Clone)]
pub(crate) struct CbSubmit {
    tx: SyncSender<CbJob>,
}

impl CbSubmit {
    /// 投递一次进度回调。
    ///
    /// 接入方语义保证（详见模块文档"对外可观测语义"小节）：
    ///
    /// - **`Transmission` 中间帧**：视为"采样型"。队列满时直接丢弃以避免
    ///   反压调度循环；丢帧不会造成"进度回退"，因为更新的一帧会自然覆盖
    ///   被丢的旧值。
    /// - **其余 status（`Pending` / `Paused` / `Complete` / `Failed`
    ///   / `Canceled`）**：视为终态相关帧，必须保证送达，使用阻塞 `send`。
    ///
    /// 当 channel 已关闭（分发线程已退出）时，仅以 debug 日志记录后吞掉
    /// 错误：这种情况只可能出现在 `close()` 之后或调度器异常退出阶段，此时
    /// 已无可预期的回调消费者。
    pub(crate) fn submit_progress(&self, cb: ProgressCb, dto: FileTransferRecord) {
        let is_sampling_frame = matches!(dto.status(), TransferStatus::Transmission);
        if is_sampling_frame {
            match self.tx.try_send(CbJob::Progress { cb, dto }) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    crate::meow_trace_log!(
                        "cb_dispatcher",
                        "transmission frame dropped: callback queue full"
                    );
                }
                Err(TrySendError::Disconnected(_)) => {
                    crate::meow_trace_log!(
                        "cb_dispatcher",
                        "transmission frame dropped: dispatcher already shut down"
                    );
                }
            }
            return;
        }
        if let Err(e) = self.tx.send(CbJob::Progress { cb, dto }) {
            crate::meow_warn_log!(
                "cb_dispatcher",
                "terminal progress frame undelivered: dispatcher gone ({e})"
            );
        }
    }

    /// 投递一次完成回调。完成回调始终是终态，必须保证送达，使用阻塞 `send`。
    pub(crate) fn submit_complete(&self, cb: CompleteCb, task_id: TaskId, payload: Option<String>) {
        if let Err(e) = self.tx.send(CbJob::Complete {
            cb,
            task_id,
            payload,
        }) {
            crate::meow_warn_log!(
                "cb_dispatcher",
                "complete callback undelivered: dispatcher gone ({e})"
            );
        }
    }
}

/// 分发线程的 join guard。drop 时阻塞 join，保证不泄漏后台线程。
///
/// 通常由 [`crate::inner::executor::worker_loop`] 持有；在 `Close` 命令处理
/// 路径下会被显式 `take` 并 `join`，让 `close().await` 的同步语义包含"所有
/// 终态回调已被回放"。
pub(crate) struct CbDispatcherJoin {
    handle: Option<JoinHandle<()>>,
}

impl CbDispatcherJoin {
    /// 阻塞等待分发线程退出。重复调用是安全的（第二次直接返回）。
    ///
    /// 为了避免在 close 流程的"夹缝时刻"通过全局 debug 日志监听器影响并发的
    /// listener 单元测试，join 成功路径保持静默（不主动 emit log）；仅当
    /// `join()` 返回 `Err`（分发线程发生 panic）时才 emit 一条 Warn —— 这种
    /// 情况罕见且可以接受。
    pub(crate) fn join(&mut self) {
        if let Some(h) = self.handle.take() {
            if h.join().is_err() {
                crate::meow_warn_log!(
                    "cb_dispatcher",
                    "dispatcher thread panicked (join returned Err)"
                );
            }
        }
    }
}

impl Drop for CbDispatcherJoin {
    fn drop(&mut self) {
        // 如果上层走了显式 join 路径，这里是 no-op；否则作为兜底，避免漏 join。
        self.join();
    }
}

/// 启动分发线程，返回投递句柄与 join guard。
///
/// 队列容量固定为 [`CALLBACK_QUEUE_CAPACITY`]（库内常量），不接受外部参数。
/// 这是有意为之：背压表现与"对外可观测语义"是一对耦合的契约，应该一起在
/// 库内被锁定，避免接入方误调参后破坏隔离效果或语义预期。
#[cfg(test)]
pub(crate) fn start() -> Result<(CbSubmit, CbDispatcherJoin), MeowError> {
    start_for(CallbackDispatcherOwner::new())
}

pub(crate) fn start_for(
    owner: CallbackDispatcherOwner,
) -> Result<(CbSubmit, CbDispatcherJoin), MeowError> {
    let (tx, rx) = sync_channel::<CbJob>(CALLBACK_QUEUE_CAPACITY);
    let handle = std::thread::Builder::new()
        .name("rusty-cat-cb".into())
        .spawn(move || {
            let _thread_marker = CallbackDispatcherThreadMarker::enter(&owner);
            // 注意：这里有意不在线程入口/出口 emit `meow_flow_log!`。
            // 调度器主路径上的 emit 时机由 worker_loop 控制；分发线程的启
            // 动/退出由 OS 决定，时机相对其他 test 不可控。在并发的 debug
            // 日志监听器测试场景下，跨线程不可控时机的 emit 会泄漏到
            // listener 序列断言上。这里只在"业务相关"事件（如回调 panic）
            // 中 emit。
            //
            // sender 全部被 drop 时 recv 返回 Err，dispatcher 自然退出。
            while let Ok(job) = rx.recv() {
                match job {
                    CbJob::Progress { cb, dto } => {
                        let ret = catch_unwind(AssertUnwindSafe(|| cb(dto)));
                        if ret.is_err() {
                            crate::meow_warn_log!(
                                "cb_dispatcher",
                                "progress callback panicked; isolated in dispatcher thread"
                            );
                        }
                    }
                    CbJob::Complete {
                        cb,
                        task_id,
                        payload,
                    } => {
                        let ret = catch_unwind(AssertUnwindSafe(|| cb(task_id, payload)));
                        if ret.is_err() {
                            crate::log::emit_lazy(|| {
                                crate::log::Log::warn(
                                    "cb_dispatcher",
                                    "complete callback panicked; isolated in dispatcher thread",
                                )
                                .with_task_id(task_id.to_string())
                            });
                        }
                    }
                }
            }
        })
        .map_err(|e| {
            crate::log::emit_lazy(|| {
                crate::log::Log::error(
                    "cb_dispatcher",
                    format!("spawn rusty-cat callback dispatcher thread failed: {e}"),
                )
            });
            MeowError::from_source(
                InnerErrorCode::RuntimeCreationFailedError,
                "spawn rusty-cat callback dispatcher thread failed",
                e,
            )
        })?;
    Ok((
        CbSubmit { tx },
        CbDispatcherJoin {
            handle: Some(handle),
        },
    ))
}
