use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, RwLock};

use tokio::sync::oneshot;

use crate::dflt::default_http_transfer::{
    build_internal_client, default_breakpoint_arcs, DefaultHttpTransfer,
};
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
use crate::transfer_status::TransferStatus;

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
///
/// # Lifecycle contract: you **must** call [`Self::close`]
///
/// The background scheduler runs on a dedicated [`std::thread`] that drives
/// its own Tokio runtime. The clean shutdown protocol is an explicit
/// `close().await` command which:
///
/// - cancels in-flight transfers,
/// - flushes `Paused` status events to user callbacks for every known group,
/// - drains already submitted callback jobs,
/// - joins the scheduler thread and lets the runtime drop.
///
/// Forgetting to call `close` leaves the scheduler thread alive until all
/// command senders are dropped (which does happen when `MeowClient` is
/// dropped, but only as a fallback). When that fallback path runs, the
/// guarantees above do **not** hold: callers may miss terminal status
/// events, in-flight HTTP transfers are aborted abruptly, and for long-lived
/// SDK hosts (servers, mobile runtimes, etc.) the misuse is nearly
/// impossible to debug from the outside.
///
/// To help surface this misuse the internal executor implements a
/// **best-effort [`Drop`]** that, when `close` was never called:
///
/// - emits a `Warn`-level log via the debug log listener (tag
///   `"executor_drop"`),
/// - performs a non-blocking `try_send` of a final `Close` command so the
///   worker still has a chance to drain its state,
/// - then drops the command sender, causing the worker loop to exit on its
///   own.
///
/// This is a safety net, **not** a substitute for calling `close`. Treat
/// `close().await` as a mandatory step in your shutdown sequence.
///
/// # Sharing across tasks / threads
///
/// `MeowClient` **intentionally does not implement [`Clone`]**.
///
/// The client owns a lazily-initialized [`Executor`] (a single background
/// worker loop plus its task table, scheduler state and shutdown flag). A
/// naive field-by-field `Clone` would copy the `OnceLock<Executor>` *before*
/// it was initialized, letting different clones each spin up their **own**
/// executor on first use. The result would be:
///
/// - multiple independent task tables (tasks enqueued via one clone are
///   invisible to `pause` / `resume` / `cancel` / `snapshot` on another);
/// - concurrency limits ([`MeowConfig::max_upload_concurrency`] /
///   [`MeowConfig::max_download_concurrency`]) silently multiplied by the
///   number of clones;
/// - [`Self::close`] only shutting down one of the worker loops, leaking the
///   rest.
///
/// To share a client across tasks or threads, wrap it in [`std::sync::Arc`]
/// and clone the `Arc` instead:
///
/// ```no_run
/// use std::sync::Arc;
/// use rusty_cat::api::{MeowClient, MeowConfig};
///
/// let client = Arc::new(MeowClient::new(MeowConfig::default()));
/// let client_for_task = Arc::clone(&client);
/// tokio::spawn(async move {
///     let _ = client_for_task; // use the shared client here
/// });
/// ```
/// Outcome of a task that reached [`TransferStatus::Complete`].
///
/// Returned by [`MeowClient::enqueue_and_wait`].
#[derive(Debug, Clone)]
pub struct TaskOutcome {
    /// Task identifier returned by the underlying scheduler.
    pub task_id: TaskId,
    /// Provider-defined payload returned by upload protocol's `complete_upload`.
    /// Download tasks usually receive `None`.
    pub payload: Option<String>,
}

pub struct MeowClient {
    /// Lazily initialized task executor.
    ///
    /// Deliberately **not** wrapped in `Arc`: `MeowClient` is not `Clone`, so
    /// there is exactly one owner of this `OnceLock`. Share the whole client
    /// via `Arc<MeowClient>` when multi-owner access is needed.
    executor: OnceLock<Executor>,
    executor_init: StdMutex<()>,
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
            executor_init: StdMutex::new(()),
            config,
            global_progress_listener: Arc::new(RwLock::new(Vec::new())),
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Returns a `reqwest::Client` aligned with this client's configuration.
    ///
    /// - If [`MeowConfigBuilder::http_client`](crate::api::MeowConfigBuilder::http_client)
    ///   injected a custom client, this
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
        // Build through the shared helper so this client carries the exact same
        // transport policy (connect timeout + idle connection pool) as the
        // transfer backend, rather than reqwest's bare defaults.
        build_internal_client(self.config.http_timeout(), self.config.tcp_keepalive())
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

