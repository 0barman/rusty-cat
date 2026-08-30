//! Verifies that abort/cleanup failures are surfaced at `WARN` level instead of
//! being silently swallowed, so billing-relevant orphaned multipart/uncommitted
//! block conditions stay observable.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rusty_cat::api::{
    set_debug_log_listener, BreakpointUpload, DebugLogListener, InnerErrorCode, Log, LogLevel,
    MeowClient, MeowConfig, MeowError, TransferStatus, TransferTask, UploadChunkCtx,
    UploadPounceBuilder, UploadPrepareCtx, UploadResumeInfo,
};

type Captured = Arc<Mutex<Vec<(LogLevel, String, String)>>>;

const SECRET: &str = "abort-secret-value";

struct DebugLogListenerReset;

impl Drop for DebugLogListenerReset {
    fn drop(&mut self) {
        let _ = set_debug_log_listener(None);
    }
}

struct SecretAbortUpload {
    chunk_entered: Arc<tokio::sync::Notify>,
    chunk_release: Arc<tokio::sync::Notify>,
    abort_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl BreakpointUpload for SecretAbortUpload {
    async fn prepare(&self, _ctx: UploadPrepareCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(0),
            provider_upload_id: Some("secret-abort-session".to_owned()),
        })
    }

    async fn upload_chunk(&self, ctx: UploadChunkCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        self.chunk_entered.notify_one();
        self.chunk_release.notified().await;
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(ctx.offset + ctx.chunk.len() as u64),
            provider_upload_id: Some("secret-abort-session".to_owned()),
        })
    }

    async fn abort_upload(
        &self,
        _client: &reqwest::Client,
        _task: &TransferTask,
    ) -> Result<(), MeowError> {
        self.abort_calls.fetch_add(1, Ordering::SeqCst);
        Err(MeowError::from_code(
            InnerErrorCode::ResponseStatusError,
            format!(
                "abort failed at https://example.invalid/abort?sig={SECRET}&security-token={SECRET}"
            ),
        ))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abort_failure_warnings_and_error_breadcrumbs_are_secret_safe() {
    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    let listener: DebugLogListener = Arc::new(move |log: Log| {
        sink.lock().unwrap().push((
            log.level(),
            log.tag().to_string(),
            log.message().to_string(),
        ));
    });
    set_debug_log_listener(Some(listener)).expect("set listener");
    let listener_reset = DebugLogListenerReset;

    // Mirrors the executor cancel path: abort failed but cleanup continues.
    rusty_cat::meow_warn_log!(
        "cancel_group",
        "protocol abort failed but continue cleanup: uploadId={}",
        "uid-1"
    );
    rusty_cat::meow_flow_log!("cancel_group", "plain debug breadcrumb {}", 1);

    // Exercise the real active-cancel path. The worker must quiesce before its
    // failing abort hook runs; both the MeowError constructor breadcrumb and
    // the scheduler WARN are captured by the listener above.
    let chunk_entered = Arc::new(tokio::sync::Notify::new());
    let chunk_release = Arc::new(tokio::sync::Notify::new());
    let abort_calls = Arc::new(AtomicUsize::new(0));
    let protocol = Arc::new(SecretAbortUpload {
        chunk_entered: Arc::clone(&chunk_entered),
        chunk_release: Arc::clone(&chunk_release),
        abort_calls: Arc::clone(&abort_calls),
    });
    let terminal = Arc::new(Mutex::new(None));
    let terminal_sink = Arc::clone(&terminal);
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(1)
            .build()
            .expect("abort warning client config"),
    );
    let task = UploadPounceBuilder::from_bytes("secret.bin", vec![1, 2, 3, 4], 2)
        .with_url("https://example.invalid/secret-abort-task")
        .with_breakpoint_upload(protocol)
        .with_max_chunk_retries(0)
        .build()
        .expect("active cancel task");
    let task_id = client
        .try_enqueue(
            task,
            move |record| {
                if matches!(record.status(), TransferStatus::Canceled) {
                    *terminal_sink.lock().expect("terminal status") = Some(record.status().clone());
                }
            },
            |_, _| panic!("canceled upload must not complete"),
        )
        .await
        .expect("enqueue active cancel task");
    tokio::time::timeout(Duration::from_secs(5), chunk_entered.notified())
        .await
        .expect("upload chunk did not enter provider");
    client.cancel(task_id).await.expect("cancel active upload");
    chunk_release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        while terminal.lock().expect("terminal status").is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("canceled status not delivered");
    client.close().await.expect("close abort warning client");
    assert_eq!(abort_calls.load(Ordering::SeqCst), 1);

    drop(listener_reset);

    let logs = captured.lock().unwrap();

    let warn = logs
        .iter()
        .find(|(_, tag, msg)| tag == "cancel_group" && msg.contains("protocol abort failed"))
        .expect("warn entry captured");
    assert_eq!(warn.0, LogLevel::Warn, "abort failure must be WARN level");
    assert!(
        warn.2.contains("uid-1"),
        "warn message should carry the provider session id for cleanup"
    );

    let debug = logs
        .iter()
        .find(|(_, _, msg)| msg.contains("plain debug breadcrumb"))
        .expect("debug entry captured");
    assert_eq!(
        debug.0,
        LogLevel::Debug,
        "flow log must remain DEBUG level for contrast"
    );

    assert!(
        logs.iter().all(|(_, _, message)| !message.contains(SECRET)),
        "captured logs leaked the abort secret: {logs:?}"
    );
    let active_cancel_warn = logs
        .iter()
        .find(|(level, tag, message)| {
            *level == LogLevel::Warn
                && tag == "cancel_group"
                && message.contains("protocol abort failed after worker quiesced")
        })
        .expect("real active-cancel abort warning captured");
    assert!(active_cancel_warn.2.contains("sig=REDACTED"));
    assert!(active_cancel_warn.2.contains("security-token=REDACTED"));
}
