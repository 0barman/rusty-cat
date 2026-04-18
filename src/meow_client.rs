use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use crate::dflt::default_http_client::{default_breakpoint_arcs, DefaultHttpClient};
use crate::error::{InnerErrorCode, MeowError};
use crate::file_transfer_record::FileTransferRecord;
use crate::ids::{GlobalProgressListenerId, TaskId};
use crate::inner::executor::Executor;
use crate::inner::inner_task::InnerTask;
use crate::inner::task_callbacks::{CompleteCb, ProgressCb, TaskCallbacks};
use crate::log::{set_debug_log_listener, DebugLogListener, DebugLogListenerError};
use crate::meow_config::MeowConfig;
use crate::pounce_task::PounceTask;
use crate::transfer_snapshot::TransferSnapshot;

/// Callback type for globally observing task progress events.
///
/// The callback is invoked from runtime worker context. Keep callback logic
/// fast and non-blocking to avoid delaying event processing.
pub type GlobalProgressListener = ProgressCb;

/// Main entry point of the `rusty-cat` SDK.
///
/// `MeowClient` owns runtime state and provides high-level operations:
/// enqueue, pause, resume, cancel, snapshot, and close.
///
/// # Usage pattern
///
/// 1. Create [`MeowConfig`].
/// 2. Construct `MeowClient::new(config)`.
/// 3. Build tasks with upload/download builders.
/// 4. Call [`Self::enqueue`] and store returned [`TaskId`].
/// 5. Control task lifecycle with pause/resume/cancel.
/// 6. Call [`Self::close`] during shutdown.
#[derive(Clone)]
pub struct MeowClient {
    /// Lazily initialized task executor.
    executor: OnceLock<Executor>,
    /// Immutable runtime configuration.
    config: MeowConfig,
    /// Global listeners receiving progress records for all tasks.
    global_progress_listener: Arc<RwLock<Vec<(GlobalProgressListenerId, GlobalProgressListener)>>>,
    /// Global closed flag. Once set to `true`, task control APIs reject calls.
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
    /// Creates a new client with the provided configuration.
    ///
    /// The internal executor is initialized lazily on first task operation.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::{MeowClient, MeowConfig};
    ///
    /// let config = MeowConfig::default();
    /// let client = MeowClient::new(config);
    /// let _ = client;
    /// ```
    pub fn new(config: MeowConfig) -> Self {
        MeowClient {
            executor: Default::default(),
            config,
            global_progress_listener: Arc::new(RwLock::new(Vec::new())),
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Returns a `reqwest::Client` aligned with this client's configuration.
    ///
    /// - If [`MeowConfig::with_http_client`] injected a custom client, this
    ///   returns its clone.
    /// - Otherwise, this builds a new client from `http_timeout` and
    ///   `tcp_keepalive`.
    ///
    /// # Errors
    ///
    /// Returns [`MeowError`] with `HttpClientBuildFailed` when client creation
    /// fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::{MeowClient, MeowConfig};
    ///
    /// let client = MeowClient::new(MeowConfig::default());
    /// let http = client.http_client()?;
    /// let _ = http;
    /// # Ok::<(), rusty_cat::api::MeowError>(())
    /// ```
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

    /// Ensures the client is still open.
    ///
    /// Returns `ClientClosed` if [`Self::close`] was called successfully.
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

    /// Registers a global progress listener for all tasks.
    ///
    /// # Parameters
    ///
    /// - `listener`: Callback receiving [`FileTransferRecord`] updates.
    ///
    /// # Returns
    ///
    /// Returns a listener ID used by
    /// [`Self::unregister_global_progress_listener`].
    ///
    /// # Usage rules
    ///
    /// Keep callback execution short and panic-free. A heavy callback can slow
    /// down global event delivery.
    ///
    /// # Errors
    ///
    /// Returns `LockPoisoned` when listener storage lock is poisoned.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::{MeowClient, MeowConfig};
    ///
    /// let client = MeowClient::new(MeowConfig::default());
    /// let listener_id = client.register_global_progress_listener(|record| {
    ///     println!("task={} progress={:.2}", record.task_id(), record.progress());
    /// })?;
    /// let _ = listener_id;
    /// # Ok::<(), rusty_cat::api::MeowError>(())
    /// ```
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

    /// Unregisters one previously registered global progress listener.
    ///
    /// Returns `Ok(false)` when the ID does not exist.
    ///
    /// # Errors
    ///
    /// Returns `LockPoisoned` when listener storage lock is poisoned.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::{MeowClient, MeowConfig};
    ///
    /// let client = MeowClient::new(MeowConfig::default());
    /// let id = client.register_global_progress_listener(|_| {})?;
    /// let removed = client.unregister_global_progress_listener(id)?;
    /// assert!(removed);
    /// # Ok::<(), rusty_cat::api::MeowError>(())
    /// ```
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

    /// Removes all registered global progress listeners.
    ///
    /// # Errors
    ///
    /// Returns `LockPoisoned` when listener storage lock is poisoned.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::{MeowClient, MeowConfig};
    ///
    /// let client = MeowClient::new(MeowConfig::default());
    /// client.clear_global_listener()?;
    /// # Ok::<(), rusty_cat::api::MeowError>(())
    /// ```
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

    /// Sets or clears the global debug log listener.
    ///
    /// - Pass `Some(listener)` to set/replace.
    /// - Pass `None` to clear.
    ///
    /// This affects all `MeowClient` instances in the current process.
    ///
    /// # Errors
    ///
    /// Returns [`DebugLogListenerError`] when the internal global listener lock
    /// is poisoned.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::sync::Arc;
    /// use rusty_cat::api::{Log, MeowClient, MeowConfig};
    ///
    /// let client = MeowClient::new(MeowConfig::default());
    /// client.set_debug_log_listener(Some(Arc::new(|log: Log| {
    ///     println!("{log}");
    /// })))?;
    ///
    /// // Clear listener when no longer needed.
    /// client.set_debug_log_listener(None)?;
    /// # Ok::<(), rusty_cat::api::DebugLogListenerError>(())
    /// ```
    pub fn set_debug_log_listener(
        &self,
        listener: Option<DebugLogListener>,
    ) -> Result<(), DebugLogListenerError> {
        set_debug_log_listener(listener)
    }
}

impl MeowClient {
    /// Enqueues a transfer task and returns its [`TaskId`].
    ///
    /// The actual upload/download execution is dispatched to an internal
    /// worker system thread. This method only performs lightweight validation
    /// and submission, so it does not block the caller thread waiting for full
    /// transfer completion.
    ///
    /// `enqueue` is also the recovery entrypoint after process restart. If the
    /// application was killed during a previous upload/download, restart your
    /// process and call `enqueue` again to resume that transfer workflow.
    ///
    /// # Parameters
    ///
    /// - `task`: Built by upload/download task builders.
    /// - `progress_cb`: Per-task callback invoked with transfer progress.
    /// - `complete_cb`: Optional callback fired once when task reaches
    ///   [`crate::transfer_status::TransferStatus::Complete`]. The second
    ///   argument is provider-defined payload returned by upload protocol
    ///   `complete_upload`; download tasks usually receive `None`.
    ///
    /// # Usage rules
    ///
    /// - `task` must be non-empty (required path/name/url and valid upload size).
    /// - Callback should be lightweight and non-blocking.
    /// - Store returned task ID for subsequent task control operations.
    /// - `enqueue` is asynchronous task submission, not synchronous transfer.
    /// - For restart recovery, re-enqueue the same logical task (same
    ///   upload/download target and compatible checkpoint context) so the
    ///   runtime can continue from existing local/remote progress.
    ///
    /// # Errors
    ///
    /// Returns:
    /// - `ClientClosed` if the client was closed.
    /// - `ParameterEmpty` if the task is invalid/empty.
    /// - Any runtime initialization or enqueue errors from the executor.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use reqwest::Method;
    /// use rusty_cat::api::{DownloadPounceBuilder, MeowClient, MeowConfig};
    ///
    /// # async fn run() -> Result<(), rusty_cat::api::MeowError> {
    /// let client = MeowClient::new(MeowConfig::default());
    /// let task = DownloadPounceBuilder::new(
    ///     "example.bin",
    ///     "./downloads/example.bin",
    ///     1024 * 1024,
    ///     "https://example.com/example.bin",
    ///     Method::GET,
    /// )
    /// .build();
    ///
    /// let task_id = client
    ///     .enqueue(
    ///         task,
    ///         |record| {
    ///             println!("status={:?} progress={:.2}", record.status(), record.progress());
    ///         },
    ///         Some(|task_id, payload| {
    ///             println!("task {task_id} completed, payload={payload:?}");
    ///         }),
    ///     )
    ///     .await?;
    /// println!("enqueued task: {task_id}");
    /// # Ok(())
    /// # }
    /// ```
    pub async fn enqueue<PCB,CCB>(
        &self,
        task: PounceTask,
        progress_cb: PCB,
        complete_cb: Option<CCB>,
    ) -> Result<TaskId, MeowError>
    where
        PCB: Fn(FileTransferRecord) + Send + Sync + 'static,
        CCB: Fn(TaskId, Option<String>) + Send + Sync + 'static,
    {
        self.ensure_open()?;
        if task.is_empty() {
            crate::meow_flow_log!("enqueue", "reject empty task");
            return Err(MeowError::from_code1(InnerErrorCode::ParameterEmpty));
        }

        crate::meow_flow_log!("enqueue", "task={:?}", task);

        let progress: ProgressCb = Arc::new(progress_cb);
        let complete: Option<CompleteCb> = complete_cb.map(|cb| Arc::new(cb) as CompleteCb);
        let callbacks = TaskCallbacks::new(Some(progress), complete);

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

    /// Pauses a running or pending task by ID.
    ///
    /// This API sends a control command to the internal scheduler worker
    /// thread. It does not execute transfer pause logic on the caller thread.
    ///
    /// # Usage rules
    ///
    /// Call this with a valid task ID returned by [`Self::enqueue`].
    ///
    /// # Errors
    ///
    /// Returns `ClientClosed`, `TaskNotFound`, or state-transition errors.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::{MeowClient, MeowConfig, TaskId};
    ///
    /// # async fn run(task_id: TaskId) -> Result<(), rusty_cat::api::MeowError> {
    /// let client = MeowClient::new(MeowConfig::default());
    /// client.pause(task_id).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn pause(&self, task_id: TaskId) -> Result<(), MeowError> {
        self.ensure_open()?;
        crate::meow_flow_log!("client_api", "pause called: task_id={:?}", task_id);
        self.get_exec()?.pause(task_id).await
    }

    /// Resumes a previously paused task.
    ///
    /// The same [`TaskId`] continues to identify the task after resume.
    /// The resume command is forwarded to the internal scheduler worker
    /// thread, so caller thread is not responsible for running transfer logic.
    ///
    /// # Errors
    ///
    /// Returns `ClientClosed`, `TaskNotFound`, or `InvalidTaskState`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::{MeowClient, MeowConfig, TaskId};
    ///
    /// # async fn run(task_id: TaskId) -> Result<(), rusty_cat::api::MeowError> {
    /// let client = MeowClient::new(MeowConfig::default());
    /// client.resume(task_id).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn resume(&self, task_id: TaskId) -> Result<(), MeowError> {
        self.ensure_open()?;
        crate::meow_flow_log!("client_api", "resume called: task_id={:?}", task_id);
        self.get_exec()?.resume(task_id).await
    }

    /// Cancels a task by ID.
    ///
    /// Cancellation is requested through the internal scheduler worker thread.
    /// Transfer cancellation execution happens in background runtime workers.
    ///
    /// # Usage rules
    ///
    /// Cancellation is best-effort; protocol-specific cleanup may run.
    ///
    /// # Errors
    ///
    /// Returns `ClientClosed`, `TaskNotFound`, or runtime cancellation errors.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::{MeowClient, MeowConfig, TaskId};
    ///
    /// # async fn run(task_id: TaskId) -> Result<(), rusty_cat::api::MeowError> {
    /// let client = MeowClient::new(MeowConfig::default());
    /// client.cancel(task_id).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn cancel(&self, task_id: TaskId) -> Result<(), MeowError> {
        self.ensure_open()?;
        crate::meow_flow_log!("client_api", "cancel called: task_id={:?}", task_id);
        self.get_exec()?.cancel(task_id).await
    }

