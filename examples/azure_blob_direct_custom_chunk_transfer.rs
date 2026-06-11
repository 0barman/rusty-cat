mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error as StdError;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use chrono::{Local, Utc};
use hmac::{Hmac, Mac};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, RANGE};
use reqwest::{Method, Url};
use rusty_cat::api::{
    DownloadPounceBuilder, DownloadRangeGetCtx, FileTransferRecord, InnerErrorCode, MeowClient,
    MeowConfig, MeowError, PounceTask, TaskId, TransferStatus, UploadChunkCtx, UploadPounceBuilder,
    UploadPrepareCtx, UploadResumeInfo,
};
use sha2::Sha256;
use tokio::sync::{mpsc, Mutex};

use support::{
    make_file, placeholder, progress_bar, remove_if_exists, temp_path, AnyResult, ONE_MB,
};

type HmacSha256 = Hmac<Sha256>;

const ACCOUNT_NAME: &str = "";
const ACCOUNT_KEY: &str = "";
const CONTAINER_NAME: &str = "test";
const BLOB_NAME_DIR: &str = "rusty-cat/examples";
const UPLOAD_FILE_SIZE: usize = 20 * 1024 * 1024;
const PAUSE_PROGRESS: f32 = 0.15;
const PAUSE_SECONDS: u64 = 2;
const WAIT_TIMEOUT_SECONDS: u64 = 20 * 60;
const MS_VERSION: &str = "2023-11-03";
const DEFAULT_RANGE_ACCEPT: &str = "application/octet-stream";

#[derive(Default)]
struct PutBlockSession {
    target_url: Option<String>,
    uploaded_blocks: BTreeSet<usize>,
}

#[derive(Clone)]
struct AzureDirectUpload {
    session: Arc<Mutex<PutBlockSession>>,
}

