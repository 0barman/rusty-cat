use std::collections::{HashMap, HashSet, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::binary::binary_download_error::{client_closed, task_canceled, task_not_found};
use crate::binary::download::{build_client, download_with_retry};
use crate::binary::{BinaryDownloadConfig, BinaryDownloadOutput, BinaryTask};
use crate::error::{InnerErrorCode, MeowError};
use crate::ids::TaskId;
use crate::meow_config::MeowConfig;

pub(crate) const BINARY_MAX_CONCURRENCY: usize = 2;
pub(crate) const BINARY_MAX_OUTSTANDING_TASKS: usize = 1024;
const BINARY_COMMAND_CAPACITY: usize = BINARY_MAX_OUTSTANDING_TASKS;
const BINARY_CALLBACK_CAPACITY: usize = 2;
const BINARY_STARTUP_TIMEOUT: Duration = Duration::from_secs(2);
const REASON_NONE: u8 = 0;
const REASON_CANCEL: u8 = 1;
const REASON_CLOSE: u8 = 2;
const CLOSE_OPEN: u8 = 0;
const CLOSE_IN_PROGRESS: u8 = 1;
const CLOSE_COMPLETE: u8 = 2;

pub(crate) type BinaryCompleteCb =
    Box<dyn FnOnce(TaskId, Result<BinaryDownloadOutput, MeowError>) + Send + 'static>;

pub(crate) struct BinaryExecutor {
    cmd_tx: mpsc::Sender<Command>,
    worker_join: Mutex<Option<JoinHandle<()>>>,
    registry: Arc<Mutex<HashSet<TaskId>>>,
    outstanding: Arc<OutstandingPool>,
    close_state: AtomicU8,
}

impl std::fmt::Debug for BinaryExecutor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BinaryExecutor")
            .field("outstanding", &self.outstanding.current())
            .finish_non_exhaustive()
    }
}

impl BinaryExecutor {
    pub(crate) fn new(meow_config: &MeowConfig) -> Result<Self, MeowError> {
        Self::new_inner(meow_config, None)
    }

    fn new_inner(
        meow_config: &MeowConfig,
        fault_hooks: Option<Arc<BinaryFaultHooks>>,
    ) -> Result<Self, MeowError> {
        let config = meow_config
            .binary_download_config()
            .cloned()
            .unwrap_or_default();
        config.validate()?;
        let timeout = config
            .request_timeout()
            .unwrap_or_else(|| meow_config.http_timeout());
        let keepalive = config
            .tcp_keepalive()
            .unwrap_or_else(|| meow_config.tcp_keepalive());
        let client = build_client(timeout, keepalive, config.redirect_limit())?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(BINARY_MAX_CONCURRENCY)
            .thread_name("rusty-cat-binary-rt")
            .enable_all()
            .build()
            .map_err(|error| {
                MeowError::from_code(
                    InnerErrorCode::RuntimeCreationFailedError,
                    format!("create binary runtime failed: {error}"),
                )
            })?;

        let (cmd_tx, cmd_rx) = mpsc::channel(BINARY_COMMAND_CAPACITY);
        let (worker_tx, worker_rx) = mpsc::unbounded_channel();
        let (callback_done_tx, callback_done_rx) = mpsc::unbounded_channel();
        let callback_dispatcher =
            start_callback_dispatcher(callback_done_tx.clone(), 0, fault_hooks.as_ref())?;
        let registry = Arc::new(Mutex::new(HashSet::new()));
        let worker_registry = Arc::clone(&registry);
        let body_slots = Arc::new(Semaphore::new(BINARY_MAX_CONCURRENCY));
        let (scheduler_ready_tx, scheduler_ready_rx) = std_mpsc::sync_channel(1);
        let scheduler_hooks = fault_hooks.clone();
        let worker_join = std::thread::Builder::new()
            .name("rusty-cat-binary-scheduler".to_owned())
            .spawn(move || {
                runtime.block_on(run_scheduler(SchedulerResources {
                    cmd_rx,
                    worker_rx,
                    worker_tx,
                    callback_done_rx,
                    callback_done_tx,
                    callback_dispatcher,
                    registry: worker_registry,
                    body_slots,
                    client,
                    config,
                    fault_hooks: scheduler_hooks,
                    scheduler_ready_tx,
                }));
            })
            .map_err(|error| {
                MeowError::from_code(
                    InnerErrorCode::RuntimeCreationFailedError,
                    format!("spawn binary scheduler failed: {error}"),
                )
            })?;

        if let Err(error) = scheduler_ready_rx.recv_timeout(BINARY_STARTUP_TIMEOUT) {
            drop(cmd_tx);
            reap_thread(worker_join, "binary scheduler after startup failure");
            return Err(MeowError::from_code(
                InnerErrorCode::RuntimeCreationFailedError,
                format!("binary scheduler did not become ready: {error}"),
            ));
        }

        Ok(Self {
            cmd_tx,
            worker_join: Mutex::new(Some(worker_join)),
            registry,
            outstanding: Arc::new(OutstandingPool::new(BINARY_MAX_OUTSTANDING_TASKS)),
            close_state: AtomicU8::new(CLOSE_OPEN),
        })
    }

