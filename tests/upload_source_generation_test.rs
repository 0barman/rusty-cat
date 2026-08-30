use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use rusty_cat::http_breakpoint::UploadResumeInfo;
use rusty_cat::upload_trait::{UploadChunkCtx, UploadPrepareCtx};
use rusty_cat::{
    BreakpointUpload, InnerErrorCode, MeowClient, MeowConfig, MeowError, TransferStatus,
    UploadPounceBuilder,
};

fn temp_path(case: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "rusty_cat_upload_generation_{case}_{}_{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    path
}

fn replace_visible_path(replacement: &std::path::Path, path: &std::path::Path) {
    #[cfg(windows)]
    std::fs::remove_file(path).expect("remove old visible path");
    std::fs::rename(replacement, path).expect("replace visible path");
}

#[derive(Default)]
struct State {
    prepare_calls: usize,
    chunks: Vec<(u64, Vec<u8>)>,
    complete_calls: usize,
    abort_calls: usize,
}

struct MutatingUpload {
    state: Arc<Mutex<State>>,
}

struct ReplacingWithSameContentUpload {
    state: Arc<Mutex<State>>,
    replacement: PathBuf,
}

struct ParallelMutationUpload {
    state: Arc<Mutex<State>>,
    source: PathBuf,
    slow_started: Arc<AtomicBool>,
    slow_started_notify: Arc<tokio::sync::Notify>,
    slow_finished: Arc<AtomicBool>,
    abort_saw_slow_finished: Arc<AtomicBool>,
}

struct HoldingUpload {
    started: Arc<AtomicBool>,
    started_notify: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

struct PassiveUpload {
    state: Arc<Mutex<State>>,
}

struct EarlyCompleteUpload {
    state: Arc<Mutex<State>>,
    request_started: Arc<AtomicBool>,
    started_notify: Arc<tokio::sync::Notify>,
    release_response: Arc<tokio::sync::Notify>,
}

struct PrepareCompletingMutationUpload {
    state: Arc<Mutex<State>>,
    source: PathBuf,
}

struct RetryablePrepareUpload {
    state: Arc<Mutex<State>>,
    backoff_probe: Arc<tokio::sync::Notify>,
}

struct PanickingUpload {
    state: Arc<Mutex<State>>,
}

#[async_trait]
impl BreakpointUpload for HoldingUpload {
    async fn prepare(&self, _ctx: UploadPrepareCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(0),
            provider_upload_id: Some("holding-generation-test".to_owned()),
        })
    }

    async fn upload_chunk(&self, ctx: UploadChunkCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        self.started.store(true, Ordering::Release);
        self.started_notify.notify_one();
        self.release.notified().await;
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(ctx.offset + ctx.chunk.len() as u64),
            provider_upload_id: Some("holding-generation-test".to_owned()),
        })
    }

    async fn complete_upload(
        &self,
        _client: &reqwest::Client,
        _task: &rusty_cat::TransferTask,
    ) -> Result<Option<String>, MeowError> {
        Ok(Some("held-complete".to_owned()))
    }
}

#[async_trait]
impl BreakpointUpload for PassiveUpload {
    async fn prepare(&self, _ctx: UploadPrepareCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        self.state.lock().expect("state").prepare_calls += 1;
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(0),
            provider_upload_id: Some("passive-generation-test".to_owned()),
        })
    }

    async fn upload_chunk(&self, ctx: UploadChunkCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        self.state
            .lock()
            .expect("state")
            .chunks
            .push((ctx.offset, ctx.chunk.to_vec()));
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(ctx.offset + ctx.chunk.len() as u64),
            provider_upload_id: Some("passive-generation-test".to_owned()),
        })
    }

    async fn complete_upload(
        &self,
        _client: &reqwest::Client,
        _task: &rusty_cat::TransferTask,
    ) -> Result<Option<String>, MeowError> {
        self.state.lock().expect("state").complete_calls += 1;
        Ok(Some("passive-complete".to_owned()))
    }

    async fn abort_upload(
        &self,
        _client: &reqwest::Client,
        _task: &rusty_cat::TransferTask,
    ) -> Result<(), MeowError> {
        self.state.lock().expect("state").abort_calls += 1;
        Ok(())
    }
}

