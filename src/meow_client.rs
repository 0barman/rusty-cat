use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use crate::dflt::default_http_client::{default_breakpoint_arcs, DefaultHttpClient};
use crate::error::{InnerErrorCode, MeowError};
use crate::file_transfer_record::FileTransferRecord;
use crate::ids::{GlobalProgressListenerId, TaskId};
use crate::inner::executor::Executor;
use crate::inner::inner_task::InnerTask;
use crate::inner::task_callbacks::{ProgressCb, TaskCallbacks};
use crate::log::{set_debug_log_listener, DebugLogListener, DebugLogListenerError};
use crate::meow_config::MeowConfig;
use crate::pounce_task::PounceTask;
use crate::transfer_snapshot::TransferSnapshot;

pub type GlobalProgressListener = ProgressCb;

#[derive(Clone)]
pub struct MeowClient {
    executor: OnceLock<Executor>,
    config: MeowConfig,
    global_progress_listener: Arc<RwLock<Vec<(GlobalProgressListenerId, GlobalProgressListener)>>>,
    closed: Arc<AtomicBool>,
}

impl std::fmt::Debug for MeowClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeowClient")
            .field("config", &self.config)
            .field("global_progress_listener", &"..")
            .finish()
    }
}

impl MeowClient {
    pub fn new(config: MeowConfig) -> Self {
        MeowClient {
            executor: Default::default(),
            config,
            global_progress_listener: Arc::new(RwLock::new(Vec::new())),
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 获取与当前 [`MeowClient`] 配置一致的 `reqwest::Client`（可用于复用同一套 timeout/keepalive 配置）。
    ///
    /// - 若 [`MeowConfig::with_http_client`] 曾注入自定义 client，则返回其 clone
    /// - 否则按 `http_timeout/tcp_keepalive` 构建一个新 client
    pub fn http_client(&self) -> Result<reqwest::Client, MeowError> {
        if let Some(c) = self.config.http_client_ref() {
            return Ok(c.clone());
        }
        reqwest::Client::builder()
            .timeout(self.config.http_timeout())
            .tcp_keepalive(self.config.tcp_keepalive())
            .build()
            .map_err(|e| {
                MeowError::from_source(
                    InnerErrorCode::HttpClientBuildFailed,
                    format!(
                        "build reqwest client failed (timeout={:?}, keepalive={:?})",
                        self.config.http_timeout(),
                        self.config.tcp_keepalive()
                    ),
                    e,
                )
            })
    }

    fn get_exec(&self) -> Result<&Executor, MeowError> {
        if let Some(exec) = self.executor.get() {
            crate::meow_flow_log!("executor", "reuse existing executor");
            return Ok(exec);
        }
        let default = DefaultHttpClient::try_with_http_timeouts(
            self.config.http_timeout(),
            self.config.tcp_keepalive(),
        )?;
        crate::meow_flow_log!(
            "executor",
            "initializing default HTTP client (timeout={:?}, tcp_keepalive={:?})",
            self.config.http_timeout(),
            self.config.tcp_keepalive()
        );
        let exec = Executor::new(
            self.config.clone(),
            Arc::new(default),
            self.global_progress_listener.clone(),
        )?;
        let _ = self.executor.set(exec);
        self.executor.get().ok_or_else(|| {
            crate::meow_flow_log!(
                "executor",
                "executor init race failed after set; returning RuntimeCreationFailedError"
            );
            MeowError::from_code_str(
                InnerErrorCode::RuntimeCreationFailedError,
                "executor init race failed",
            )
        })
    }

    fn ensure_open(&self) -> Result<(), MeowError> {
        if self.closed.load(Ordering::SeqCst) {
            crate::meow_flow_log!("client", "ensure_open failed: client already closed");
            Err(MeowError::from_code_str(
                InnerErrorCode::ClientClosed,
                "meow client is already closed",
            ))
        } else {
            Ok(())
        }
    }

    pub fn register_global_progress_listener<F>(
        &self,
        listener: F,
    ) -> Result<GlobalProgressListenerId, MeowError>
    where
        F: Fn(FileTransferRecord) + Send + Sync + 'static,
    {
        let id = GlobalProgressListenerId::new_v4();
        crate::meow_flow_log!("listener", "register global listener: id={:?}", id);
        let mut guard = self.global_progress_listener.write().map_err(|e| {
            MeowError::from_code(
                InnerErrorCode::LockPoisoned,
                format!("register global listener lock poisoned: {}", e),
            )
        })?;
        guard.push((id, Arc::new(listener)));
        Ok(id)
    }

    /// 移除此前注册的一个全局监听；若 `id` 不存在则返回 `false`。
    pub fn unregister_global_progress_listener(
        &self,
        id: GlobalProgressListenerId,
    ) -> Result<bool, MeowError> {
        let mut g = self.global_progress_listener.write().map_err(|e| {
            MeowError::from_code(
                InnerErrorCode::LockPoisoned,
                format!("unregister global listener lock poisoned: {}", e),
            )
        })?;
        if let Some(pos) = g.iter().position(|(k, _)| *k == id) {
            g.remove(pos);
            crate::meow_flow_log!(
                "listener",
                "unregister global listener success: id={:?}",
                id
            );
            Ok(true)
        } else {
            crate::meow_flow_log!("listener", "unregister global listener missed: id={:?}", id);
            Ok(false)
        }
    }

    pub fn clear_global_listener(&self) -> Result<(), MeowError> {
        crate::meow_flow_log!("listener", "clear all global listeners");
        self.global_progress_listener
            .write()
            .map_err(|e| {
                MeowError::from_code(
                    InnerErrorCode::LockPoisoned,
                    format!("clear global listeners lock poisoned: {}", e),
                )
            })?
            .clear();
        Ok(())
    }

    /// 设置或取消流程调试日志监听器（全局生效）。
    ///
    /// - 传入 `Some(listener)`：设置/替换监听器
    /// - 传入 `None`：取消注册监听器
    pub fn set_debug_log_listener(
        &self,
        listener: Option<DebugLogListener>,
    ) -> Result<(), DebugLogListenerError> {
        set_debug_log_listener(listener)
    }
}

impl MeowClient {
    pub async fn enqueue<PCB>(
        &self,
        task: PounceTask,
        progress_cb: PCB,
    ) -> Result<TaskId, MeowError>
    where
        PCB: Fn(FileTransferRecord) + Send + Sync + 'static,
    {
        self.ensure_open()?;
        if task.is_empty() {
            crate::meow_flow_log!("enqueue", "reject empty task");
            return Err(MeowError::from_code1(InnerErrorCode::ParameterEmpty));
        }

        crate::meow_flow_log!("enqueue", "task={:?}", task);

        let progress: ProgressCb = Arc::new(progress_cb);
        let callbacks = TaskCallbacks::new(Some(progress));

        let (def_up, def_down) = default_breakpoint_arcs();
        let inner = InnerTask::from_pounce(
            task,
            self.config.breakpoint_download_http().clone(),
            self.config.http_client_ref().cloned(),
            def_up,
            def_down,
        )
        .await?;

        let task_id = self.get_exec()?.enqueue(inner, callbacks)?;
        crate::meow_flow_log!("enqueue", "enqueue success: task_id={:?}", task_id);
        Ok(task_id)
    }

    // pub async fn get_task_status(&self, task_id: TaskId)-> Result<FileTransferRecord, MeowError> {
    //     todo!(arman) -
    // }

    pub async fn pause(&self, task_id: TaskId) -> Result<(), MeowError> {
        self.ensure_open()?;
        crate::meow_flow_log!("client_api", "pause called: task_id={:?}", task_id);
        self.get_exec()?.pause(task_id).await
    }

    /// 恢复一个此前被 pause 的任务，继续使用同一个 task_id 进行控制。
    pub async fn resume(&self, task_id: TaskId) -> Result<(), MeowError> {
        self.ensure_open()?;
        crate::meow_flow_log!("client_api", "resume called: task_id={:?}", task_id);
        self.get_exec()?.resume(task_id).await
    }

    pub async fn cancel(&self, task_id: TaskId) -> Result<(), MeowError> {
        self.ensure_open()?;
        crate::meow_flow_log!("client_api", "cancel called: task_id={:?}", task_id);
        self.get_exec()?.cancel(task_id).await
    }

    pub async fn snapshot(&self) -> Result<TransferSnapshot, MeowError> {
        self.ensure_open()?;
        crate::meow_flow_log!("client_api", "snapshot called");
        self.get_exec()?.snapshot().await
    }

    pub async fn close(&self) -> Result<(), MeowError> {
        if self
            .closed
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            crate::meow_flow_log!("client_api", "close rejected: already closed");
            return Err(MeowError::from_code_str(
                InnerErrorCode::ClientClosed,
                "meow client is already closed",
            ));
        }
        if let Some(exec) = self.executor.get() {
            crate::meow_flow_log!("client_api", "close forwarding to executor");
            if let Err(e) = exec.close().await {
                // 关闭命令未完成时回滚 closed，允许调用方重试 close。
                self.closed.store(false, Ordering::SeqCst);
                return Err(e);
            }
            Ok(())
        } else {
            crate::meow_flow_log!("client_api", "close with no executor initialized");
            Ok(())
        }
    }

    pub async fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}