    #[cfg(test)]
    fn new_with_fault_hooks(
        meow_config: &MeowConfig,
        fault_hooks: Arc<BinaryFaultHooks>,
    ) -> Result<Self, MeowError> {
        Self::new_inner(meow_config, Some(fault_hooks))
    }

    pub(crate) fn try_enqueue(
        &self,
        task: BinaryTask,
        parsed_url: reqwest::Url,
        complete_cb: BinaryCompleteCb,
    ) -> Result<TaskId, MeowError> {
        let permit = self.outstanding.try_acquire().ok_or_else(|| {
            MeowError::from_code_str(
                InnerErrorCode::BinaryTaskQueueFull,
                "binary task capacity reached 1024",
            )
        })?;
        let task_id = TaskId::new();
        {
            let mut registry = self.registry.lock().map_err(|error| {
                MeowError::from_code(
                    InnerErrorCode::LockPoisoned,
                    format!("binary task registry lock poisoned: {error}"),
                )
            })?;
            registry.insert(task_id);
        }
        let command = Command::Enqueue(Box::new(PendingTask {
            task_id,
            task,
            parsed_url,
            complete_cb,
            outstanding_permit: permit,
        }));
        if let Err(error) = self.cmd_tx.try_send(command) {
            if let Ok(mut registry) = self.registry.lock() {
                registry.remove(&task_id);
            }
            return Err(MeowError::from_code(
                InnerErrorCode::CommandSendFailed,
                format!("enqueue binary task failed: {error}"),
            ));
        }
        Ok(task_id)
    }

    pub(crate) fn contains_task(&self, task_id: TaskId) -> Result<bool, MeowError> {
        self.registry
            .lock()
            .map(|registry| registry.contains(&task_id))
            .map_err(|error| {
                MeowError::from_code(
                    InnerErrorCode::LockPoisoned,
                    format!("binary task registry lock poisoned: {error}"),
                )
            })
    }

    pub(crate) async fn cancel(&self, task_id: TaskId) -> Result<(), MeowError> {
        let (respond_to, response) = oneshot::channel();
        self.cmd_tx
            .send(Command::Cancel {
                task_id,
                respond_to,
            })
            .await
            .map_err(|error| {
                MeowError::from_code(
                    InnerErrorCode::CommandSendFailed,
                    format!("send binary cancel failed: {error}"),
                )
            })?;
        response.await.map_err(|error| {
            MeowError::from_code(
                InnerErrorCode::CommandResponseFailed,
                format!("receive binary cancel response failed: {error}"),
            )
        })?
    }

    pub(crate) async fn close(&self) -> Result<(), MeowError> {
        match self.close_state.compare_exchange(
            CLOSE_OPEN,
            CLOSE_IN_PROGRESS,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => {}
            Err(CLOSE_COMPLETE) => return Ok(()),
            Err(CLOSE_IN_PROGRESS) => {
                return Err(MeowError::from_code_str(
                    InnerErrorCode::CommandResponseFailed,
                    "binary close is already in progress",
                ));
            }
            Err(_) => {
                return Err(MeowError::from_code_str(
                    InnerErrorCode::Unknown,
                    "binary close state was invalid",
                ));
            }
        }
        let mut close_guard = BinaryCloseGuard::new(&self.close_state);
        let (respond_to, response) = oneshot::channel();
        if let Err(error) = self
            .cmd_tx
            .send(Command::Close {
                respond_to: Some(respond_to),
            })
            .await
        {
            return Err(MeowError::from_code(
                InnerErrorCode::CommandSendFailed,
                format!("send binary close failed: {error}"),
            ));
        }
        let scheduler_response = match response.await {
            Ok(result) => result,
            Err(error) => {
                return Err(MeowError::from_code(
                    InnerErrorCode::CommandResponseFailed,
                    format!("receive binary close response failed: {error}"),
                ));
            }
        };
        let scheduler_result = match scheduler_response {
            BinaryCloseResponse::Retryable(error) => return Err(error),
            BinaryCloseResponse::Complete(result) => result,
        };
        let join_result = self.join_worker().await;
        if join_result.is_ok() {
            close_guard.mark_complete();
        }
        match (scheduler_result, join_result) {
            (Err(error), _) | (_, Err(error)) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        }
    }

    pub(crate) fn is_close_complete(&self) -> bool {
        self.close_state.load(Ordering::SeqCst) == CLOSE_COMPLETE
    }

    async fn join_worker(&self) -> Result<(), MeowError> {
        let handle = self
            .worker_join
            .lock()
            .map_err(|error| {
                MeowError::from_code(
                    InnerErrorCode::LockPoisoned,
                    format!("binary worker join lock poisoned: {error}"),
                )
            })?
            .take();
        let Some(handle) = handle else {
            return Ok(());
        };
        tokio::task::spawn_blocking(move || handle.join())
            .await
            .map_err(|error| {
                MeowError::from_code(
                    InnerErrorCode::Unknown,
                    format!("join binary scheduler task failed: {error}"),
                )
            })?
            .map_err(|_| {
                MeowError::from_code_str(
                    InnerErrorCode::Unknown,
                    "binary scheduler thread panicked",
                )
            })
    }
}

