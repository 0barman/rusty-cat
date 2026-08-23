use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use rusty_cat::http_breakpoint::UploadResumeInfo;
use rusty_cat::upload_trait::{UploadChunkCtx, UploadPrepareCtx};
use rusty_cat::{
    BreakpointUpload, MeowClient, MeowConfig, MeowError, TransferStatus, UploadPounceBuilder,
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

#[derive(Default)]
struct State {
    chunks: Vec<(u64, Vec<u8>)>,
    complete_calls: usize,
    abort_calls: usize,
}

struct MutatingUpload {
    state: Arc<Mutex<State>>,
}

#[async_trait]
impl BreakpointUpload for MutatingUpload {
    async fn prepare(&self, _ctx: UploadPrepareCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
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
            .find_map(|s| {
                matches!(
                    s,
                    TransferStatus::Complete | TransferStatus::Failed(_) | TransferStatus::Canceled
                )
                .then(|| s.clone())
            });
        if terminal.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    client.close().await.expect("close");

    let state = state.lock().expect("state");
    assert!(matches!(terminal, Some(TransferStatus::Failed(_))));
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
