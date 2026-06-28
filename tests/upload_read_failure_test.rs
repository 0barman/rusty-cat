//! 故意异常用例 — 上传读分片路径（commit 71402a2: 改为读入 `BytesMut` 后 `freeze`）。
//!
//! 71402a2 把每个分片的磁盘读取从「复用 Vec + copy_from_slice」改为「每分片 `BytesMut` +
//! `read_exact(&mut buf[..])` + `freeze()`」。本文件验证该读取路径在**源文件被截断**
//! （`total_size` 在 build 时已按文件大小定格，运行期文件变短）时的失败行为：
//!   - `read_exact` 读不满 `read_len` -> 返回 IO 错误 -> 整文件 Failed；
//!   - 读取发生在协议 `upload_chunk` **之前**，故失败分片绝不会被协议看到、不会误传；
//!   - 失败后不会调用 `complete`，也不会卡死。
//!
//! 这类异常只能在执行器读盘层触发，故用一个「只记录、无网络」的自定义协议承接。

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use rusty_cat::http_breakpoint::UploadResumeInfo;
use rusty_cat::upload_trait::{UploadChunkCtx, UploadPrepareCtx};
use rusty_cat::{
    BreakpointUpload, MeowClient, MeowConfig, MeowError, TransferStatus, UploadPounceBuilder,
};

fn temp_upload_path(case: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    p.push(format!("rusty_cat_read_fail_{case}_{ts}.bin"));
    p
}

#[derive(Default, Clone)]
struct Recorder {
    /// offset -> chunk len for every part the protocol actually received.
    received: BTreeMap<u64, usize>,
    complete_calls: usize,
}

/// Records what the executor hands it; never touches the network. Serial only.
struct RecordingUpload {
    rec: Arc<Mutex<Recorder>>,
}

#[async_trait]
impl BreakpointUpload for RecordingUpload {
    async fn prepare(&self, _ctx: UploadPrepareCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(0),
            provider_upload_id: None,
        })
    }

    async fn upload_chunk(&self, ctx: UploadChunkCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        let offset = ctx.offset;
        let len = ctx.chunk.len();
        self.rec
            .lock()
            .expect("rec lock")
            .received
            .insert(offset, len);
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(offset + len as u64),
            provider_upload_id: None,
        })
    }

    async fn complete_upload(
        &self,
        _client: &reqwest::Client,
        _task: &rusty_cat::TransferTask,
    ) -> Result<Option<String>, MeowError> {
        self.rec.lock().expect("rec lock").complete_calls += 1;
        Ok(Some("done".to_string()))
    }
}

struct RunOutcome {
    terminal: Option<TransferStatus>,
    rec: Recorder,
}

/// Writes a `build_len`-byte file, builds the task (which captures total_size from
/// the file's metadata), truncates the file to `truncate_to`, then runs the upload.
async fn run_truncated(case: &str, build_len: usize, truncate_to: u64) -> RunOutcome {
    let path = temp_upload_path(case);
    fs::write(&path, vec![7u8; build_len]).expect("write upload fixture");

    let rec = Arc::new(Mutex::new(Recorder::default()));
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(1)
            .build()
            .expect("valid config"),
    );
    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();

    // total_size is fixed at build() from the file's current length.
    let task = UploadPounceBuilder::new("p.bin", &path, 1000)
        .with_url("https://placeholder/upload")
        .with_breakpoint_upload(Arc::new(RecordingUpload { rec: rec.clone() }))
        // Fail immediately on the first read error: no retry can fix a truncated
        // file, so 0 retries keeps the test fast and deterministic.
        .with_max_chunk_retries(0)
        .build()
        .expect("build upload task");

    // Shrink the file AFTER build so total_size (captured above) is now larger
    // than the file: the chunk read must hit EOF inside read_exact.
    let f = fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("open for truncate");
    f.set_len(truncate_to).expect("truncate fixture");
    drop(f);

    client
        .try_enqueue(
            task,
            move |record| {
                statuses_cb
                    .lock()
                    .expect("lock statuses")
                    .push(record.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("enqueue upload task");

    let mut terminal = None;
    for _ in 0..500 {
        let snapshot = statuses.lock().expect("lock statuses").clone();
        terminal = snapshot
            .iter()
            .rev()
            .find(|s| {
                matches!(
                    s,
                    TransferStatus::Complete | TransferStatus::Failed(_) | TransferStatus::Canceled
                )
            })
            .cloned();
        if terminal.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    client.close().await.expect("close client");
    let _ = fs::remove_file(&path);

    let rec = rec.lock().expect("rec lock").clone();
    RunOutcome { terminal, rec }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fully_truncated_source_fails_first_chunk_read_without_uploading() {
    // File emptied after build: the very first chunk's read_exact hits EOF, so the
    // upload fails before the protocol ever sees a byte.
    let out = run_truncated("empty", 4000, 0).await;

    assert!(
        matches!(out.terminal, Some(TransferStatus::Failed(_))),
        "a truncated source file must fail the upload, got {:?}",
        out.terminal
    );
    assert!(
        out.rec.received.is_empty(),
        "no chunk must reach the protocol when the first read fails, got {:?}",
        out.rec.received.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        out.rec.complete_calls, 0,
        "complete must not be called after a read failure"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partially_truncated_source_fails_mid_stream_without_completing() {
    // 4000-byte file (4 chunks at build) truncated to 2500 bytes: chunks at
    // offset 0 and 1000 read fine, the chunk at offset 2000 can only read 500 of
    // its 1000 bytes -> read_exact EOF -> Failed mid-stream, finalize never runs.
    let out = run_truncated("partial", 4000, 2500).await;

    assert!(
        matches!(out.terminal, Some(TransferStatus::Failed(_))),
        "a mid-stream short read must fail the upload, got {:?}",
        out.terminal
    );
    assert_eq!(
        out.rec.received.keys().copied().collect::<Vec<_>>(),
        vec![0, 1000],
        "only the fully-readable leading chunks may reach the protocol"
    );
    assert_eq!(
        out.rec.complete_calls, 0,
        "complete must not be called after a mid-stream read failure"
    );
}