    /// Returns a snapshot of queue and active transfer groups.
    ///
    /// Useful for diagnostics and external monitoring dashboards.
    /// Snapshot collection is coordinated by internal scheduler worker state.
    ///
    /// # Errors
    ///
    /// Returns `ClientClosed`, runtime command delivery errors, or scheduler
    /// snapshot retrieval errors.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::{MeowClient, MeowConfig};
    ///
    /// # async fn run() -> Result<(), rusty_cat::api::MeowError> {
    /// let client = MeowClient::new(MeowConfig::default());
    /// let snap = client.snapshot().await?;
    /// println!("queued={}, active={}", snap.queued_groups, snap.active_groups);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn snapshot(&self) -> Result<TransferSnapshot, MeowError> {
        self.ensure_open()?;
        crate::meow_flow_log!("client_api", "snapshot called");
        self.get_exec()?.snapshot().await
    }

    /// Closes this client and its underlying executor.
    ///
    /// After a successful close:
    /// - New task operations are rejected.
    /// - Existing runtime resources are released.
    ///
    /// # Idempotency
    ///
    /// Calling `close` more than once returns `ClientClosed`.
    ///
    /// # Retry behavior
    ///
    /// If executor close fails, the closed flag is rolled back so caller can
    /// retry close.
    ///
    /// # Errors
    ///
    /// Returns `ClientClosed` when already closed, or underlying executor close
    /// errors when shutdown is not completed.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::{MeowClient, MeowConfig};
    ///
    /// # async fn run() -> Result<(), rusty_cat::api::MeowError> {
    /// let client = MeowClient::new(MeowConfig::default());
    /// client.close().await?;
    /// # Ok(())
    /// # }
    /// ```
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
                // Roll back closed flag so caller can retry close.
                self.closed.store(false, Ordering::SeqCst);
                return Err(e);
            }
            Ok(())
        } else {
            crate::meow_flow_log!("client_api", "close with no executor initialized");
            Ok(())
        }
    }

    /// Returns whether this client is currently closed.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::{MeowClient, MeowConfig};
    ///
    /// # async fn run() {
    /// let client = MeowClient::new(MeowConfig::default());
    /// let _closed = client.is_closed().await;
    /// # }
    /// ```
    pub async fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}
