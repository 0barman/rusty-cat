use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use rusty_cat::http_breakpoint::UploadResumeInfo;
use rusty_cat::upload_trait::{UploadChunkCtx, UploadPrepareCtx};
use rusty_cat::{
    BreakpointUpload, MeowClient, MeowConfig, MeowError, TaskId, TransferStatus,
    UploadPounceBuilder,
};

fn temp_upload_path(case: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    p.push(format!("rusty_cat_complete_callback_{case}_{ts}.bin"));
    p
}

#[derive(Debug)]
struct StaticPayloadUpload {
    payload: String,
}

#[async_trait]
impl BreakpointUpload for StaticPayloadUpload {
    async fn prepare(&self, ctx: UploadPrepareCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(ctx.local_offset),
        })
    }

    async fn upload_chunk(&self, ctx: UploadChunkCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(ctx.offset + ctx.chunk.len() as u64),
        })
    }

    async fn complete_upload(
        &self,
        _client: &reqwest::Client,
        _task: &rusty_cat::TransferTask,
    ) -> Result<Option<String>, MeowError> {
        Ok(Some(self.payload.clone()))
    }
}

#[tokio::test]
async fn complete_callback_receives_payload_from_upload_protocol() {
    let upload_path = temp_upload_path("payload");
    fs::write(&upload_path, b"payload-upload-data").expect("write upload source fixture");

    let client = MeowClient::new(MeowConfig::new(1, 1));
    let complete_calls: Arc<Mutex<Vec<(TaskId, Option<String>)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));

    let complete_calls_cb = complete_calls.clone();
    let statuses_cb = statuses.clone();
    let payload = "aliyun://bucket/path/file.txt".to_string();
    let task = UploadPounceBuilder::new("payload.bin", &upload_path, 4)
        .with_url("https://placeholder/upload")
        .with_breakpoint_upload(Arc::new(StaticPayloadUpload {
            payload: payload.clone(),
        }))
        .build()
        .expect("build upload task");

    let task_id = client
        .try_enqueue(
            task,
            move |record| {
                statuses_cb
                    .lock()
                    .expect("lock statuses")
                    .push(record.status().clone());
            },
            move |id, data| {
                complete_calls_cb
                    .lock()
                    .expect("lock complete calls")
                    .push((id, data));
            },
        )
        .await
        .expect("enqueue task");

    for _ in 0..100 {
        let is_complete = statuses
            .lock()
            .expect("lock statuses")
            .iter()
            .any(|s| matches!(s, TransferStatus::Complete));
        if is_complete {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let calls = complete_calls.lock().expect("lock complete calls").clone();
    assert_eq!(calls.len(), 1, "complete callback should be fired once");
    assert_eq!(
        calls[0].0, task_id,
        "complete callback task id should match"
    );
    assert_eq!(calls[0].1.as_deref(), Some(payload.as_str()));

    client.close().await.expect("close client");
    let _ = fs::remove_file(&upload_path);
}
