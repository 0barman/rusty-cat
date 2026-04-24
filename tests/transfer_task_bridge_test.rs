#[path = "dev_server/mod.rs"]
mod dev_server;

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_LENGTH};
use reqwest::multipart;
use reqwest::Method;
use rusty_cat::down_pounce_builder::DownloadPounceBuilder;
use rusty_cat::error::{InnerErrorCode, MeowError};
use rusty_cat::file_transfer_record::FileTransferRecord;
use rusty_cat::http_breakpoint::{
    BreakpointDownload, BreakpointUpload, DownloadHeadCtx, DownloadRangeGetCtx, UploadBody,
    UploadChunkCtx, UploadPrepareCtx, UploadRequest, UploadResumeInfo,
};
use rusty_cat::meow_config::MeowConfig;
use rusty_cat::transfer_status::TransferStatus;
use rusty_cat::transfer_task::TransferTask;
use rusty_cat::up_pounce_builder::UploadPounceBuilder;
use rusty_cat::MeowClient;

fn temp_path(case: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    p.push(format!("rusty_cat_transfer_task_bridge_{case}_{ts}.bin"));
    p
}

async fn wait_terminal_status(statuses: Arc<Mutex<Vec<TransferStatus>>>) -> TransferStatus {
    for _ in 0..300 {
        if let Some(last) = statuses.lock().expect("lock statuses").last().cloned() {
            if matches!(
                last,
                TransferStatus::Complete | TransferStatus::Failed(_) | TransferStatus::Canceled
            ) {
                return last;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("did not receive terminal status in time");
}

async fn bridge_send_upload_request(
    client: &reqwest::Client,
    req: UploadRequest,
) -> Result<String, MeowError> {
    let mut builder = client.request(req.method, req.url).headers(req.headers);
    builder = match req.body {
        UploadBody::Multipart(form) => builder.multipart(form),
        UploadBody::Binary(bytes) => builder.body(bytes),
    };
    let resp = builder
        .send()
        .await
        .map_err(|e| MeowError::from_source(InnerErrorCode::HttpError, e.to_string(), e))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| MeowError::from_source(InnerErrorCode::HttpError, e.to_string(), e))?;
    if !status.is_success() {
        return Err(MeowError::from_code(
            InnerErrorCode::ResponseStatusError,
            format!("upload HTTP {status}: {body}"),
        ));
    }
    Ok(body)
}

struct InspectUploadProtocol {
    prepare_calls: Arc<AtomicUsize>,
    chunk_calls: Arc<AtomicUsize>,
}

impl InspectUploadProtocol {
    fn parse_upload_response(&self, body: &str) -> Result<UploadResumeInfo, MeowError> {
        let v: serde_json::Value = serde_json::from_str(body).map_err(|e| {
            MeowError::from_code(
                InnerErrorCode::ResponseParseError,
                format!("custom parse upload response failed: {e}"),
            )
        })?;
        let next_byte = v.get("nextByte").and_then(|n| n.as_u64());
        let file_id = v
            .get("fileId")
            .and_then(|v| v.as_str())
            .map(ToString::to_string);
        Ok(UploadResumeInfo {
            completed_file_id: file_id,
            next_byte,
        })
    }
}

#[async_trait]
impl BreakpointUpload for InspectUploadProtocol {
    async fn prepare(&self, ctx: UploadPrepareCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        let client = ctx.client;
        let task = ctx.task;
        self.prepare_calls.fetch_add(1, Ordering::Relaxed);
        // 覆盖 TransferTask 公共 getter：上传场景。
        assert_eq!(task.direction().as_ref(), "Upload");
        assert!(task.total_size() > 0);
        assert!(task.chunk_size() > 0);
        assert!(!task.file_name().is_empty());
        assert!(!task.file_path().as_os_str().is_empty());
        assert!(!task.url().is_empty());
        assert_eq!(task.method(), Method::POST);
        assert!(task.headers().contains_key("x-upload-case"));
        let form = multipart::Form::new()
            .text("fileName", task.file_name().to_string())
            .text("totalSize", task.total_size().to_string());
        let req = UploadRequest::from_task(task, UploadBody::Multipart(form));
        let body = bridge_send_upload_request(client, req).await?;
        self.parse_upload_response(&body)
    }

    async fn upload_chunk(&self, ctx: UploadChunkCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        let client = ctx.client;
        let task = ctx.task;
        let chunk = ctx.chunk;
        let offset = ctx.offset;
        self.chunk_calls.fetch_add(1, Ordering::Relaxed);
        assert!(offset <= task.total_size());
        let part = multipart::Part::bytes(chunk.to_vec())
            .file_name("chunk.bin")
            .mime_str("application/octet-stream")
            .map_err(|e| MeowError::from_code(InnerErrorCode::HttpError, e.to_string()))?;
        let form = multipart::Form::new()
            .part("file", part)
            .text("offset", offset.to_string())
            .text("partSize", chunk.len().to_string())
            .text("totalSize", task.total_size().to_string());
        let req = UploadRequest::from_task(task, UploadBody::Multipart(form));
        let body = bridge_send_upload_request(client, req).await?;
        self.parse_upload_response(&body)
    }
}

struct InspectDownloadProtocol {
    merge_head_calls: Arc<AtomicUsize>,
    merge_get_calls: Arc<AtomicUsize>,
}

impl BreakpointDownload for InspectDownloadProtocol {
    fn merge_head_headers(&self, ctx: DownloadHeadCtx<'_>) -> Result<(), MeowError> {
        self.merge_head_calls.fetch_add(1, Ordering::Relaxed);
        ctx.base.insert(
            HeaderName::from_static("x-download-case"),
            HeaderValue::from_static("head-merged"),
        );
        Ok(())
    }

    fn merge_range_get_headers(&self, ctx: DownloadRangeGetCtx<'_>) -> Result<(), MeowError> {
        let task = ctx.task;
        self.merge_get_calls.fetch_add(1, Ordering::Relaxed);
        // 覆盖 TransferTask 公共 getter：下载场景。
        assert_eq!(task.direction().as_ref(), "Download");
        assert!(task.chunk_size() > 0);
        assert!(!task.file_name().is_empty());
        assert!(!task.file_path().as_os_str().is_empty());
        assert!(!task.url().is_empty());
        assert_eq!(task.method(), Method::GET);
        ctx.base.insert(
            HeaderName::from_static("range"),
            HeaderValue::from_str(ctx.range_value).expect("valid range header"),
        );
        ctx.base.insert(
            HeaderName::from_static("accept"),
            HeaderValue::from_static("application/octet-stream"),
        );
        Ok(())
    }

    fn total_size_from_head(&self, headers: &HeaderMap) -> Result<u64, MeowError> {
        headers
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|&n| n > 0)
            .ok_or_else(|| {
                MeowError::from_code_str(
                    InnerErrorCode::MissingOrInvalidContentLengthFromHead,
                    "missing content length in custom breakpoint",
                )
            })
    }
}

trait DirectionName {
    fn as_ref(&self) -> &'static str;
}

impl DirectionName for rusty_cat::direction::Direction {
    fn as_ref(&self) -> &'static str {
        match self {
            rusty_cat::direction::Direction::Upload => "Upload",
            rusty_cat::direction::Direction::Download => "Download",
        }
    }
}