#[async_trait]
impl rusty_cat::api::BreakpointUpload for AzureDirectUpload {
    async fn prepare(
        &self,
        ctx: UploadPrepareCtx<'_>,
    ) -> Result<UploadResumeInfo, rusty_cat::api::MeowError> {
        {
            let mut state = self.session.lock().await;
            if state.target_url.as_deref() != Some(ctx.task.url()) {
                *state = PutBlockSession {
                    target_url: Some(ctx.task.url().to_string()),
                    uploaded_blocks: BTreeSet::new(),
                };
            }
            if ctx.local_offset == 0 {
                state.uploaded_blocks.clear();
                return Ok(UploadResumeInfo {
                    completed_file_id: None,
                    next_byte: Some(0),
                    provider_upload_id: None,
                });
            }
            if !state.uploaded_blocks.is_empty() {
                return Ok(UploadResumeInfo {
                    completed_file_id: None,
                    next_byte: Some(ctx.local_offset),
                    provider_upload_id: None,
                });
            }
        }
        let indices = list_uncommitted_blocks(ctx.client, ctx.task).await?;
        if !indices.is_empty() {
            self.session.lock().await.uploaded_blocks.extend(indices);
        }
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(ctx.local_offset),
            provider_upload_id: None,
        })
    }

    async fn upload_chunk(
        &self,
        ctx: UploadChunkCtx<'_>,
    ) -> Result<UploadResumeInfo, rusty_cat::api::MeowError> {
        let idx = usize::try_from(ctx.offset / ctx.task.chunk_size()).map_err(|e| {
            rusty_cat::api::MeowError::from_code(
                rusty_cat::api::InnerErrorCode::InvalidRange,
                format!("part index overflow: {e}"),
            )
        })?;
        let block_id = block_id_by_index(idx);
        let url = build_query_url(
            ctx.task,
            &[("comp", "block".to_string()), ("blockid", block_id)],
        )?;
        let headers = signed_headers(
            "PUT",
            &url,
            Some(ctx.chunk.len()),
            Some("application/octet-stream"),
            &[],
        )?;
        let resp = ctx
            .client
            .request(Method::PUT, url)
            .headers(headers)
            .body(reqwest::Body::from(ctx.chunk.clone()))
            .send()
            .await
            .map_err(map_http)?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(rusty_cat::api::MeowError::from_code(
                rusty_cat::api::InnerErrorCode::ResponseStatusError,
                format!("microsoft put block failed: {status}, body: {body}"),
            ));
        }
        self.session.lock().await.uploaded_blocks.insert(idx);
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(ctx.offset + ctx.chunk.len() as u64),
            provider_upload_id: None,
        })
    }

    async fn complete_upload(
        &self,
        client: &reqwest::Client,
        task: &rusty_cat::api::TransferTask,
    ) -> Result<Option<String>, rusty_cat::api::MeowError> {
        let total_chunks =
            ((task.total_size() + task.chunk_size() - 1) / task.chunk_size()) as usize;
        let block_ids = (0..total_chunks).map(block_id_by_index).collect::<Vec<_>>();
        let xml = block_list_xml(block_ids.iter().map(String::as_str));
        let url = build_query_url(task, &[("comp", "blocklist".to_string())])?;
        let headers = signed_headers("PUT", &url, Some(xml.len()), Some("application/xml"), &[])?;
        let resp = client
            .request(Method::PUT, url)
            .headers(headers)
            .body(xml)
            .send()
            .await
            .map_err(map_http)?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(rusty_cat::api::MeowError::from_code(
                rusty_cat::api::InnerErrorCode::ResponseStatusError,
                format!("microsoft put block list failed: {status}, body: {body}"),
            ));
        }
        self.session.lock().await.uploaded_blocks.clear();
        Ok(None)
    }

    async fn abort_upload(
        &self,
        client: &reqwest::Client,
        task: &rusty_cat::api::TransferTask,
    ) -> Result<(), rusty_cat::api::MeowError> {
        let url = Url::parse(task.url()).map_err(param_error)?;
        let headers = signed_headers("DELETE", &url, None, None, &[])?;
        let resp = client
            .request(Method::DELETE, url)
            .headers(headers)
            .send()
            .await
            .map_err(map_http)?;
        let status = resp.status();
        if !(status.is_success() || status == reqwest::StatusCode::NOT_FOUND) {
            let body = resp.text().await.unwrap_or_default();
            return Err(rusty_cat::api::MeowError::from_code(
                rusty_cat::api::InnerErrorCode::ResponseStatusError,
                format!("microsoft delete blob on cancel failed: {status}, body: {body}"),
            ));
        }
        self.session.lock().await.uploaded_blocks.clear();
        Ok(())
    }
}

#[derive(Clone)]
struct AzureDirectDownload;

impl rusty_cat::api::BreakpointDownload for AzureDirectDownload {
    fn merge_head_headers(
        &self,
        ctx: rusty_cat::api::DownloadHeadCtx<'_>,
    ) -> Result<(), rusty_cat::api::MeowError> {
        signed_headers_into_task(ctx.task, "HEAD", ctx.base)
    }

    fn merge_range_get_headers(
        &self,
        ctx: DownloadRangeGetCtx<'_>,
    ) -> Result<(), rusty_cat::api::MeowError> {
        ctx.base.insert(RANGE, header_value(ctx.range_value)?);
        if !ctx.base.contains_key(ACCEPT) {
            ctx.base
                .insert(ACCEPT, HeaderValue::from_static(DEFAULT_RANGE_ACCEPT));
        }
        signed_headers_into_task(ctx.task, "GET", ctx.base)
    }
}

async fn list_uncommitted_blocks(
    client: &reqwest::Client,
    task: &rusty_cat::api::TransferTask,
) -> Result<Vec<usize>, rusty_cat::api::MeowError> {
    let url = build_query_url(
        task,
        &[
            ("comp", "blocklist".to_string()),
            ("blocklisttype", "uncommitted".to_string()),
        ],
    )?;
    let headers = signed_headers("GET", &url, None, None, &[])?;
    let resp = client
        .request(Method::GET, url)
        .headers(headers)
        .send()
        .await
        .map_err(map_http)?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(Vec::new());
    }
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(rusty_cat::api::MeowError::from_code(
            rusty_cat::api::InnerErrorCode::ResponseStatusError,
            format!("microsoft list block list failed: {status}, body: {body}"),
        ));
    }
    Ok(parse_block_indices_from_block_list(&body))
}