#[async_trait]
impl BreakpointUpload for EarlyCompleteUpload {
    async fn prepare(&self, _ctx: UploadPrepareCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        self.state.lock().expect("state").prepare_calls += 1;
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(0),
            provider_upload_id: Some("early-complete-generation-test".to_owned()),
        })
    }

    async fn upload_chunk(&self, ctx: UploadChunkCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        self.state
            .lock()
            .expect("state")
            .chunks
            .push((ctx.offset, ctx.chunk.to_vec()));
        self.request_started.store(true, Ordering::Release);
        self.started_notify.notify_one();
        self.release_response.notified().await;
        Ok(UploadResumeInfo {
            // The provider deduplicated/finalized the whole object from this
            // first, non-predicted chunk request.
            completed_file_id: Some("remote-object-complete".to_owned()),
            next_byte: Some(ctx.offset + ctx.chunk.len() as u64),
            provider_upload_id: Some("early-complete-generation-test".to_owned()),
        })
    }

    async fn complete_upload(
        &self,
        _client: &reqwest::Client,
        _task: &rusty_cat::TransferTask,
    ) -> Result<Option<String>, MeowError> {
        self.state.lock().expect("state").complete_calls += 1;
        Ok(Some("unexpected-explicit-complete".to_owned()))
    }

    async fn abort_upload(
        &self,
        _client: &reqwest::Client,
        _task: &rusty_cat::TransferTask,
    ) -> Result<(), MeowError> {
        self.state.lock().expect("state").abort_calls += 1;
        Ok(())
    }
}

#[async_trait]
impl BreakpointUpload for PrepareCompletingMutationUpload {
    async fn prepare(&self, ctx: UploadPrepareCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        self.state.lock().expect("state").prepare_calls += 1;
        std::fs::write(&self.source, vec![b'B'; ctx.task.total_size() as usize])
            .expect("mutate source during provider prepare");
        Ok(UploadResumeInfo {
            completed_file_id: Some("provider-says-complete".to_owned()),
            next_byte: Some(ctx.task.total_size()),
            provider_upload_id: Some("prepare-complete-generation-test".to_owned()),
        })
    }

    async fn upload_chunk(&self, ctx: UploadChunkCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        self.state
            .lock()
            .expect("state")
            .chunks
            .push((ctx.offset, ctx.chunk.to_vec()));
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(ctx.offset + ctx.chunk.len() as u64),
            provider_upload_id: Some("prepare-complete-generation-test".to_owned()),
        })
    }

    async fn complete_upload(
        &self,
        _client: &reqwest::Client,
        _task: &rusty_cat::TransferTask,
    ) -> Result<Option<String>, MeowError> {
        self.state.lock().expect("state").complete_calls += 1;
        Ok(Some("unexpected-explicit-complete".to_owned()))
    }

    async fn abort_upload(
        &self,
        _client: &reqwest::Client,
        _task: &rusty_cat::TransferTask,
    ) -> Result<(), MeowError> {
        self.state.lock().expect("state").abort_calls += 1;
        Ok(())
    }

    fn supports_parallel_parts(&self) -> bool {
        true
    }
}

#[async_trait]
impl BreakpointUpload for RetryablePrepareUpload {
    async fn prepare(&self, ctx: UploadPrepareCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        let call = {
            let mut state = self.state.lock().expect("state");
            state.prepare_calls += 1;
            state.prepare_calls
        };
        if call == 1 {
            // The retry backoff is at least 160 ms. Signal well inside that
            // window so the test can issue cancel after the first provider
            // future has returned but before a second call is allowed.
            let probe = Arc::clone(&self.backoff_probe);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                probe.notify_one();
            });
            return Err(MeowError::from_code(
                InnerErrorCode::ResponseStatusError,
                "retryable prepare failure".to_owned(),
            ));
        }
        Ok(UploadResumeInfo {
            completed_file_id: Some("unexpected-retry-complete".to_owned()),
            next_byte: Some(ctx.task.total_size()),
            provider_upload_id: Some("retryable-prepare-generation-test".to_owned()),
        })
    }

    async fn upload_chunk(&self, _ctx: UploadChunkCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        Err(MeowError::from_code_str(
            InnerErrorCode::InvalidTaskState,
            "upload_chunk must not run in prepare-backoff test",
        ))
    }

    async fn abort_upload(
        &self,
        _client: &reqwest::Client,
        _task: &rusty_cat::TransferTask,
    ) -> Result<(), MeowError> {
        self.state.lock().expect("state").abort_calls += 1;
        Ok(())
    }
}