#[tokio::test]
async fn custom_upload_breakpoint_exercises_transfer_task_getters() {
    // 场景说明：
    // 1) 注入自定义 BreakpointUpload，在 prepare/chunk 回调里读取 TransferTask 的公共 getter；
    // 2) 通过真实上传流程确保这些 getter 在生产路径可用；
    // 3) 覆盖 transfer_task.rs 的上传相关读取分支。
    let server = dev_server::DevFileServer::spawn(Vec::new());
    let src = temp_path("upload_src");
    fs::write(&src, b"custom-upload-transfer-task").expect("write upload source");

    let prepare_calls = Arc::new(AtomicUsize::new(0));
    let chunk_calls = Arc::new(AtomicUsize::new(0));
    let protocol = Arc::new(InspectUploadProtocol {
        prepare_calls: prepare_calls.clone(),
        chunk_calls: chunk_calls.clone(),
    });

    let mut headers = HeaderMap::new();
    headers.insert("x-upload-case", HeaderValue::from_static("yes"));

    let client = MeowClient::new(MeowConfig::new(1, 1));
    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();
    let task = UploadPounceBuilder::new("bridge.bin", &src, 1024)
        .with_url(format!("{}/upload/bridge.bin", server.base_url()))
        .with_method(Method::POST)
        .with_headers(headers)
        .with_breakpoint_upload(protocol)
        .build()
        .expect("build upload task");
    client
        .try_enqueue(
            task,
            move |record: FileTransferRecord| {
                statuses_cb
                    .lock()
                    .expect("lock statuses")
                    .push(record.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("enqueue upload");

    let terminal = wait_terminal_status(statuses).await;
    assert!(matches!(terminal, TransferStatus::Complete));
    assert!(prepare_calls.load(Ordering::Relaxed) >= 1);
    assert!(chunk_calls.load(Ordering::Relaxed) >= 1);

    client.close().await.expect("close client");
    fs::remove_file(&src).expect("remove source");
    server.shutdown();
}

#[tokio::test]
async fn custom_download_breakpoint_exercises_transfer_task_getters() {
    // 场景说明：
    // 1) 注入自定义 BreakpointDownload，在 merge_head_headers/merge_range_get_headers 中读取 TransferTask getter；
    // 2) 通过真实下载流程验证自定义协议路径；
    // 3) 覆盖 transfer_task.rs 的下载相关读取分支。
    let payload = b"custom-download-transfer-task".repeat(1024);
    let server = dev_server::DevFileServer::spawn(payload.clone());
    let path = temp_path("download_dst");

    let merge_head_calls = Arc::new(AtomicUsize::new(0));
    let merge_get_calls = Arc::new(AtomicUsize::new(0));
    let protocol = Arc::new(InspectDownloadProtocol {
        merge_head_calls: merge_head_calls.clone(),
        merge_get_calls: merge_get_calls.clone(),
    });

    let client = MeowClient::new(MeowConfig::new(1, 1));
    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();
    let task = DownloadPounceBuilder::new(
        "bridge.bin",
        &path,
        1024,
        format!("{}/download/bridge.bin", server.base_url()),
    )
    .with_breakpoint_download(protocol)
    .build();
    client
        .try_enqueue(
            task,
            move |record: FileTransferRecord| {
                statuses_cb
                    .lock()
                    .expect("lock statuses")
                    .push(record.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("enqueue download");

    let terminal = wait_terminal_status(statuses).await;
    assert!(matches!(terminal, TransferStatus::Complete));
    assert!(merge_head_calls.load(Ordering::Relaxed) >= 1);
    assert!(merge_get_calls.load(Ordering::Relaxed) >= 1);
    let bytes = fs::read(&path).expect("read downloaded file");
    assert_eq!(bytes, payload);

    client.close().await.expect("close client");
    fs::remove_file(&path).expect("remove file");
    server.shutdown();
}