fn build_query_url(
    task: &rusty_cat::api::TransferTask,
    query_pairs: &[(&str, String)],
) -> Result<Url, rusty_cat::api::MeowError> {
    let mut url = Url::parse(task.url()).map_err(param_error)?;
    {
        let mut pairs = url.query_pairs_mut();
        for (k, v) in query_pairs {
            pairs.append_pair(k, v.as_str());
        }
    }
    Ok(url)
}

fn signed_headers_into_task(
    task: &rusty_cat::api::TransferTask,
    method: &str,
    base: &mut HeaderMap,
) -> Result<(), rusty_cat::api::MeowError> {
    let url = Url::parse(task.url()).map_err(param_error)?;
    insert_header(base, "x-ms-version", MS_VERSION)?;
    insert_header(base, "x-ms-date", now_rfc1123_gmt().as_str())?;
    let auth = build_authorization(method, &url, base)?;
    insert_header(base, "authorization", auth.as_str())?;
    Ok(())
}

fn signed_headers(
    method: &str,
    url: &Url,
    content_length: Option<usize>,
    content_type: Option<&str>,
    extra_headers: &[(&str, &str)],
) -> Result<HeaderMap, rusty_cat::api::MeowError> {
    let mut headers = HeaderMap::new();
    insert_header(&mut headers, "x-ms-version", MS_VERSION)?;
    insert_header(&mut headers, "x-ms-date", now_rfc1123_gmt().as_str())?;
    if let Some(v) = content_type {
        insert_header(&mut headers, "content-type", v)?;
    }
    if let Some(v) = content_length {
        insert_header(&mut headers, "content-length", v.to_string().as_str())?;
    }
    for (k, v) in extra_headers {
        insert_header(&mut headers, k, v)?;
    }
    let authorization = build_authorization(method, url, &headers)?;
    insert_header(&mut headers, "authorization", authorization.as_str())?;
    Ok(headers)
}

fn build_authorization(
    method: &str,
    url: &Url,
    headers: &HeaderMap,
) -> Result<String, rusty_cat::api::MeowError> {
    let canonicalized_headers = canonicalized_headers(headers)?;
    let canonicalized_resource = canonicalized_resource(url);
    let string_to_sign = format!(
        "{method}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{canonicalized_headers}{canonicalized_resource}",
        header_map_value(headers, "content-encoding"),
        header_map_value(headers, "content-language"),
        canonicalized_content_length(headers),
        header_map_value(headers, "content-md5"),
        header_map_value(headers, "content-type"),
        "",
        header_map_value(headers, "if-modified-since"),
        header_map_value(headers, "if-match"),
        header_map_value(headers, "if-none-match"),
        header_map_value(headers, "if-unmodified-since"),
        header_map_value(headers, "range"),
    );
    let key = BASE64_STANDARD.decode(ACCOUNT_KEY).map_err(|e| {
        rusty_cat::api::MeowError::from_code(
            rusty_cat::api::InnerErrorCode::ParameterEmpty,
            format!("decode microsoft account key failed: {e}"),
        )
    })?;
    let mut mac = HmacSha256::new_from_slice(&key).map_err(|e| {
        rusty_cat::api::MeowError::from_code(
            rusty_cat::api::InnerErrorCode::ParameterEmpty,
            format!("build HMAC-SHA256 failed: {e}"),
        )
    })?;
    mac.update(string_to_sign.as_bytes());
    let signature = BASE64_STANDARD.encode(mac.finalize().into_bytes());
    Ok(format!("SharedKey {ACCOUNT_NAME}:{signature}"))
}

fn canonicalized_headers(headers: &HeaderMap) -> Result<String, rusty_cat::api::MeowError> {
    let mut pairs = Vec::new();
    for (k, v) in headers {
        let k = k.as_str().to_ascii_lowercase();
        if !k.starts_with("x-ms-") {
            continue;
        }
        let value = v.to_str().map_err(|e| {
            rusty_cat::api::MeowError::from_code(
                rusty_cat::api::InnerErrorCode::ParameterEmpty,
                format!("x-ms header is not valid ASCII: {e}"),
            )
        })?;
        pairs.push((k, value.trim().to_string()));
    }
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = String::new();
    for (k, v) in pairs {
        out.push_str(&k);
        out.push(':');
        out.push_str(&v);
        out.push('\n');
    }
    Ok(out)
}