impl Drop for BinaryExecutor {
    fn drop(&mut self) {
        if self.close_state.load(Ordering::SeqCst) != CLOSE_COMPLETE {
            let _ = self.cmd_tx.try_send(Command::Close { respond_to: None });
        }
        let handle = self
            .worker_join
            .lock()
            .ok()
            .and_then(|mut guard| guard.take());
        if let Some(handle) = handle {
            let spawn_result = std::thread::Builder::new()
                .name("rusty-cat-binary-reaper".to_owned())
                .spawn(move || {
                    let _ = handle.join();
                });
            if let Err(error) = spawn_result {
                crate::meow_warn_log!(
                    "binary_drop",
                    "spawn binary reaper failed; scheduler detached: {}",
                    error
                );
            }
        }
    }
}

struct BinaryCloseGuard<'a> {
    state: &'a AtomicU8,
    complete: bool,
}

impl<'a> BinaryCloseGuard<'a> {
    fn new(state: &'a AtomicU8) -> Self {
        Self {
            state,
            complete: false,
        }
    }

    fn mark_complete(&mut self) {
        self.state.store(CLOSE_COMPLETE, Ordering::SeqCst);
        self.complete = true;
    }
}

impl Drop for BinaryCloseGuard<'_> {
    fn drop(&mut self) {
        if !self.complete {
            let _ = self.state.compare_exchange(
                CLOSE_IN_PROGRESS,
                CLOSE_OPEN,
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
        }
    }
}

enum Command {
    Enqueue(Box<PendingTask>),
    Cancel {
        task_id: TaskId,
        respond_to: oneshot::Sender<Result<(), MeowError>>,
    },
    Close {
        respond_to: Option<oneshot::Sender<BinaryCloseResponse>>,
    },
}

enum BinaryCloseResponse {
    Complete(Result<(), MeowError>),
    Retryable(MeowError),
}

struct PendingTask {
    task_id: TaskId,
    task: BinaryTask,
    parsed_url: reqwest::Url,
    complete_cb: BinaryCompleteCb,
    outstanding_permit: OutstandingPermit,
}

struct ActiveTask {
    cancel: CancellationToken,
    reason: Arc<AtomicU8>,
}

struct WorkerEvent {
    pending: PendingTask,
    result: Result<BinaryDownloadOutput, MeowError>,
    reason: Arc<AtomicU8>,
    body_permit: OwnedSemaphorePermit,
}

struct CallbackJob {
    task_id: TaskId,
    complete_cb: Option<BinaryCompleteCb>,
    result: Option<Result<BinaryDownloadOutput, MeowError>>,
    _outstanding_permit: OutstandingPermit,
    _body_permit: Option<OwnedSemaphorePermit>,
}

struct CallbackDispatcher {
    tx: std_mpsc::SyncSender<CallbackJob>,
    join: JoinHandle<()>,
    generation: usize,
}

#[derive(Default)]
struct BinaryFaultHooks {
    dispatcher_start_failures: AtomicUsize,
    dispatcher_replacement_failures: AtomicUsize,
    dispatcher_exit_after_jobs: AtomicUsize,
    dispatcher_ready_failures: AtomicUsize,
    scheduler_ready_failures: AtomicUsize,
    dispatcher_starts: AtomicUsize,
}

impl BinaryFaultHooks {
    fn take(counter: &AtomicUsize) -> bool {
        counter
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                if value > 0 {
                    Some(value - 1)
                } else {
                    None
                }
            })
            .is_ok()
    }

    fn take_dispatcher_start_failure(&self, generation: usize) -> bool {
        if generation == 0 {
            Self::take(&self.dispatcher_start_failures)
        } else {
            Self::take(&self.dispatcher_replacement_failures)
        }
    }

    fn take_dispatcher_exit_after_job(&self) -> bool {
        Self::take(&self.dispatcher_exit_after_jobs)
    }

    fn take_dispatcher_ready_failure(&self) -> bool {
        Self::take(&self.dispatcher_ready_failures)
    }

    fn take_scheduler_ready_failure(&self) -> bool {
        Self::take(&self.scheduler_ready_failures)
    }
}

struct SchedulerResources {
    cmd_rx: mpsc::Receiver<Command>,
    worker_rx: mpsc::UnboundedReceiver<WorkerEvent>,
    worker_tx: mpsc::UnboundedSender<WorkerEvent>,
    callback_done_rx: mpsc::UnboundedReceiver<TaskId>,
    callback_done_tx: mpsc::UnboundedSender<TaskId>,
    callback_dispatcher: CallbackDispatcher,
    registry: Arc<Mutex<HashSet<TaskId>>>,
    body_slots: Arc<Semaphore>,
    client: reqwest::Client,
    config: BinaryDownloadConfig,
    fault_hooks: Option<Arc<BinaryFaultHooks>>,
    scheduler_ready_tx: std_mpsc::SyncSender<()>,
}