#[async_trait]
impl BreakpointUpload for PanickingUpload {
    async fn prepare(&self, _ctx: UploadPrepareCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        self.state.lock().expect("state").prepare_calls += 1;
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(0),
            provider_upload_id: Some("panicking-generation-test".to_owned()),
        })
    }

    async fn upload_chunk(&self, _ctx: UploadChunkCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        panic!("injected serial upload protocol panic")
    }

    async fn abort_upload(
        &self,
        _client: &reqwest::Client,
        _task: &rusty_cat::TransferTask,
    ) -> Result<(), MeowError> {
        self.state.lock().expect("state").abort_calls += 1;
        Ok(())
    }
}

#[async_trait]
impl BreakpointUpload for MutatingUpload {
    async fn prepare(&self, _ctx: UploadPrepareCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        self.state.lock().expect("state").prepare_calls += 1;
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(0),
            provider_upload_id: Some("generation-test".to_owned()),
        })
    }

    async fn upload_chunk(&self, ctx: UploadChunkCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        self.state
            .lock()
            .expect("state")
            .chunks
            .push((ctx.offset, ctx.chunk.to_vec()));
        if ctx.offset == 1024 {
            std::fs::write(ctx.task.file_path(), vec![b'B'; 2048]).expect("mutate source");
        }
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(ctx.offset + ctx.chunk.len() as u64),
            provider_upload_id: Some("generation-test".to_owned()),
        })
    }

    async fn complete_upload(
        &self,
        _client: &reqwest::Client,
        _task: &rusty_cat::TransferTask,
    ) -> Result<Option<String>, MeowError> {
        self.state.lock().expect("state").complete_calls += 1;
        Ok(Some("completed".to_owned()))
    }

    async fn abort_upload(
        &self,
        _client: &reqwest::Client,
        _task: &rusty_cat::TransferTask,
    ) -> Result<(), MeowError> {
        self.state.lock().expect("state").abort_calls += 1;
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_source_change_fails_before_provider_prepare_and_aborts_once() {
    let blocker_path = temp_path("prepare_blocker");
    let source_path = temp_path("changed_before_prepare");
    std::fs::write(&blocker_path, vec![b'H'; 1024]).expect("blocker fixture");
    std::fs::write(&source_path, vec![b'A'; 2048]).expect("source fixture");

    let blocker_started = Arc::new(AtomicBool::new(false));
    let blocker_started_notify = Arc::new(tokio::sync::Notify::new());
    let blocker_release = Arc::new(tokio::sync::Notify::new());
    let target_state = Arc::new(Mutex::new(State::default()));
    let target_statuses = Arc::new(Mutex::new(Vec::new()));
    let target_statuses_cb = Arc::clone(&target_statuses);
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(1)
            .build()
            .expect("config"),
    );

    let blocker = UploadPounceBuilder::new("blocker.bin", &blocker_path, 1024)
        .with_url("https://placeholder/blocker")
        .with_breakpoint_upload(Arc::new(HoldingUpload {
            started: Arc::clone(&blocker_started),
            started_notify: Arc::clone(&blocker_started_notify),
            release: Arc::clone(&blocker_release),
        }))
        .build()
        .expect("blocker task");
    client
        .try_enqueue(blocker, |_| {}, |_, _| {})
        .await
        .expect("enqueue blocker");
    tokio::time::timeout(Duration::from_secs(5), async {
        while !blocker_started.load(Ordering::Acquire) {
            blocker_started_notify.notified().await;
        }
    })
    .await
    .expect("blocker did not start");

    let target = UploadPounceBuilder::new("changed.bin", &source_path, 1024)
        .with_url("https://placeholder/changed")
        .with_breakpoint_upload(Arc::new(PassiveUpload {
            state: Arc::clone(&target_state),
        }))
        .with_max_chunk_retries(0)
        .build()
        .expect("target task");
    client
        .try_enqueue(
            target,
            move |record| {
                target_statuses_cb
                    .lock()
                    .expect("statuses")
                    .push(record.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("enqueue target");

    // `try_enqueue` has already hashed the original generation, but the single
    // upload slot is still occupied. Change the same visible file before this
    // task can enter provider prepare.
    std::fs::write(&source_path, vec![b'B'; 2048]).expect("replace queued source bytes");
    blocker_release.notify_one();

    let terminal = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(status) = target_statuses
                .lock()
                .expect("statuses")
                .iter()
                .rev()
                .find(|status| {
                    matches!(
                        status,
                        TransferStatus::Complete
                            | TransferStatus::Failed(_)
                            | TransferStatus::Canceled
                    )
                })
                .cloned()
            {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("target did not reach terminal status");
    client.close().await.expect("close");

    assert!(matches!(terminal, TransferStatus::Failed(_)));
    let state = target_state.lock().expect("state");
    assert_eq!(
        state.prepare_calls, 0,
        "changed local bytes must be rejected before provider prepare"
    );
    assert!(state.chunks.is_empty());
    assert_eq!(state.complete_calls, 0);
    assert_eq!(
        state.abort_calls, 1,
        "the potentially resumable provider session must be invalidated once"
    );

    drop(state);
    let _ = std::fs::remove_file(blocker_path);
    let _ = std::fs::remove_file(source_path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_early_completion_wins_cancel_racing_the_in_flight_request() {
    let source_path = temp_path("early_complete_cancel_race");
    std::fs::write(&source_path, vec![b'A'; 3072]).expect("source fixture");
    let state = Arc::new(Mutex::new(State::default()));
    let request_started = Arc::new(AtomicBool::new(false));
    let started_notify = Arc::new(tokio::sync::Notify::new());
    let release_response = Arc::new(tokio::sync::Notify::new());
    let statuses = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = Arc::clone(&statuses);
    let (terminal_tx, terminal_rx) = tokio::sync::oneshot::channel();
    let terminal_tx = Arc::new(Mutex::new(Some(terminal_tx)));
    let terminal_tx_cb = Arc::clone(&terminal_tx);
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(1)
            .build()
            .expect("config"),
    );
    let target = UploadPounceBuilder::new("early.bin", &source_path, 1024)
        .with_url("https://placeholder/early-complete")
        .with_breakpoint_upload(Arc::new(EarlyCompleteUpload {
            state: Arc::clone(&state),
            request_started: Arc::clone(&request_started),
            started_notify: Arc::clone(&started_notify),
            release_response: Arc::clone(&release_response),
        }))
        .with_max_chunk_retries(0)
        .build()
        .expect("target task");
    let task_id = client
        .try_enqueue(
            target,
            move |record| {
                let status = record.status().clone();
                statuses_cb.lock().expect("statuses").push(status.clone());
                if matches!(
                    status,
                    TransferStatus::Complete | TransferStatus::Failed(_) | TransferStatus::Canceled
                ) {
                    if let Some(sender) = terminal_tx_cb.lock().expect("terminal sender").take() {
                        let _ = sender.send(status);
                    }
                }
            },
            |_, _| {},
        )
        .await
        .expect("enqueue target");

    tokio::time::timeout(Duration::from_secs(5), async {
        while !request_started.load(Ordering::Acquire) {
            started_notify.notified().await;
        }
    })
    .await
    .expect("first provider request did not start");
    client
        .cancel(task_id)
        .await
        .expect("cancel remains accepted for the live task");
    release_response.notify_one();

    let terminal = tokio::time::timeout(Duration::from_secs(5), terminal_rx)
        .await
        .expect("target did not reach terminal status")
        .expect("terminal callback sender dropped");
    client.close().await.expect("close");

    assert!(
        matches!(terminal, TransferStatus::Complete),
        "remote terminal acknowledgement must be reported as Complete, got {terminal:?}"
    );
    let state = state.lock().expect("state");
    assert_eq!(
        state.chunks.len(),
        1,
        "provider completed on the first part"
    );
    assert_eq!(state.complete_calls, 0, "no explicit finalize is needed");
    assert_eq!(
        state.abort_calls, 0,
        "an already-complete remote object must never be aborted"
    );

    drop(state);
    let _ = std::fs::remove_file(source_path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_prepare_complete_revalidates_source_before_reporting_complete() {
    let source_path = temp_path("parallel_prepare_complete_mutation");
    std::fs::write(&source_path, vec![b'A'; 2048]).expect("source fixture");
    let state = Arc::new(Mutex::new(State::default()));
    let statuses = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = Arc::clone(&statuses);
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(1)
            .build()
            .expect("config"),
    );
    let target = UploadPounceBuilder::new("prepare-complete.bin", &source_path, 1024)
        .with_url("https://placeholder/parallel-prepare-complete")
        .with_breakpoint_upload(Arc::new(PrepareCompletingMutationUpload {
            state: Arc::clone(&state),
            source: source_path.clone(),
        }))
        .with_max_parts_in_flight(2)
        .with_max_chunk_retries(0)
        .build()
        .expect("target task");
    client
        .try_enqueue(
            target,
            move |record| {
                statuses_cb
                    .lock()
                    .expect("statuses")
                    .push(record.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("enqueue target");

    let terminal = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(status) = statuses
                .lock()
                .expect("statuses")
                .iter()
                .rev()
                .find(|status| {
                    matches!(
                        status,
                        TransferStatus::Complete
                            | TransferStatus::Failed(_)
                            | TransferStatus::Canceled
                    )
                })
                .cloned()
            {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("target did not reach terminal status");
    client.close().await.expect("close");

    assert!(
        matches!(terminal, TransferStatus::Failed(_)),
        "changed source must override the provider fast-path result: {terminal:?}"
    );
    let state = state.lock().expect("state");
    assert_eq!(state.prepare_calls, 1);
    assert!(state.chunks.is_empty(), "no part should be dispatched");
    assert_eq!(state.complete_calls, 0);
    assert_eq!(state.abort_calls, 1);

    drop(state);
    let _ = std::fs::remove_file(source_path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_during_upload_prepare_backoff_prevents_another_provider_call() {
    let source_path = temp_path("cancel_prepare_backoff");
    std::fs::write(&source_path, vec![b'A'; 1024]).expect("source fixture");
    let state = Arc::new(Mutex::new(State::default()));
    let backoff_probe = Arc::new(tokio::sync::Notify::new());
    let statuses = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = Arc::clone(&statuses);
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(1)
            .build()
            .expect("config"),
    );
    let target = UploadPounceBuilder::new("backoff.bin", &source_path, 1024)
        .with_url("https://placeholder/prepare-backoff")
        .with_breakpoint_upload(Arc::new(RetryablePrepareUpload {
            state: Arc::clone(&state),
            backoff_probe: Arc::clone(&backoff_probe),
        }))
        .with_max_upload_prepare_retries(20)
        .build()
        .expect("target task");
    let task_id = client
        .try_enqueue(
            target,
            move |record| {
                statuses_cb
                    .lock()
                    .expect("statuses")
                    .push(record.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("enqueue target");

    tokio::time::timeout(Duration::from_secs(5), backoff_probe.notified())
        .await
        .expect("prepare did not enter its retry backoff");
    client
        .cancel(task_id)
        .await
        .expect("cancel during prepare backoff");

    let terminal = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(status) = statuses
                .lock()
                .expect("statuses")
                .iter()
                .rev()
                .find(|status| {
                    matches!(
                        status,
                        TransferStatus::Complete
                            | TransferStatus::Failed(_)
                            | TransferStatus::Canceled
                    )
                })
                .cloned()
            {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("target did not reach terminal status");
    client.close().await.expect("close");

    assert!(matches!(terminal, TransferStatus::Canceled));
    let state = state.lock().expect("state");
    assert_eq!(
        state.prepare_calls, 1,
        "cancelled backoff must not start another provider prepare"
    );
    assert_eq!(state.complete_calls, 0);
    assert_eq!(state.abort_calls, 1);

    drop(state);
    let _ = std::fs::remove_file(source_path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serial_protocol_panic_aborts_and_releases_source_before_failed_callback() {
    let source_path = temp_path("serial_protocol_panic");
    let moved_path = temp_path("serial_protocol_panic_moved");
    std::fs::write(&source_path, vec![b'A'; 1024]).expect("source fixture");
    let state = Arc::new(Mutex::new(State::default()));
    let callback_result: Arc<Mutex<Option<Result<(), String>>>> = Arc::new(Mutex::new(None));
    let callback_result_sink = Arc::clone(&callback_result);
    let callback_source = source_path.clone();
    let callback_moved = moved_path.clone();
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(1)
            .build()
            .expect("config"),
    );
    let target = UploadPounceBuilder::new("panic.bin", &source_path, 1024)
        .with_url("https://placeholder/serial-protocol-panic")
        .with_breakpoint_upload(Arc::new(PanickingUpload {
            state: Arc::clone(&state),
        }))
        .with_max_chunk_retries(0)
        .build()
        .expect("target task");
    client
        .try_enqueue(
            target,
            move |record| {
                if matches!(record.status(), TransferStatus::Failed(_)) {
                    let result = std::fs::rename(&callback_source, &callback_moved)
                        .map_err(|error| error.to_string());
                    *callback_result_sink.lock().expect("callback result") = Some(result);
                }
            },
            |_, _| {},
        )
        .await
        .expect("enqueue target");

    tokio::time::timeout(Duration::from_secs(5), async {
        while callback_result.lock().expect("callback result").is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Failed callback did not run");
    client.close().await.expect("close");

    let result = callback_result
        .lock()
        .expect("callback result")
        .take()
        .expect("callback result missing");
    assert!(
        result.is_ok(),
        "Failed callback must observe the upload source handle released: {result:?}"
    );
    let state = state.lock().expect("state");
    assert_eq!(state.prepare_calls, 1);
    assert_eq!(state.complete_calls, 0);
    assert_eq!(
        state.abort_calls, 1,
        "panic cleanup must abort the provider session exactly once"
    );

    drop(state);
    let _ = std::fs::remove_file(source_path);
    let _ = std::fs::remove_file(moved_path);
}

#[async_trait]
impl BreakpointUpload for ReplacingWithSameContentUpload {
    async fn prepare(&self, _ctx: UploadPrepareCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(0),
            provider_upload_id: Some("same-content-generation-test".to_owned()),
        })
    }

    async fn upload_chunk(&self, ctx: UploadChunkCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        self.state
            .lock()
            .expect("state")
            .chunks
            .push((ctx.offset, ctx.chunk.to_vec()));
        if ctx.offset == 0 {
            replace_visible_path(&self.replacement, ctx.task.file_path());
        }
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(ctx.offset + ctx.chunk.len() as u64),
            provider_upload_id: Some("same-content-generation-test".to_owned()),
        })
    }

    async fn complete_upload(
        &self,
        _client: &reqwest::Client,
        _task: &rusty_cat::TransferTask,
    ) -> Result<Option<String>, MeowError> {
        self.state.lock().expect("state").complete_calls += 1;
        Ok(Some("completed".to_owned()))
    }

    async fn abort_upload(
        &self,
        _client: &reqwest::Client,
        _task: &rusty_cat::TransferTask,
    ) -> Result<(), MeowError> {
        self.state.lock().expect("state").abort_calls += 1;
        Ok(())
    }
}

#[async_trait]
impl BreakpointUpload for ParallelMutationUpload {
    async fn prepare(&self, _ctx: UploadPrepareCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(0),
            provider_upload_id: Some("parallel-generation-test".to_owned()),
        })
    }

    async fn upload_chunk(&self, ctx: UploadChunkCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        match ctx.offset {
            0 => {
                self.slow_started.store(true, Ordering::Release);
                self.slow_started_notify.notify_waiters();
                tokio::time::sleep(Duration::from_millis(250)).await;
                self.slow_finished.store(true, Ordering::Release);
            }
            1024 => {
                while !self.slow_started.load(Ordering::Acquire) {
                    self.slow_started_notify.notified().await;
                }
                // Both first-window parts have already been read and verified.
                // Mutating now makes the subsequently dispatched third part fail
                // its stored block digest while offset 0 is still in flight.
                std::fs::write(&self.source, vec![b'B'; 3072]).expect("mutate source");
            }
            other => panic!("digest-mismatched part reached provider: offset={other}"),
        }
        self.state
            .lock()
            .expect("state")
            .chunks
            .push((ctx.offset, ctx.chunk.to_vec()));
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(ctx.offset + ctx.chunk.len() as u64),
            provider_upload_id: Some("parallel-generation-test".to_owned()),
        })
    }

    async fn complete_upload(
        &self,
        _client: &reqwest::Client,
        _task: &rusty_cat::TransferTask,
    ) -> Result<Option<String>, MeowError> {
        self.state.lock().expect("state").complete_calls += 1;
        Ok(Some("completed".to_owned()))
    }

    async fn abort_upload(
        &self,
        _client: &reqwest::Client,
        _task: &rusty_cat::TransferTask,
    ) -> Result<(), MeowError> {
        self.abort_saw_slow_finished.store(
            self.slow_finished.load(Ordering::Acquire),
            Ordering::Release,
        );
        self.state.lock().expect("state").abort_calls += 1;
        Ok(())
    }

    fn supports_parallel_parts(&self) -> bool {
        true
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_change_between_last_part_and_complete_aborts_and_never_completes() {
    let path = temp_path("before_complete");
    std::fs::write(&path, vec![b'A'; 2048]).expect("fixture");
    let state = Arc::new(Mutex::new(State::default()));
    let protocol = Arc::new(MutatingUpload {
        state: Arc::clone(&state),
    });
    let statuses = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = Arc::clone(&statuses);
    let (terminal_tx, terminal_rx) = tokio::sync::oneshot::channel();
    let terminal_tx = Arc::new(Mutex::new(Some(terminal_tx)));
    let terminal_tx_cb = Arc::clone(&terminal_tx);
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(1)
            .build()
            .expect("config"),
    );
    let task = UploadPounceBuilder::new("generation.bin", &path, 1024)
        .with_url("https://placeholder/upload")
        .with_breakpoint_upload(protocol)
        .with_max_chunk_retries(0)
        .build()
        .expect("task");

    client
        .try_enqueue(
            task,
            move |record| {
                let status = record.status().clone();
                statuses_cb.lock().expect("statuses").push(status.clone());
                if matches!(
                    status,
                    TransferStatus::Complete | TransferStatus::Failed(_) | TransferStatus::Canceled
                ) {
                    if let Some(sender) = terminal_tx_cb.lock().expect("terminal sender").take() {
                        let _ = sender.send(status);
                    }
                }
            },
            |_, _| {},
        )
        .await
        .expect("enqueue");

    let terminal = tokio::time::timeout(Duration::from_secs(5), terminal_rx)
        .await
        .expect("terminal callback timeout")
        .expect("terminal callback sender dropped");
    client.close().await.expect("close");

    let state = state.lock().expect("state");
    assert!(matches!(terminal, TransferStatus::Failed(_)));
    assert_eq!(
        state.complete_calls, 0,
        "changed source must never complete"
    );
    assert_eq!(state.abort_calls, 1, "source failure aborts exactly once");
    assert_eq!(state.chunks.len(), 2);
    assert!(state
        .chunks
        .iter()
        .all(|(_, bytes)| bytes.iter().all(|byte| *byte == b'A')));

    drop(state);
    let _ = std::fs::remove_file(path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_source_failure_aborts_only_after_all_provider_parts_quiesce() {
    let path = temp_path("parallel_abort_after_drain");
    std::fs::write(&path, vec![b'A'; 3072]).expect("fixture");
    let state = Arc::new(Mutex::new(State::default()));
    let slow_started = Arc::new(AtomicBool::new(false));
    let slow_started_notify = Arc::new(tokio::sync::Notify::new());
    let slow_finished = Arc::new(AtomicBool::new(false));
    let abort_saw_slow_finished = Arc::new(AtomicBool::new(false));
    let protocol = Arc::new(ParallelMutationUpload {
        state: Arc::clone(&state),
        source: path.clone(),
        slow_started: Arc::clone(&slow_started),
        slow_started_notify,
        slow_finished: Arc::clone(&slow_finished),
        abort_saw_slow_finished: Arc::clone(&abort_saw_slow_finished),
    });
    let statuses = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = Arc::clone(&statuses);
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(1)
            .build()
            .expect("config"),
    );
    let task = UploadPounceBuilder::new("parallel-generation.bin", &path, 1024)
        .with_url("https://placeholder/upload")
        .with_breakpoint_upload(protocol)
        .with_max_parts_in_flight(2)
        .with_max_chunk_retries(0)
        .build()
        .expect("task");

    client
        .try_enqueue(
            task,
            move |record| {
                statuses_cb
                    .lock()
                    .expect("statuses")
                    .push(record.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("enqueue");

    let mut terminal = None;
    for _ in 0..500 {
        terminal = statuses
            .lock()
            .expect("statuses")
            .iter()
            .rev()
            .find(|status| {
                matches!(
                    status,
                    TransferStatus::Complete | TransferStatus::Failed(_) | TransferStatus::Canceled
                )
            })
            .cloned();
        if terminal.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    client.close().await.expect("close");

    assert!(matches!(terminal, Some(TransferStatus::Failed(_))));
    assert!(slow_finished.load(Ordering::Acquire));
    assert!(
        abort_saw_slow_finished.load(Ordering::Acquire),
        "provider abort must run only after the slow sibling upload_chunk returned"
    );
    let state = state.lock().expect("state");
    assert_eq!(state.abort_calls, 1, "source failure aborts exactly once");
    assert_eq!(state.complete_calls, 0);
    assert_eq!(
        state.chunks.len(),
        2,
        "third part must fail before provider"
    );

    drop(state);
    let _ = std::fs::remove_file(path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn complete_callback_observes_upload_source_handle_already_released() {
    let path = temp_path("terminal_handle_release");
    let moved = temp_path("terminal_handle_release_moved");
    std::fs::write(&path, vec![b'A'; 1024]).expect("fixture");
    let state = Arc::new(Mutex::new(State::default()));
    let callback_result: Arc<Mutex<Option<Result<(), String>>>> = Arc::new(Mutex::new(None));
    let callback_result_sink = Arc::clone(&callback_result);
    let callback_path = path.clone();
    let callback_moved = moved.clone();
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(1)
            .build()
            .expect("config"),
    );
    let task = UploadPounceBuilder::new("release.bin", &path, 1024)
        .with_url("https://placeholder/upload")
        .with_breakpoint_upload(Arc::new(MutatingUpload {
            state: Arc::clone(&state),
        }))
        .build()
        .expect("task");

    client
        .try_enqueue(
            task,
            move |record| {
                if matches!(record.status(), TransferStatus::Complete) {
                    let result = std::fs::rename(&callback_path, &callback_moved)
                        .map_err(|error| error.to_string());
                    *callback_result_sink.lock().expect("callback result") = Some(result);
                }
            },
            |_, _| {},
        )
        .await
        .expect("enqueue");

    for _ in 0..500 {
        if callback_result.lock().expect("callback result").is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    client.close().await.expect("close");
    let result = callback_result
        .lock()
        .expect("callback result")
        .take()
        .expect("Complete callback did not run");
    assert!(
        result.is_ok(),
        "Complete callback must be able to rename the upload source immediately: {result:?}"
    );
    assert!(moved.exists());

    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(moved);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_content_path_replacement_keeps_upload_running_on_its_stable_handle() {
    let path = temp_path("same_content_replace");
    let replacement = temp_path("same_content_replacement");
    let original: Vec<u8> = (0..=255).cycle().take(3072).collect();
    std::fs::write(&path, &original).expect("fixture");
    std::fs::write(&replacement, &original).expect("identical replacement");
    let state = Arc::new(Mutex::new(State::default()));
    let protocol = Arc::new(ReplacingWithSameContentUpload {
        state: Arc::clone(&state),
        replacement: replacement.clone(),
    });
    let statuses = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = Arc::clone(&statuses);
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(1)
            .build()
            .expect("config"),
    );
    let task = UploadPounceBuilder::new("same-content.bin", &path, 1024)
        .with_url("https://placeholder/upload")
        .with_breakpoint_upload(protocol)
        .with_max_chunk_retries(0)
        .build()
        .expect("task");

    client
        .try_enqueue(
            task,
            move |record| {
                statuses_cb
                    .lock()
                    .expect("statuses")
                    .push(record.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("enqueue");

    let mut terminal = None;
    for _ in 0..500 {
        terminal = statuses
            .lock()
            .expect("statuses")
            .iter()
            .rev()
            .find_map(|status| {
                matches!(
                    status,
                    TransferStatus::Complete | TransferStatus::Failed(_) | TransferStatus::Canceled
                )
                .then(|| status.clone())
            });
        if terminal.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    client.close().await.expect("close");

    let state = state.lock().expect("state");
    assert!(
        matches!(terminal, Some(TransferStatus::Complete)),
        "same bytes must preserve content identity, got {terminal:?}"
    );
    assert_eq!(state.complete_calls, 1);
    assert_eq!(state.abort_calls, 0);
    assert_eq!(state.chunks.len(), 3);
    for (offset, bytes) in &state.chunks {
        let start = *offset as usize;
        assert_eq!(&bytes[..], &original[start..start + bytes.len()]);
    }

    drop(state);
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(replacement);
}