fn canonicalized_resource(url: &Url) -> String {
    let mut out = format!("/{ACCOUNT_NAME}{}", url.path());
    let mut query_map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (k, v) in url.query_pairs() {
        query_map
            .entry(k.to_ascii_lowercase())
            .or_default()
            .push(v.into_owned());
    }
    for (k, values) in query_map {
        let mut values = values;
        values.sort();
        out.push('\n');
        out.push_str(&k);
        out.push(':');
        out.push_str(&values.join(","));
    }
    out
}

fn canonicalized_content_length(headers: &HeaderMap) -> String {
    let raw = header_map_value(headers, "content-length");
    if raw == "0" {
        String::new()
    } else {
        raw
    }
}

fn header_map_value(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn now_rfc1123_gmt() -> String {
    Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

fn insert_header(
    headers: &mut HeaderMap,
    name: &str,
    value: &str,
) -> Result<(), rusty_cat::api::MeowError> {
    let name = HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
        rusty_cat::api::MeowError::from_code(
            rusty_cat::api::InnerErrorCode::ParameterEmpty,
            format!("invalid header name '{name}': {e}"),
        )
    })?;
    let value = HeaderValue::from_str(value).map_err(|e| {
        rusty_cat::api::MeowError::from_code(
            rusty_cat::api::InnerErrorCode::ParameterEmpty,
            format!("invalid header value for '{name}': {e}"),
        )
    })?;
    headers.insert(name, value);
    Ok(())
}

fn block_id_by_index(idx: usize) -> String {
    BASE64_STANDARD.encode(format!("{idx:08}"))
}

fn block_list_xml<'a>(block_ids: impl IntoIterator<Item = &'a str>) -> String {
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList>");
    for id in block_ids {
        xml.push_str("<Latest>");
        xml.push_str(id);
        xml.push_str("</Latest>");
    }
    xml.push_str("</BlockList>");
    xml
}

fn parse_block_indices_from_block_list(xml: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for seg in xml.split("<Name>").skip(1) {
        if let Some((v, _)) = seg.split_once("</Name>") {
            if let Ok(raw) = BASE64_STANDARD.decode(v.trim()) {
                if let Ok(s) = String::from_utf8(raw) {
                    if let Ok(i) = s.parse::<usize>() {
                        out.push(i);
                    }
                }
            }
        }
    }
    out.sort_unstable();
    out
}

fn blob_url(blob_name: &str) -> String {
    format!(
        "https://{ACCOUNT_NAME}.blob.core.windows.net/{}/{}",
        CONTAINER_NAME.trim_matches('/'),
        blob_name.trim_start_matches('/')
    )
}

fn current_blob_name() -> String {
    let date = Local::now().format("%Y-%m%d");
    let random = format!(
        "{:04x}",
        Utc::now().timestamp_nanos_opt().unwrap_or_default() & 0xffff
    );
    format!(
        "{}/direct-azure-{date}-{random}-20mb.bin",
        BLOB_NAME_DIR.trim_matches('/')
    )
}

fn current_file_name() -> String {
    Local::now()
        .format("rusty_cat_azure_direct_%Y%m%d_%H_%M_20mb.bin")
        .to_string()
}

fn header_value(value: &str) -> Result<HeaderValue, rusty_cat::api::MeowError> {
    HeaderValue::from_str(value).map_err(|e| {
        rusty_cat::api::MeowError::from_code(
            rusty_cat::api::InnerErrorCode::ParameterEmpty,
            format!("invalid header value: {e}"),
        )
    })
}

fn param_error<E: std::fmt::Display>(e: E) -> rusty_cat::api::MeowError {
    rusty_cat::api::MeowError::from_code(
        rusty_cat::api::InnerErrorCode::ParameterEmpty,
        format!("invalid microsoft blob url: {e}"),
    )
}