impl CallbackJob {
    fn run(mut self) {
        let callback = self.complete_cb.take();
        let result = self.result.take();
        if let (Some(callback), Some(result)) = (callback, result) {
            if catch_unwind(AssertUnwindSafe(|| callback(self.task_id, result))).is_err() {
                crate::meow_warn_log!("binary_callback", "binary complete callback panicked");
            }
        }
    }
}

async fn run_scheduler(resources: SchedulerResources) {
    let SchedulerResources {
        mut cmd_rx,
        mut worker_rx,
        worker_tx,
        mut callback_done_rx,
        callback_done_tx,
        callback_dispatcher,
        registry,
        body_slots,
        client,
        config,
        fault_hooks,
        scheduler_ready_tx,
    } = resources;
    if fault_hooks
        .as_ref()
        .is_some_and(|hooks| hooks.take_scheduler_ready_failure())
        || scheduler_ready_tx.send(()).is_err()
    {
        return;
    }
    let mut pending = VecDeque::new();
    let mut active: HashMap<TaskId, ActiveTask> = HashMap::new();
    let mut terminal_jobs = VecDeque::new();
    let mut callback_inflight = 0usize;
    let mut dispatcher = Some(callback_dispatcher);
    let mut next_dispatcher_generation = 1usize;
    let mut dispatcher_restart_requested = false;
    let mut dispatcher_error: Option<String> = None;
    let mut terminal_failure = false;
    let mut closing = false;
    let mut cmd_closed = false;
    let mut close_responder: Option<oneshot::Sender<BinaryCloseResponse>> = None;

    loop {
        drain_callback_acks(&mut callback_done_rx, &mut callback_inflight, &registry);

        if dispatcher
            .as_ref()
            .is_some_and(|current| current.join.is_finished())
        {
            if let Some(stopped) = dispatcher.take() {
                next_dispatcher_generation = stopped.generation.saturating_add(1);
                drop(stopped.tx);
                let join_result = stopped.join.join();
                drain_callback_acks(&mut callback_done_rx, &mut callback_inflight, &registry);
                if callback_inflight == 0 {
                    dispatcher_restart_requested = true;
                    if join_result.is_err() {
                        crate::meow_warn_log!(
                            "binary_callback",
                            "binary callback dispatcher panicked between jobs; restarting"
                        );
                    }
                } else {
                    callback_inflight = 0;
                    terminal_failure = true;
                    closing = true;
                    dispatcher_error = Some(
                        "binary callback dispatcher stopped with an unacknowledged job".to_owned(),
                    );
                    begin_close(&registry, &mut pending, &active, &mut terminal_jobs);
                }
            }
        }

        if dispatcher.is_none() && dispatcher_restart_requested {
            match start_callback_dispatcher(
                callback_done_tx.clone(),
                next_dispatcher_generation,
                fault_hooks.as_ref(),
            ) {
                Ok(replacement) => {
                    dispatcher = Some(replacement);
                    dispatcher_restart_requested = false;
                    dispatcher_error = None;
                    terminal_failure = false;
                }
                Err(error) => {
                    dispatcher_restart_requested = false;
                    dispatcher_error = Some(error.to_string());
                    terminal_failure = true;
                    closing = true;
                    begin_close(&registry, &mut pending, &active, &mut terminal_jobs);
                }
            }
        }

        if let Some(current) = dispatcher.as_ref() {
            if flush_callback_jobs(&current.tx, &mut terminal_jobs, &mut callback_inflight)
                == CallbackFlush::Disconnected
            {
                dispatcher_restart_requested = true;
            }
        }

        if !closing && !terminal_failure {
            launch_pending(
                &mut pending,
                &mut active,
                &worker_tx,
                &client,
                &config,
                &body_slots,
            );
        }

        if closing && dispatcher.is_none() && !dispatcher_restart_requested && active.is_empty() {
            let message = dispatcher_error
                .clone()
                .unwrap_or_else(|| "binary callback dispatcher unavailable".to_owned());
            if let Some(responder) = close_responder.take() {
                let _ = responder.send(BinaryCloseResponse::Retryable(MeowError::from_code(
                    InnerErrorCode::Unknown,
                    message,
                )));
            }
            closing = false;
            if cmd_closed {
                terminal_jobs.clear();
                if let Ok(mut registered) = registry.lock() {
                    registered.clear();
                }
                break;
            }
        }

        if closing
            && dispatcher.is_some()
            && pending.is_empty()
            && active.is_empty()
            && terminal_jobs.is_empty()
            && callback_inflight == 0
        {
            break;
        }

        tokio::select! {
            _ = tokio::task::yield_now(), if dispatcher_restart_requested => {}
            command = cmd_rx.recv(), if !cmd_closed => {
                match command {
                    Some(Command::Enqueue(task)) if !closing && !terminal_failure => {
                        pending.push_back(*task)
                    }
                    Some(Command::Enqueue(task)) => {
                        finish_task(&registry, *task, Err(client_closed()), None, &mut terminal_jobs);
                    }
                    Some(Command::Cancel { task_id, respond_to }) => {
                        let result = cancel_task(
                            task_id,
                            &registry,
                            &mut pending,
                            &active,
                            &mut terminal_jobs,
                        );
                        let _ = respond_to.send(result);
                    }
                    Some(Command::Close { respond_to }) => {
                        if close_responder.is_none() {
                            close_responder = respond_to;
                        }
                        if dispatcher.is_none() {
                            dispatcher_restart_requested = true;
                            dispatcher_error = None;
                        }
                        begin_close(
                            &registry,
                            &mut pending,
                            &active,
                            &mut terminal_jobs,
                        );
                        closing = true;
                    }
                    None => {
                        cmd_closed = true;
                        if dispatcher.is_none() {
                            dispatcher_restart_requested = true;
                        }
                        begin_close(
                            &registry,
                            &mut pending,
                            &active,
                            &mut terminal_jobs,
                        );
                        closing = true;
                    }
                }
            }
            event = worker_rx.recv(), if !active.is_empty() => {
                if let Some(event) = event {
                    active.remove(&event.pending.task_id);
                    let reason = event.reason.load(Ordering::SeqCst);
                    let result = match reason {
                        REASON_CANCEL => Err(task_canceled()),
                        REASON_CLOSE => Err(client_closed()),
                        _ => event.result,
                    };
                    let body_permit = result.is_ok().then_some(event.body_permit);
                    finish_task(
                        &registry,
                        event.pending,
                        result,
                        body_permit,
                        &mut terminal_jobs,
                    );
                }
            }
            done = callback_done_rx.recv(), if callback_inflight > 0 => {
                if let Some(task_id) = done {
                    callback_inflight = callback_inflight.saturating_sub(1);
                    remove_registered_task(&registry, task_id);
                }
            }
        }
    }

    let mut shutdown_error =
        dispatcher_error.map(|message| MeowError::from_code(InnerErrorCode::Unknown, message));
    if let Some(current) = dispatcher.take() {
        drop(current.tx);
        if current.join.join().is_err() {
            crate::meow_error_log!("binary_callback", "binary callback dispatcher panicked");
            shutdown_error.get_or_insert_with(|| {
                MeowError::from_code_str(
                    InnerErrorCode::Unknown,
                    "binary callback dispatcher panicked",
                )
            });
        }
    }
    if let Some(responder) = close_responder {
        let result = shutdown_error.map_or(Ok(()), Err);
        let _ = responder.send(BinaryCloseResponse::Complete(result));
    }
}