        let _init_guard = self.executor_init.lock().map_err(|e| {
            MeowError::from_code(
                InnerErrorCode::LockPoisoned,
                format!("executor init lock poisoned: {}", e),
            )
        })?;
        if let Some(exec) = self.executor.get() {
            crate::meow_flow_log!(
                "executor",
                "reuse executor initialized by concurrent caller"
            );
            return Ok(exec);
        }

        let default_http_transfer = DefaultHttpTransfer::try_with_http_timeouts(
            self.config.http_timeout(),
            self.config.tcp_keepalive(),
        )?;
        crate::meow_key_log!(
            "executor",
            "initializing DefaultHttpTransfer (timeout={:?}, tcp_keepalive={:?})",
            self.config.http_timeout(),
            self.config.tcp_keepalive()
        );
        let exec = Executor::new(
            self.config.clone(),
            Arc::new(default_http_transfer),
            self.global_progress_listener.clone(),
        )?;
        self.executor.set(exec).map_err(|_| {
            crate::meow_error_log!(
                "executor",
                "executor init race failed while holding init lock"
            );
            MeowError::from_code_str(
                InnerErrorCode::RuntimeCreationFailedError,
                "executor init race failed",
            )
        })?;
        self.executor.get().ok_or_else(|| {
            crate::meow_error_log!(
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
        let id = GlobalProgressListenerId::new();
        crate::meow_key_log!("listener", "register global listener: id={:?}", id);
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
            crate::meow_key_log!(
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
        crate::meow_key_log!("listener", "clear all global listeners");
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
    /// Submits a transfer task to the internal scheduler and returns its
    /// [`TaskId`].
    ///
    /// The actual upload/download execution is dispatched to an internal
    /// worker system thread. This method only performs lightweight validation
    /// and submission, so it does not block the caller thread waiting for full
    /// transfer completion.
    ///
    /// `try_enqueue` is also the recovery entrypoint after process restart.
    /// If the application was killed during a previous upload/download,
    /// restart your process and call `try_enqueue` again to resume that
    /// transfer workflow.
    ///
    /// # Back-pressure semantics (why the `try_` prefix)
    ///
    /// Internally this method uses
    /// [`tokio::sync::mpsc::Sender::try_send`] to hand the `Enqueue` command
    /// to the scheduler worker, **not** `send().await`. That means:
    ///
    /// - The `await` point in this function is used for task normalization
    ///   (e.g. resolving upload breakpoints, building [`InnerTask`]), **not**
    ///   for waiting on command-queue capacity.
    /// - If the command queue is momentarily full (bursty enqueue under
    ///   [`MeowConfig::command_queue_capacity`]), this method returns an
    ///   immediate `CommandSendFailed` error instead of suspending the
    ///   caller until a slot frees up.
    /// - Other control APIs ([`Self::pause`], [`Self::resume`],
    ///   [`Self::cancel`], [`Self::snapshot`]) use `send().await` and **do**
    ///   wait for queue capacity. Only enqueue is fail-fast.
    ///
    /// Callers that want to batch-enqueue under burst load should either:
    ///
    /// 1. size [`MeowConfig::command_queue_capacity`] appropriately, or
    /// 2. retry on `CommandSendFailed` with their own back-off, or
    /// 3. rate-limit enqueue calls on the caller side.
    ///
    /// The name explicitly carries `try_` so this fail-fast behavior is
    /// visible at the call site. If a fully-awaiting variant is introduced
    /// later it should be named `enqueue` (without the `try_` prefix).
    ///
    /// # Parameters
    ///
    /// - `task`: Built by upload/download task builders.
    /// - `progress_cb`: Per-task callback invoked with transfer progress.
    /// - `complete_cb`: Callback fired once when task reaches
    ///   [`crate::transfer_status::TransferStatus::Complete`]. The second
    ///   argument is provider-defined payload returned by upload protocol
    ///   `complete_upload`; download tasks usually receive `None`.
    ///
    /// # Usage rules
    ///
    /// - `task` must be non-empty (required path/name/url and valid upload size).
    /// - Callback should be lightweight and non-blocking.
    /// - Store returned task ID for subsequent task control operations.
    /// - `try_enqueue` is asynchronous task submission, not synchronous transfer.
    /// - For restart recovery, re-enqueue the same logical task (same
    ///   upload/download target and compatible checkpoint context) so the
    ///   runtime can continue from existing local/remote progress.
    ///
    /// # Errors
    ///
    /// Returns:
    /// - `ClientClosed` if the client was closed.
    /// - `ParameterEmpty` if the task is invalid/empty.
    /// - `CommandSendFailed` if the scheduler command queue is full at the
    ///   moment of submission (see back-pressure semantics above).
    /// - Any runtime initialization errors from the executor.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::{DownloadPounceBuilder, MeowClient, MeowConfig};
    ///
    /// # async fn run() -> Result<(), rusty_cat::api::MeowError> {
    /// let client = MeowClient::new(MeowConfig::default());
    /// let task = DownloadPounceBuilder::new(
    ///     "example.bin",
    ///     "./downloads/example.bin",
    ///     1024 * 1024,
    ///     "https://example.com/example.bin",
    /// )
    /// .build();
    ///
    /// let task_id = client
    ///     .try_enqueue(
    ///         task,
    ///         |record| {
    ///             println!("status={:?} progress={:.2}", record.status(), record.progress());
    ///         },
    ///         |task_id, payload| {
    ///             println!("task {task_id} completed, payload={payload:?}");
    ///         },
    ///     )
    ///     .await?;
    /// println!("enqueued task: {task_id}");
    /// # Ok(())
    /// # }
    /// ```
    pub async fn try_enqueue<PCB, CCB>(
        &self,
        task: PounceTask,
        progress_cb: PCB,
        complete_cb: CCB,
    ) -> Result<TaskId, MeowError>
    where
        PCB: Fn(FileTransferRecord) + Send + Sync + 'static,
        CCB: Fn(TaskId, Option<String>) + Send + Sync + 'static,
    {
        self.ensure_open()?;
        if task.is_empty() {
            crate::meow_warn_log!("try_enqueue", "reject empty task");
            return Err(MeowError::from_code1(InnerErrorCode::ParameterEmpty));
        }

        crate::meow_flow_log!(
            "try_enqueue",
            "task dir={:?} name={:?} size={} chunk={} method={:?} url={}",
            task.direction,
            task.file_name,
            task.total_size,
            task.chunk_size,
            task.method,
            crate::log::sanitize_url(&task.url)
        );

        let progress: ProgressCb = Arc::new(progress_cb);
        let complete: Option<CompleteCb> = Some(Arc::new(complete_cb) as CompleteCb);
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

        let task_id = self.get_exec()?.try_enqueue(inner, callbacks)?;
        crate::meow_key_log!("try_enqueue", "try_enqueue success: task_id={:?}", task_id);
        Ok(task_id)
    }

    /// Imports a transfer task in the **paused** state without scheduling it.
    ///
    /// This is the restart/restore entry point for callers that persist their
    /// own transfer records: rebuild a [`PounceTask`] from your database, import
    /// it here, and the task is registered into the scheduler as
    /// [`TransferStatus::Paused`] **without** queueing, so it performs **zero
    /// network or file I/O** until you explicitly start it.
    ///
    /// To start a previously imported task, call [`Self::resume`] with the
    /// returned [`TaskId`]. A typical "restore N, start a user-selected subset"
    /// flow imports every task with `try_enqueue_paused` and then calls
    /// [`Self::resume`] only for the ids the user chose; the rest stay paused.
    ///
    /// # Difference from [`Self::try_enqueue`]
    ///
    /// - `try_enqueue` schedules immediately (the task becomes `Pending` and may
    ///   start transferring as soon as a concurrency slot is free).
    /// - `try_enqueue_paused` registers the task as `Paused` and never queues it
    ///   until [`Self::resume`] is called.
    ///
    /// Back-pressure is identical: this method uses
    /// [`tokio::sync::mpsc::Sender::try_send`] and fails fast with
    /// `CommandSendFailed` if the command queue is full (see
    /// [`Self::try_enqueue`] for the rationale behind the `try_` prefix).
    ///
    /// # Resume semantics after import
    ///
    /// When the imported task is later resumed, the resume point is recomputed
    /// by the executor, **not** taken from any value passed here:
    ///
    /// - **Download**: resumes from the on-disk partial file length, so the
    ///   partial file must still exist at the task's `file_path`.
    /// - **Upload**: resumes from the server-reported `next_byte` during the
    ///   upload `prepare` stage.
    ///
    /// # Progress reporting while paused
    ///
    /// The single `Paused` [`FileTransferRecord`] emitted on import reports
    /// progress `0.0` because no `prepare` has run yet. Render the imported
    /// task's real progress from your own persisted record; the SDK corrects it
    /// after the first resume.
    ///
    /// # Parameters
    ///
    /// Same as [`Self::try_enqueue`]: a built `task`, a per-task `progress_cb`,
    /// and a `complete_cb` fired once on terminal `Complete`.
    ///
    /// # Errors
    ///
    /// - `ClientClosed` if the client was closed.
    /// - `ParameterEmpty` if the task is invalid/empty.
    /// - `CommandSendFailed` if the scheduler command queue is full.
    /// - Any runtime initialization errors from the executor.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::{DownloadPounceBuilder, MeowClient, MeowConfig};
    ///
    /// # async fn run() -> Result<(), rusty_cat::api::MeowError> {
    /// let client = MeowClient::new(MeowConfig::default());
    /// let task = DownloadPounceBuilder::new(
    ///     "example.bin",
    ///     "./downloads/example.bin",
    ///     1024 * 1024,
    ///     "https://example.com/example.bin",
    /// )
    /// .build();
    ///
    /// // Import without starting it (no HTTP request, no file open).
    /// let task_id = client
    ///     .try_enqueue_paused(task, |_record| {}, |_id, _payload| {})
    ///     .await?;
    ///
    /// // Later, when the user chooses to start this one:
    /// client.resume(task_id).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn try_enqueue_paused<PCB, CCB>(
        &self,
        task: PounceTask,
        progress_cb: PCB,
        complete_cb: CCB,
    ) -> Result<TaskId, MeowError>
    where
        PCB: Fn(FileTransferRecord) + Send + Sync + 'static,
        CCB: Fn(TaskId, Option<String>) + Send + Sync + 'static,
    {
        self.ensure_open()?;
        if task.is_empty() {
            crate::meow_warn_log!("try_enqueue_paused", "reject empty task");
            return Err(MeowError::from_code1(InnerErrorCode::ParameterEmpty));
        }

        crate::meow_flow_log!(
            "try_enqueue_paused",
            "task dir={:?} name={:?} size={} chunk={} method={:?} url={}",
            task.direction,
            task.file_name,
            task.total_size,
            task.chunk_size,
            task.method,
            crate::log::sanitize_url(&task.url)
        );

        let progress: ProgressCb = Arc::new(progress_cb);
        let complete: Option<CompleteCb> = Some(Arc::new(complete_cb) as CompleteCb);
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

        let task_id = self.get_exec()?.try_enqueue_paused(inner, callbacks)?;
        crate::meow_key_log!(
            "try_enqueue_paused",
            "try_enqueue_paused success: task_id={:?}",
            task_id
        );
        Ok(task_id)
    }

    /// Enqueues a task and `await`s until it reaches a terminal status.
    ///
    /// Wraps [`Self::try_enqueue`] with an internal oneshot channel so callers
    /// do not have to write the channel + double-callback + single-send-guard
    /// boilerplate themselves.
    ///
    /// # Returns
    ///
    /// - `Ok(TaskOutcome)` when the task reaches [`TransferStatus::Complete`].
    /// - `Err(MeowError)` carrying the underlying failure for
    ///   [`TransferStatus::Failed`].
    /// - `Err(MeowError)` with code [`InnerErrorCode::TaskCanceled`] for
    ///   [`TransferStatus::Canceled`].
    ///
    /// # Progress
    ///
    /// `progress_cb` receives every [`FileTransferRecord`] update, identical to
    /// the per-task progress callback in [`Self::try_enqueue`].
    ///
    /// # Cancellation / timeout
    ///
    /// Dropping the returned future does **not** cancel the underlying transfer;
    /// the task continues running in the executor. Use [`Self::cancel`] with
    /// the task id (obtainable from `progress_cb`'s `record.task_id()`) to
    /// abort an in-flight transfer.
    ///
    /// To cap wall-clock waiting time, wrap this future:
    ///
    /// ```ignore
    /// let outcome = tokio::time::timeout(
    ///     std::time::Duration::from_secs(60),
    ///     client.enqueue_and_wait(task, |_| {}),
    /// )
    /// .await??;
    /// ```
    ///
    /// # Errors
    ///
    /// In addition to the terminal-status errors above, propagates any error
    /// from [`Self::try_enqueue`] (e.g. `ClientClosed`, `ParameterEmpty`,
    /// `CommandSendFailed`).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::{DownloadPounceBuilder, MeowClient, MeowConfig};
    ///
    /// # async fn run() -> Result<(), rusty_cat::api::MeowError> {
    /// let client = MeowClient::new(MeowConfig::default());
    /// let task = DownloadPounceBuilder::new(
    ///     "example.bin",
    ///     "./downloads/example.bin",
    ///     1024 * 1024,
    ///     "https://example.com/example.bin",
    /// )
    /// .build();
    ///
    /// let outcome = client
    ///     .enqueue_and_wait(task, |record| {
    ///         println!(
    ///             "task={} progress={:.2}",
    ///             record.task_id(),
    ///             record.progress()
    ///         );
    ///     })
    ///     .await?;
    /// println!("task {} complete, payload={:?}", outcome.task_id, outcome.payload);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn enqueue_and_wait<PCB>(
        &self,
        task: PounceTask,
        progress_cb: PCB,
    ) -> Result<TaskOutcome, MeowError>
    where
        PCB: Fn(FileTransferRecord) + Send + Sync + 'static,
    {
        type TerminalMsg = Result<(TaskId, Option<String>), MeowError>;
        let (tx, rx) = oneshot::channel::<TerminalMsg>();
        let tx_slot: Arc<StdMutex<Option<oneshot::Sender<TerminalMsg>>>> =
            Arc::new(StdMutex::new(Some(tx)));
        let progress_slot = Arc::clone(&tx_slot);
        let complete_slot = tx_slot;

        self.try_enqueue(
            task,
            move |record: FileTransferRecord| {
                progress_cb(record.clone());
                match record.status() {
                    TransferStatus::Failed(err) => {
                        send_terminal_once(&progress_slot, Err(err.clone()));
                    }
                    TransferStatus::Canceled => {
                        send_terminal_once(
                            &progress_slot,
                            Err(MeowError::from_code_str(
                                InnerErrorCode::TaskCanceled,
                                "task was canceled",
                            )),
                        );
                    }
                    _ => {}
                }
            },
            move |task_id, payload| {
                send_terminal_once(&complete_slot, Ok((task_id, payload)));
            },
        )
        .await?;

        match rx.await {
            Ok(Ok((task_id, payload))) => Ok(TaskOutcome { task_id, payload }),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(MeowError::from_code_str(
                InnerErrorCode::CommandResponseFailed,
                "transfer terminal channel closed without notification",
            )),
        }
    }

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
        crate::meow_key_log!("client_api", "pause called: task_id={:?}", task_id);
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
        crate::meow_key_log!("client_api", "resume called: task_id={:?}", task_id);
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
        crate::meow_key_log!("client_api", "cancel called: task_id={:?}", task_id);
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
    /// `close` is the terminal lifecycle operation for a `MeowClient`. After
    /// it succeeds, this client stays permanently closed; submit more work by
    /// constructing a new `MeowClient` and enqueueing tasks there.
    ///
    /// After a successful close:
    ///
    /// - New task and control operations on this client are rejected with
    ///   `ClientClosed`.
    /// - All known unfinished task groups (queued, paused, or active) receive
    ///   a `Paused` progress notification through their task callback and all
    ///   registered global listeners.
    /// - In-flight transfers are cancelled and the scheduler state is cleared.
    /// - Already submitted callback jobs are drained before returning.
    /// - The internal scheduler thread is joined, which drops its Tokio
    ///   runtime and releases SDK-owned background execution resources.
    ///
    /// `Paused` is used for shutdown notifications rather than `Canceled` so
    /// callers can recreate a client later and re-enqueue the same logical
    /// transfer when they want to resume from available breakpoint state.
    ///
    /// # Idempotency
    ///
    /// Calling `close` more than once returns `ClientClosed`.
    ///
    /// # Retry behavior
    ///
    /// If executor close fails, the closed flag is rolled back so caller can
    /// retry close. A successful close is not restartable.
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
            crate::meow_key_log!("client_api", "close forwarding to executor");
            if let Err(e) = exec.close().await {
                // Roll back closed flag so caller can retry close.
                self.closed.store(false, Ordering::SeqCst);
                return Err(e);
            }
            Ok(())
        } else {
            crate::meow_key_log!("client_api", "close with no executor initialized");
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
    /// let client = MeowClient::new(MeowConfig::default());
    /// let _closed = client.is_closed();
    /// ```
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

fn send_terminal_once(
    slot: &Arc<StdMutex<Option<oneshot::Sender<Result<(TaskId, Option<String>), MeowError>>>>>,
    msg: Result<(TaskId, Option<String>), MeowError>,
) {
    if let Ok(mut guard) = slot.lock() {
        if let Some(sender) = guard.take() {
            let _ = sender.send(msg);
        }
    }
}