fn map_http(e: reqwest::Error) -> rusty_cat::api::MeowError {
    rusty_cat::api::MeowError::from_source(
        rusty_cat::api::InnerErrorCode::HttpError,
        e.to_string(),
        e,
    )
}

fn azure_config() -> AnyResult<MeowConfig> {
    Ok(MeowConfig::builder()
        .max_upload_concurrency(1)
        .max_download_concurrency(1)
        .http_timeout(Duration::from_secs(30))
        .tcp_keepalive(Duration::from_secs(60))
        .build()?)
}

fn ensure_azure_credentials(stage: &str) -> bool {
    if ACCOUNT_NAME.trim().is_empty() || ACCOUNT_KEY.trim().is_empty() {
        println!(
            "Azure direct {stage} cannot start: please fill ACCOUNT_NAME and ACCOUNT_KEY at the top of this file first"
        );
        return false;
    }
    true
}

fn error_with_sources(error: &(dyn StdError + 'static)) -> String {
    let mut out = error.to_string();
    let mut source = error.source();
    while let Some(err) = source {
        out.push_str("; caused by: ");
        out.push_str(&err.to_string());
        source = err.source();
    }
    out
}

enum TaskEvent {
    Progress(f32),
    Complete,
    Failed(String),
    Canceled,
}

enum PauseResumeOutcome {
    Resumed,
    Finished,
}

fn is_resume_still_stopping(error: &MeowError) -> bool {
    error.code() == InnerErrorCode::InvalidTaskState as i32
        && error.msg().contains("still stopping")
}

fn drain_terminal_event(
    rx: &mut mpsc::UnboundedReceiver<TaskEvent>,
    label: &str,
) -> AnyResult<Option<()>> {
    loop {
        match rx.try_recv() {
            Ok(TaskEvent::Complete) => return Ok(Some(())),
            Ok(TaskEvent::Failed(e)) => return Err(e.into()),
            Ok(TaskEvent::Canceled) => return Err(format!("{label} canceled").into()),
            Ok(TaskEvent::Progress(_)) => {}
            Err(mpsc::error::TryRecvError::Empty) => return Ok(None),
            Err(mpsc::error::TryRecvError::Disconnected) => {
                return Err("wait transfer finished without completion callback".into())
            }
        }
    }
}

async fn resume_when_pause_stopped(
    client: &MeowClient,
    task_id: TaskId,
    rx: &mut mpsc::UnboundedReceiver<TaskEvent>,
    label: &str,
) -> AnyResult<PauseResumeOutcome> {
    for _ in 0..100 {
        match client.resume(task_id).await {
            Ok(()) => return Ok(PauseResumeOutcome::Resumed),
            Err(e) if is_resume_still_stopping(&e) => {
                tokio::time::sleep(Duration::from_millis(200)).await;
                if drain_terminal_event(rx, label)?.is_some() {
                    return Ok(PauseResumeOutcome::Finished);
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
    Err("resume target is still stopping after retries".into())
}

async fn run_task_pause_once_at(
    client: &MeowClient,
    task: PounceTask,
    label: &str,
    pause_at: f32,
    pause_for: Duration,
) -> AnyResult<TaskId> {
    let pb = progress_bar(label, 0);
    let done_pb = pb.clone();
    let fail_pb = pb.clone();
    let label = label.to_string();
    let done_label = label.clone();
    let fail_label = label.clone();
    let (tx, mut rx) = mpsc::unbounded_channel::<TaskEvent>();
    let progress_tx = tx.clone();
    let complete_tx = tx.clone();
    let task_id = client
        .try_enqueue(
            task,
            move |record: FileTransferRecord| {
                pb.set_length(record.total_size());
                let progress = record.progress().clamp(0.0, 1.0);
                let pos = (progress * record.total_size() as f32) as u64;
                pb.set_position(pos.min(record.total_size()));
                match record.status() {
                    TransferStatus::Failed(e) => {
                        fail_pb.finish_with_message(format!("{fail_label} failed"));
                        let _ = progress_tx.send(TaskEvent::Failed(format!(
                            "{fail_label} failed: {}",
                            error_with_sources(e)
                        )));
                    }
                    TransferStatus::Canceled => {
                        fail_pb.finish_with_message(format!("{fail_label} canceled"));
                        let _ = progress_tx.send(TaskEvent::Canceled);
                    }
                    _ => {
                        let _ = progress_tx.send(TaskEvent::Progress(progress));
                    }
                }
            },
            move |_task_id, payload| {
                done_pb.finish_with_message(format!("{done_label} complete {payload:?}"));
                let _ = complete_tx.send(TaskEvent::Complete);
            },
        )
        .await?;
    let mut paused = false;
    let wait = async {
        loop {
            let Some(event) = rx.recv().await else {
                return Err::<(), Box<dyn std::error::Error + Send + Sync>>(
                    "wait transfer finished without completion callback".into(),
                );
            };
            match event {
                TaskEvent::Progress(progress) if !paused && progress >= pause_at => {
                    paused = true;
                    client.pause(task_id).await?;
                    tokio::time::sleep(pause_for).await;
                    if drain_terminal_event(&mut rx, &label)?.is_some() {
                        return Ok::<(), Box<dyn std::error::Error + Send + Sync>>(());
                    }
                    match resume_when_pause_stopped(client, task_id, &mut rx, &label).await? {
                        PauseResumeOutcome::Resumed => {}
                        PauseResumeOutcome::Finished => {
                            return Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
                        }
                    }
                }
                TaskEvent::Complete => {
                    return Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
                }
                TaskEvent::Failed(e) => {
                    return Err::<(), Box<dyn std::error::Error + Send + Sync>>(e.into())
                }
                TaskEvent::Canceled => {
                    return Err::<(), Box<dyn std::error::Error + Send + Sync>>(
                        format!("{label} canceled").into(),
                    )
                }
                TaskEvent::Progress(_) => {}
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(WAIT_TIMEOUT_SECONDS), wait)
        .await
        .map_err(|e| format!("wait transfer timeout: {e}"))??;
    Ok(task_id)
}

#[tokio::main]
async fn main() -> AnyResult<()> {
    if !ensure_azure_credentials("upload/download") {
        return Ok(());
    }
    if placeholder(ACCOUNT_NAME) || placeholder(ACCOUNT_KEY) || placeholder(CONTAINER_NAME) {
        println!("fill Azure ACCOUNT_NAME, ACCOUNT_KEY and CONTAINER_NAME constants at the top of this file first");
        return Ok(());
    }
    let file_name = current_file_name();
    let blob_name = current_blob_name();
    let blob_url = blob_url(&blob_name);
    let upload_path = temp_path(&format!("rusty_cat_azure_direct_upload/{file_name}"));
    let download_path = temp_path(&format!("rusty_cat_azure_direct_download/{file_name}"));
    remove_if_exists(&upload_path)?;
    remove_if_exists(&download_path)?;
    make_file(&upload_path, UPLOAD_FILE_SIZE)?;
    let client = MeowClient::new(azure_config()?);
    let upload_protocol = AzureDirectUpload {
        session: Arc::new(Mutex::new(PutBlockSession::default())),
    };
    let upload_task = UploadPounceBuilder::new("azure-direct-upload.bin", &upload_path, ONE_MB)
        .with_url(blob_url.clone())
        .with_breakpoint_upload(Arc::new(upload_protocol))
        .build()?;
    println!("azure direct upload url: {blob_url}");
    run_task_pause_once_at(
        &client,
        upload_task,
        "azure direct upload",
        PAUSE_PROGRESS,
        Duration::from_secs(PAUSE_SECONDS),
    )
    .await?;
    let download_task = DownloadPounceBuilder::new(
        "azure-direct-download.bin",
        &download_path,
        ONE_MB,
        blob_url.clone(),
    )
    .with_breakpoint_download(Arc::new(AzureDirectDownload))
    .build();
    println!("azure direct download url: {blob_url}");
    run_task_pause_once_at(
        &client,
        download_task,
        "azure direct download",
        PAUSE_PROGRESS,
        Duration::from_secs(PAUSE_SECONDS),
    )
    .await?;
    client.close().await?;
    remove_if_exists(&upload_path)?;
    remove_if_exists(&download_path)?;
    Ok(())
}