fn launch_pending(
    pending: &mut VecDeque<PendingTask>,
    active: &mut HashMap<TaskId, ActiveTask>,
    worker_tx: &mpsc::UnboundedSender<WorkerEvent>,
    client: &reqwest::Client,
    config: &BinaryDownloadConfig,
    body_slots: &Arc<Semaphore>,
) {
    while active.len() < BINARY_MAX_CONCURRENCY {
        let Ok(body_permit) = Arc::clone(body_slots).try_acquire_owned() else {
            break;
        };
        let Some(task) = pending.pop_front() else {
            drop(body_permit);
            break;
        };
        let cancel = CancellationToken::new();
        let reason = Arc::new(AtomicU8::new(REASON_NONE));
        active.insert(
            task.task_id,
            ActiveTask {
                cancel: cancel.clone(),
                reason: Arc::clone(&reason),
            },
        );
        let worker_tx = worker_tx.clone();
        let client = client.clone();
        let config = config.clone();
        let worker_task = task.task.clone();
        let worker_url = task.parsed_url.clone();
        let worker_cancel = cancel.clone();
        tokio::spawn(async move {
            let download_worker = tokio::spawn(async move {
                download_with_retry(&client, &worker_task, &worker_url, &config, &worker_cancel)
                    .await
            });
            let result = map_worker_join_result(download_worker.await);
            let _ = worker_tx.send(WorkerEvent {
                pending: task,
                result,
                reason,
                body_permit,
            });
        });
    }
}

fn cancel_task(
    task_id: TaskId,
    registry: &Arc<Mutex<HashSet<TaskId>>>,
    pending: &mut VecDeque<PendingTask>,
    active: &HashMap<TaskId, ActiveTask>,
    terminal_jobs: &mut VecDeque<CallbackJob>,
) -> Result<(), MeowError> {
    if let Some(position) = pending.iter().position(|task| task.task_id == task_id) {
        if let Some(task) = pending.remove(position) {
            finish_task(registry, task, Err(task_canceled()), None, terminal_jobs);
            return Ok(());
        }
    }
    if let Some(task) = active.get(&task_id) {
        if task
            .reason
            .compare_exchange(
                REASON_NONE,
                REASON_CANCEL,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
        {
            task.cancel.cancel();
            return Ok(());
        }
    }
    Err(task_not_found())
}

fn begin_close(
    registry: &Arc<Mutex<HashSet<TaskId>>>,
    pending: &mut VecDeque<PendingTask>,
    active: &HashMap<TaskId, ActiveTask>,
    terminal_jobs: &mut VecDeque<CallbackJob>,
) {
    while let Some(task) = pending.pop_front() {
        finish_task(registry, task, Err(client_closed()), None, terminal_jobs);
    }
    for task in active.values() {
        task.reason.store(REASON_CLOSE, Ordering::SeqCst);
        task.cancel.cancel();
    }
}

fn finish_task(
    _registry: &Arc<Mutex<HashSet<TaskId>>>,
    task: PendingTask,
    result: Result<BinaryDownloadOutput, MeowError>,
    body_permit: Option<OwnedSemaphorePermit>,
    terminal_jobs: &mut VecDeque<CallbackJob>,
) {
    terminal_jobs.push_back(CallbackJob {
        task_id: task.task_id,
        complete_cb: Some(task.complete_cb),
        result: Some(result),
        _outstanding_permit: task.outstanding_permit,
        _body_permit: body_permit,
    });
}

fn remove_registered_task(registry: &Arc<Mutex<HashSet<TaskId>>>, task_id: TaskId) {
    if let Ok(mut registry) = registry.lock() {
        registry.remove(&task_id);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CallbackFlush {
    Idle,
    Sent,
    Full,
    Disconnected,
}

fn flush_callback_jobs(
    callback_tx: &std_mpsc::SyncSender<CallbackJob>,
    jobs: &mut VecDeque<CallbackJob>,
    inflight: &mut usize,
) -> CallbackFlush {
    if *inflight > 0 {
        return CallbackFlush::Idle;
    }
    let Some(job) = jobs.pop_front() else {
        return CallbackFlush::Idle;
    };
    match callback_tx.try_send(job) {
        Ok(()) => {
            *inflight = 1;
            CallbackFlush::Sent
        }
        Err(std_mpsc::TrySendError::Full(job)) => {
            jobs.push_front(job);
            CallbackFlush::Full
        }
        Err(std_mpsc::TrySendError::Disconnected(job)) => {
            jobs.push_front(job);
            CallbackFlush::Disconnected
        }
    }
}

fn drain_callback_acks(
    done_rx: &mut mpsc::UnboundedReceiver<TaskId>,
    inflight: &mut usize,
    registry: &Arc<Mutex<HashSet<TaskId>>>,
) {
    while let Ok(task_id) = done_rx.try_recv() {
        *inflight = inflight.saturating_sub(1);
        remove_registered_task(registry, task_id);
    }
}

fn map_worker_join_result(
    result: Result<Result<BinaryDownloadOutput, MeowError>, tokio::task::JoinError>,
) -> Result<BinaryDownloadOutput, MeowError> {
    match result {
        Ok(result) => result,
        Err(error) if error.is_panic() => Err(MeowError::from_code_str(
            InnerErrorCode::Unknown,
            "binary download worker panicked",
        )),
        Err(_) => Err(MeowError::from_code_str(
            InnerErrorCode::Unknown,
            "binary download worker was canceled",
        )),
    }
}

fn start_callback_dispatcher(
    done_tx: mpsc::UnboundedSender<TaskId>,
    generation: usize,
    fault_hooks: Option<&Arc<BinaryFaultHooks>>,
) -> Result<CallbackDispatcher, MeowError> {
    if fault_hooks.is_some_and(|hooks| hooks.take_dispatcher_start_failure(generation)) {
        return Err(MeowError::from_code_str(
            InnerErrorCode::RuntimeCreationFailedError,
            "injected binary callback dispatcher start failure",
        ));
    }
    let (callback_tx, callback_rx) =
        std_mpsc::sync_channel::<CallbackJob>(BINARY_CALLBACK_CAPACITY);
    let (ready_tx, ready_rx) = std_mpsc::sync_channel(1);
    let thread_hooks = fault_hooks.cloned();
    let join = std::thread::Builder::new()
        .name("rusty-cat-binary-callback".to_owned())
        .spawn(move || {
            if thread_hooks
                .as_ref()
                .is_some_and(|hooks| hooks.take_dispatcher_ready_failure())
            {
                return;
            }
            if ready_tx.send(()).is_err() {
                return;
            }
            while let Ok(job) = callback_rx.recv() {
                let task_id = job.task_id;
                job.run();
                let exit_after_job = thread_hooks
                    .as_ref()
                    .is_some_and(|hooks| hooks.take_dispatcher_exit_after_job());
                if exit_after_job {
                    // Publish the channel disconnect before acknowledging the
                    // completed callback. Otherwise the scheduler can observe
                    // the ack first, successfully send the next job while the
                    // receiver is still alive, and then lose that queued job
                    // when this dispatcher exits.
                    drop(callback_rx);
                    let _ = done_tx.send(task_id);
                    return;
                }
                let _ = done_tx.send(task_id);
            }
        })
        .map_err(|error| {
            MeowError::from_code(
                InnerErrorCode::RuntimeCreationFailedError,
                format!("spawn binary callback dispatcher failed: {error}"),
            )
        })?;
    if let Err(error) = ready_rx.recv_timeout(BINARY_STARTUP_TIMEOUT) {
        drop(callback_tx);
        reap_thread(join, "binary callback dispatcher after startup failure");
        return Err(MeowError::from_code(
            InnerErrorCode::RuntimeCreationFailedError,
            format!("binary callback dispatcher did not become ready: {error}"),
        ));
    }
    if let Some(hooks) = fault_hooks {
        hooks.dispatcher_starts.fetch_add(1, Ordering::SeqCst);
    }
    Ok(CallbackDispatcher {
        tx: callback_tx,
        join,
        generation,
    })
}

fn reap_thread(handle: JoinHandle<()>, label: &'static str) {
    let spawn_result = std::thread::Builder::new()
        .name("rusty-cat-binary-startup-reaper".to_owned())
        .spawn(move || {
            let _ = handle.join();
        });
    if let Err(error) = spawn_result {
        crate::meow_warn_log!(
            "binary_startup",
            "spawn reaper for {} failed; thread detached: {}",
            label,
            error
        );
    }
}

struct OutstandingPool {
    max: usize,
    current: AtomicUsize,
}

impl OutstandingPool {
    fn new(max: usize) -> Self {
        Self {
            max,
            current: AtomicUsize::new(0),
        }
    }

    fn try_acquire(self: &Arc<Self>) -> Option<OutstandingPermit> {
        let acquired = self
            .current
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                (current < self.max).then_some(current + 1)
            })
            .is_ok();
        acquired.then(|| OutstandingPermit {
            pool: Arc::clone(self),
        })
    }

    fn current(&self) -> usize {
        self.current.load(Ordering::SeqCst)
    }
}

struct OutstandingPermit {
    pool: Arc<OutstandingPool>,
}

impl Drop for OutstandingPermit {
    fn drop(&mut self) {
        self.pool.current.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> MeowConfig {
        MeowConfig::builder()
            .binary_download_config(
                BinaryDownloadConfig::builder()
                    .request_timeout(Duration::from_millis(100))
                    .retry_delays(Vec::new())
                    .build()
                    .expect("binary config"),
            )
            .build()
            .expect("meow config")
    }

    #[test]
    fn outstanding_pool_is_bounded_and_recovers() {
        let pool = Arc::new(OutstandingPool::new(2));
        let first = pool.try_acquire().expect("first");
        let second = pool.try_acquire().expect("second");
        assert!(pool.try_acquire().is_none());
        drop(first);
        assert!(pool.try_acquire().is_some());
        drop(second);
    }

    #[test]
    fn public_outstanding_capacity_accepts_1024_and_rejects_1025th() {
        let pool = Arc::new(OutstandingPool::new(BINARY_MAX_OUTSTANDING_TASKS));
        let permits: Vec<_> = (0..BINARY_MAX_OUTSTANDING_TASKS)
            .map(|_| pool.try_acquire().expect("permit within capacity"))
            .collect();
        assert!(pool.try_acquire().is_none());
        drop(permits);
        assert_eq!(pool.current(), 0);
        assert!(pool.try_acquire().is_some());
    }

    #[tokio::test]
    async fn worker_join_failures_are_terminal_errors_instead_of_lost_events() {
        let panicked = tokio::spawn(async {
            panic!("intentional worker panic");
            #[allow(unreachable_code)]
            Ok::<BinaryDownloadOutput, MeowError>(BinaryDownloadOutput::new(
                bytes::Bytes::new(),
                None,
            ))
        });
        let error = map_worker_join_result(panicked.await).expect_err("panic must be mapped");
        assert_eq!(error.code(), InnerErrorCode::Unknown as i32);

        let canceled = tokio::spawn(async {
            std::future::pending::<()>().await;
            Ok::<BinaryDownloadOutput, MeowError>(BinaryDownloadOutput::new(
                bytes::Bytes::new(),
                None,
            ))
        });
        canceled.abort();
        let error = map_worker_join_result(canceled.await).expect_err("abort must be mapped");
        assert_eq!(error.code(), InnerErrorCode::Unknown as i32);
    }

    #[test]
    fn disconnected_callback_dispatcher_preserves_jobs_for_replacement() {
        let (callback_tx, callback_rx) = std_mpsc::sync_channel(1);
        drop(callback_rx);
        let registry = Arc::new(Mutex::new(HashSet::new()));
        let task_id = TaskId::new();
        registry.lock().unwrap().insert(task_id);
        let pool = Arc::new(OutstandingPool::new(1));
        let permit = pool.try_acquire().expect("outstanding permit");
        let mut jobs = VecDeque::from([CallbackJob {
            task_id,
            complete_cb: Some(Box::new(|_, _| {})),
            result: Some(Err(task_canceled())),
            _outstanding_permit: permit,
            _body_permit: None,
        }]);
        let mut inflight = 0;

        assert_eq!(
            flush_callback_jobs(&callback_tx, &mut jobs, &mut inflight),
            CallbackFlush::Disconnected
        );
        assert_eq!(jobs.len(), 1);
        assert!(registry.lock().unwrap().contains(&task_id));
        assert_eq!(pool.current(), 1);
        assert_eq!(inflight, 0);
        drop(jobs);
        assert_eq!(pool.current(), 0);
    }

    #[tokio::test]
    async fn dispatcher_exit_between_jobs_restarts_without_losing_callbacks() {
        let hooks = Arc::new(BinaryFaultHooks::default());
        hooks.dispatcher_exit_after_jobs.store(2, Ordering::SeqCst);
        let executor = BinaryExecutor::new_with_fault_hooks(&test_config(), Arc::clone(&hooks))
            .expect("executor");
        let (done_tx, mut done_rx) = mpsc::unbounded_channel();
        for index in 0..3 {
            let done_tx = done_tx.clone();
            let task = BinaryTask::new(format!("http://127.0.0.1:1/{index}"));
            let url = reqwest::Url::parse(task.url()).expect("url");
            executor
                .try_enqueue(
                    task,
                    url,
                    Box::new(move |task_id, _| {
                        let _ = done_tx.send(task_id);
                    }),
                )
                .expect("enqueue");
        }
        drop(done_tx);

        let mut completed = HashSet::new();
        while completed.len() < 3 {
            let task_id = tokio::time::timeout(Duration::from_secs(3), done_rx.recv())
                .await
                .expect("callback timeout")
                .expect("callback channel");
            assert!(completed.insert(task_id), "callback must be exactly once");
        }
        assert!(hooks.dispatcher_starts.load(Ordering::SeqCst) >= 3);
        executor.close().await.expect("close after replacement");
    }

    #[tokio::test]
    async fn failed_dispatcher_replacement_keeps_jobs_for_close_retry() {
        let hooks = Arc::new(BinaryFaultHooks::default());
        hooks.dispatcher_exit_after_jobs.store(1, Ordering::SeqCst);
        hooks
            .dispatcher_replacement_failures
            .store(1, Ordering::SeqCst);
        let executor = BinaryExecutor::new_with_fault_hooks(&test_config(), Arc::clone(&hooks))
            .expect("executor");
        let (done_tx, mut done_rx) = mpsc::unbounded_channel();
        for index in 0..3 {
            let done_tx = done_tx.clone();
            let task = BinaryTask::new(format!("http://127.0.0.1:1/retry-{index}"));
            let url = reqwest::Url::parse(task.url()).expect("url");
            executor
                .try_enqueue(
                    task,
                    url,
                    Box::new(move |task_id, _| {
                        let _ = done_tx.send(task_id);
                    }),
                )
                .expect("enqueue");
        }
        drop(done_tx);

        let first_close = executor.close().await;
        assert!(first_close.is_err(), "injected replacement failure");
        assert!(!executor.is_close_complete());
        executor
            .close()
            .await
            .expect("retry close after fault clears");

        let mut completed = HashSet::new();
        while completed.len() < 3 {
            let task_id = tokio::time::timeout(Duration::from_secs(3), done_rx.recv())
                .await
                .expect("callback timeout")
                .expect("callback channel");
            assert!(completed.insert(task_id), "callback must be exactly once");
        }
        assert!(executor.is_close_complete());
    }

    #[test]
    fn scheduler_ready_failure_does_not_publish_a_live_executor() {
        let hooks = Arc::new(BinaryFaultHooks::default());
        hooks.scheduler_ready_failures.store(1, Ordering::SeqCst);
        let error = BinaryExecutor::new_with_fault_hooks(&test_config(), hooks)
            .expect_err("scheduler Ready failure");
        assert_eq!(
            error.code(),
            InnerErrorCode::RuntimeCreationFailedError as i32
        );
        let executor = BinaryExecutor::new(&test_config()).expect("subsequent startup succeeds");
        drop(executor);
    }

    #[test]
    fn dispatcher_start_failure_does_not_publish_a_live_executor() {
        let hooks = Arc::new(BinaryFaultHooks::default());
        hooks.dispatcher_start_failures.store(1, Ordering::SeqCst);
        let error = BinaryExecutor::new_with_fault_hooks(&test_config(), hooks)
            .expect_err("dispatcher start failure");
        assert_eq!(
            error.code(),
            InnerErrorCode::RuntimeCreationFailedError as i32
        );
        let executor = BinaryExecutor::new(&test_config()).expect("subsequent startup succeeds");
        drop(executor);
    }

    #[test]
    fn dispatcher_ready_failure_does_not_publish_a_live_executor() {
        let hooks = Arc::new(BinaryFaultHooks::default());
        hooks.dispatcher_ready_failures.store(1, Ordering::SeqCst);
        let error = BinaryExecutor::new_with_fault_hooks(&test_config(), hooks)
            .expect_err("dispatcher Ready failure");
        assert_eq!(
            error.code(),
            InnerErrorCode::RuntimeCreationFailedError as i32
        );
        let executor = BinaryExecutor::new(&test_config()).expect("subsequent startup succeeds");
        drop(executor);
    }
}
