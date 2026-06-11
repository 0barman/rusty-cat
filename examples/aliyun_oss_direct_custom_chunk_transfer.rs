mod support;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{Local, Utc};
use hmac::{Hmac, Mac};
use reqwest::header::{
    HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_LENGTH, CONTENT_TYPE, RANGE,
};
use reqwest::{Method, Url};
use rusty_cat::api::{
    DownloadPounceBuilder, DownloadRangeGetCtx, FileTransferRecord, MeowClient, PounceTask, TaskId,
    TransferStatus, UploadChunkCtx, UploadPounceBuilder, UploadPrepareCtx, UploadResumeInfo,
};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, Mutex};

use support::{
    config, make_file, placeholder, progress_bar, remove_if_exists, temp_path, AnyResult, ONE_MB,
};

type HmacSha256 = Hmac<Sha256>;

const BUCKET_NAME: &str = "";
const ACCESS_KEY_ID: &str = "";
const ACCESS_KEY_SECRET: &str = "";
const REGION: &str = "cn-beijing";
const OBJECT_KEY_DIR: &str = "/test";
const UPLOAD_FILE_SIZE: usize = 20 * 1024 * 1024;
const PAUSE_PROGRESS: f32 = 0.10;
const PAUSE_SECONDS: u64 = 2;
const OSS_UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";
const DEFAULT_RANGE_ACCEPT: &str = "application/octet-stream";

#[derive(Default)]
struct MultipartSession {
    target_url: Option<String>,
    upload_id: Option<String>,
}

#[derive(Clone)]
struct AliyunDirectUpload {
    session: Arc<Mutex<MultipartSession>>,
}

#[async_trait]
impl rusty_cat::api::BreakpointUpload for AliyunDirectUpload {
    async fn prepare(
        &self,
        ctx: UploadPrepareCtx<'_>,
    ) -> Result<UploadResumeInfo, rusty_cat::api::MeowError> {
        {
            let mut state = self.session.lock().await;
            if state.target_url.as_deref() != Some(ctx.task.url()) {
                *state = MultipartSession {
                    target_url: Some(ctx.task.url().to_string()),
                    upload_id: None,
                };
            }
            if state.upload_id.is_some() {
                return Ok(UploadResumeInfo {
                    completed_file_id: None,
                    next_byte: Some(ctx.local_offset),
                    provider_upload_id: None,
                });
            }
        }
        if ctx.local_offset > 0 {
            if let Some(upload_id) = try_adopt_upload_id_from_list(ctx.client, ctx.task).await? {
                let mut state = self.session.lock().await;
                state.target_url = Some(ctx.task.url().to_string());
                state.upload_id = Some(upload_id);
                return Ok(UploadResumeInfo {
                    completed_file_id: None,
                    next_byte: Some(ctx.local_offset),
                    provider_upload_id: None,
                });
            }
            return Err(rusty_cat::api::MeowError::from_code_str(
                rusty_cat::api::InnerErrorCode::InvalidTaskState,
                "local offset > 0 but no OSS multipart session found",
            ));
        }
        let upload_id = initiate_multipart_upload(ctx.client, ctx.task).await?;
        let mut state = self.session.lock().await;
        state.target_url = Some(ctx.task.url().to_string());
        state.upload_id = Some(upload_id);
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(0),
            provider_upload_id: None,
        })
    }

    async fn upload_chunk(
        &self,
        ctx: UploadChunkCtx<'_>,
    ) -> Result<UploadResumeInfo, rusty_cat::api::MeowError> {
        let upload_id = self.session.lock().await.upload_id.clone().ok_or_else(|| {
            rusty_cat::api::MeowError::from_code_str(
                rusty_cat::api::InnerErrorCode::InvalidTaskState,
                "multipart upload_id missing",
            )
        })?;
        let part_number = ctx.offset / ctx.task.chunk_size() + 1;
        if part_number > 10_000 {
            return Err(rusty_cat::api::MeowError::from_code(
                rusty_cat::api::InnerErrorCode::InvalidRange,
                format!("partNumber out of range: {part_number}"),
            ));
        }
        let (url, raw_query) = build_query_url(
            ctx.task,
            &[
                ("partNumber", part_number.to_string()),
                ("uploadId", upload_id),
            ],
        )?;
        let headers = signed_headers(
            "PUT",
            object_canonical_uri_from_task_url(ctx.task)?.as_str(),
            Some(raw_query.as_str()),
            &[],
            None,
        )?;
        let resp = ctx
            .client
            .request(Method::PUT, url)
            .headers(headers)
            .body(reqwest::Body::from(ctx.chunk.clone()))
            .send()
            .await
            .map_err(map_http)?;
        let status = resp.status();
        let etag_present = resp.headers().get("etag").is_some();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(rusty_cat::api::MeowError::from_code(
                rusty_cat::api::InnerErrorCode::ResponseStatusError,
                format!("oss upload part failed: {status}, body: {body}"),
            ));
        }
        if !etag_present {
            return Err(rusty_cat::api::MeowError::from_code_str(
                rusty_cat::api::InnerErrorCode::ResponseParseError,
                "oss upload part success but missing ETag header",
            ));
        }
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
        let upload_id = self.session.lock().await.upload_id.clone();
        let Some(upload_id) = upload_id else {
            return Ok(None);
        };
        let (url, raw_query) = build_query_url(task, &[("uploadId", upload_id)])?;
        let mut headers = signed_headers(
            "POST",
            object_canonical_uri_from_task_url(task)?.as_str(),
            Some(raw_query.as_str()),
            &[
                ("content-type", "application/xml"),
                ("x-oss-complete-all", "yes"),
            ],
            Some("content-type;x-oss-complete-all"),
        )?;
        headers.insert(CONTENT_LENGTH, header_value("0")?);
        headers.insert(CONTENT_TYPE, header_value("application/xml")?);
        let resp = client
            .request(Method::POST, url)
            .headers(headers)
            .send()
            .await
            .map_err(map_http)?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(rusty_cat::api::MeowError::from_code(
                rusty_cat::api::InnerErrorCode::ResponseStatusError,
                format!("oss complete multipart upload failed: {status}, body: {body}"),
            ));
        }
        self.session.lock().await.upload_id = None;
        Ok(None)
    }

    async fn abort_upload(
        &self,
        client: &reqwest::Client,
        task: &rusty_cat::api::TransferTask,
    ) -> Result<(), rusty_cat::api::MeowError> {
        let upload_id = self.session.lock().await.upload_id.clone();
        let Some(upload_id) = upload_id else {
            return Ok(());
        };
        let (url, raw_query) = build_query_url(task, &[("uploadId", upload_id)])?;
        let headers = signed_headers(
            "DELETE",
            object_canonical_uri_from_task_url(task)?.as_str(),
            Some(raw_query.as_str()),
            &[],
            None,
        )?;
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
                format!("oss abort multipart upload failed: {status}, body: {body}"),
            ));
        }
        self.session.lock().await.upload_id = None;
        Ok(())
    }
}

#[derive(Clone)]
struct AliyunDirectDownload;

impl rusty_cat::api::BreakpointDownload for AliyunDirectDownload {
    fn merge_head_headers(
        &self,
        ctx: rusty_cat::api::DownloadHeadCtx<'_>,
    ) -> Result<(), rusty_cat::api::MeowError> {
        let headers = signed_headers(
            "HEAD",
            object_canonical_uri_from_task_url(ctx.task)?.as_str(),
            None,
            &[],
            None,
        )?;
        merge(ctx.base, headers);
        Ok(())
    }

    fn merge_range_get_headers(
        &self,
        ctx: DownloadRangeGetCtx<'_>,
    ) -> Result<(), rusty_cat::api::MeowError> {
        ctx.base.insert(RANGE, header_value(ctx.range_value)?);
        if !ctx.base.contains_key(ACCEPT) {
            ctx.base.insert(ACCEPT, header_value(DEFAULT_RANGE_ACCEPT)?);
        }
        let headers = signed_headers(
            "GET",
            object_canonical_uri_from_task_url(ctx.task)?.as_str(),
            None,
            &[],
            None,
        )?;
        merge(ctx.base, headers);
        Ok(())
    }
}

async fn initiate_multipart_upload(
    client: &reqwest::Client,
    task: &rusty_cat::api::TransferTask,
) -> Result<String, rusty_cat::api::MeowError> {
    let canonical_uri = object_canonical_uri_from_task_url(task)?;
    let mut url = Url::parse(task.url()).map_err(param_error)?;
    url.set_query(Some("uploads"));
    let raw_query = url.query().unwrap_or("uploads");
    let headers = signed_headers("POST", &canonical_uri, Some(raw_query), &[], None)?;
    let resp = client
        .request(Method::POST, url)
        .headers(headers)
        .send()
        .await
        .map_err(map_http)?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(rusty_cat::api::MeowError::from_code(
            rusty_cat::api::InnerErrorCode::ResponseStatusError,
            format!("oss initiate multipart failed: {status}, body: {body}"),
        ));
    }
    extract_xml_tag(&body, "UploadId").ok_or_else(|| {
        rusty_cat::api::MeowError::from_code(
            rusty_cat::api::InnerErrorCode::ResponseParseError,
            format!("oss initiate multipart missing UploadId: {body}"),
        )
    })
}

async fn try_adopt_upload_id_from_list(
    client: &reqwest::Client,
    task: &rusty_cat::api::TransferTask,
) -> Result<Option<String>, rusty_cat::api::MeowError> {
    let object_key = object_key_from_task_url(task)?;
    let mut url = Url::parse(task.url()).map_err(param_error)?;
    url.set_path("/");
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("uploads", "");
        pairs.append_pair("prefix", &object_key);
        pairs.append_pair("max-uploads", "1000");
    }
    let raw_query = url.query().ok_or_else(|| {
        rusty_cat::api::MeowError::from_code_str(
            rusty_cat::api::InnerErrorCode::ParameterEmpty,
            "build list multipart query failed",
        )
    })?;
    let bucket_uri = format!("/{BUCKET_NAME}/");
    let headers = signed_headers("GET", &bucket_uri, Some(raw_query), &[], None)?;
    let resp = client
        .request(Method::GET, url)
        .headers(headers)
        .send()
        .await
        .map_err(map_http)?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(rusty_cat::api::MeowError::from_code(
            rusty_cat::api::InnerErrorCode::ResponseStatusError,
            format!("oss list multipart uploads failed: {status}, body: {body}"),
        ));
    }
    let ids = extract_upload_ids_for_key(&body, &object_key);
    if ids.len() > 1 {
        return Err(rusty_cat::api::MeowError::from_code(
            rusty_cat::api::InnerErrorCode::InvalidTaskState,
            format!("found multiple multipart sessions for object '{object_key}'"),
        ));
    }
    Ok(ids.into_iter().next())
}

fn build_query_url(
    task: &rusty_cat::api::TransferTask,
    query_pairs: &[(&str, String)],
) -> Result<(Url, String), rusty_cat::api::MeowError> {
    let mut url = Url::parse(task.url()).map_err(param_error)?;
    {
        let mut pairs = url.query_pairs_mut();
        for (k, v) in query_pairs {
            pairs.append_pair(k, v.as_str());
        }
    }
    let query = url.query().map(|q| q.to_string()).ok_or_else(|| {
        rusty_cat::api::MeowError::from_code_str(
            rusty_cat::api::InnerErrorCode::ParameterEmpty,
            "build query url failed",
        )
    })?;
    Ok((url, query))
}

fn object_url(object_key: &str) -> String {
    format!("{}/{}", endpoint(), object_key.trim_start_matches('/'))
}

fn object_key_from_task_url(
    task: &rusty_cat::api::TransferTask,
) -> Result<String, rusty_cat::api::MeowError> {
    let url = Url::parse(task.url()).map_err(param_error)?;
    Ok(url.path().trim_start_matches('/').to_string())
}

fn current_object_key() -> String {
    let date = Local::now().format("%Y-%m%d");
    let random = format!(
        "{:04x}",
        Utc::now().timestamp_nanos_opt().unwrap_or_default() & 0xffff
    );
    format!(
        "{}/direct-aliyun-{date}-{random}-20mb.bin",
        OBJECT_KEY_DIR.trim_matches('/')
    )
}

fn bucket_host() -> String {
    format!("{BUCKET_NAME}.oss-{REGION}.aliyuncs.com")
}

fn endpoint() -> String {
    format!("https://{}", bucket_host())
}

fn current_file_name() -> String {
    Local::now()
        .format("rusty_cat_aliyun_%Y%m%d_%H_%M.bin")
        .to_string()
}

fn object_canonical_uri_from_task_url(
    task: &rusty_cat::api::TransferTask,
) -> Result<String, rusty_cat::api::MeowError> {
    let url = Url::parse(task.url()).map_err(param_error)?;
    Ok(format!("/{BUCKET_NAME}{}", url.path()))
}

fn signed_headers(
    method: &str,
    canonical_uri: &str,
    raw_query: Option<&str>,
    sign_pairs: &[(&str, &str)],
    additional_headers: Option<&str>,
) -> Result<HeaderMap, rusty_cat::api::MeowError> {
    let now = Utc::now();
    let iso8601 = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date = now.format("%Y%m%d").to_string();
    let mut entries = vec![
        (
            "x-oss-content-sha256".to_string(),
            OSS_UNSIGNED_PAYLOAD.to_string(),
        ),
        ("x-oss-date".to_string(), iso8601.clone()),
    ];
    for (k, v) in sign_pairs {
        entries.push((k.to_ascii_lowercase(), (*v).to_string()));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let canonical_headers = entries
        .iter()
        .map(|(k, v)| format!("{k}:{v}\n"))
        .collect::<String>();
    let additional_headers = additional_headers.unwrap_or_default();
    let canonical_request = format!(
        "{method}\n{canonical_uri}\n{}\n{canonical_headers}\n{additional_headers}\n{OSS_UNSIGNED_PAYLOAD}",
        raw_query.unwrap_or_default()
    );
    let scope = format!("{date}/{REGION}/oss/aliyun_v4_request");
    let string_to_sign = format!(
        "OSS4-HMAC-SHA256\n{iso8601}\n{scope}\n{}",
        hex::encode(Sha256::digest(canonical_request.as_bytes()))
    );
    let signing_key = hmac_sha256(
        format!("aliyun_v4{ACCESS_KEY_SECRET}").as_bytes(),
        date.as_bytes(),
    );
    let signing_key = hmac_sha256(&signing_key, REGION.as_bytes());
    let signing_key = hmac_sha256(&signing_key, b"oss");
    let signing_key = hmac_sha256(&signing_key, b"aliyun_v4_request");
    let signature = hex::encode(hmac_sha256(&signing_key, string_to_sign.as_bytes()));
    let mut headers = HeaderMap::new();
    headers.insert("x-oss-date", header_value(&iso8601)?);
    headers.insert("x-oss-content-sha256", header_value(OSS_UNSIGNED_PAYLOAD)?);
    for (k, v) in sign_pairs {
        let name = HeaderName::from_bytes(k.as_bytes()).map_err(|e| {
            rusty_cat::api::MeowError::from_code(
                rusty_cat::api::InnerErrorCode::ParameterEmpty,
                format!("invalid header name '{k}': {e}"),
            )
        })?;
        headers.insert(name, header_value(v)?);
    }
    let mut auth = format!("OSS4-HMAC-SHA256 Credential={ACCESS_KEY_ID}/{scope}");
    if !additional_headers.is_empty() {
        auth.push_str(",AdditionalHeaders=");
        auth.push_str(additional_headers);
    }
    auth.push_str(",Signature=");
    auth.push_str(&signature);
    headers.insert("authorization", header_value(&auth)?);
    Ok(headers)
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).unwrap();
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn header_value(v: &str) -> Result<HeaderValue, rusty_cat::api::MeowError> {
    HeaderValue::from_str(v).map_err(|e| {
        rusty_cat::api::MeowError::from_code(
            rusty_cat::api::InnerErrorCode::ParameterEmpty,
            format!("invalid header value: {e}"),
        )
    })
}

fn extract_xml_tag(body: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let (_, tail) = body.split_once(open.as_str())?;
    let (value, _) = tail.split_once(close.as_str())?;
    Some(value.trim().to_string())
}

fn extract_upload_ids_for_key(xml: &str, key: &str) -> Vec<String> {
    let mut ids = Vec::new();
    for seg in xml.split("<Upload>").skip(1) {
        if let Some((upload_block, _)) = seg.split_once("</Upload>") {
            let item_key = extract_xml_tag(upload_block, "Key");
            let upload_id = extract_xml_tag(upload_block, "UploadId");
            if item_key.as_deref() == Some(key) {
                if let Some(id) = upload_id {
                    ids.push(id);
                }
            }
        }
    }
    ids
}

fn merge(base: &mut HeaderMap, headers: HeaderMap) {
    for (k, v) in headers {
        if let Some(k) = k {
            base.insert(k, v);
        }
    }
}

fn param_error<E: std::fmt::Display>(e: E) -> rusty_cat::api::MeowError {
    rusty_cat::api::MeowError::from_code(
        rusty_cat::api::InnerErrorCode::ParameterEmpty,
        format!("invalid url: {e}"),
    )
}

fn map_http(e: reqwest::Error) -> rusty_cat::api::MeowError {
    rusty_cat::api::MeowError::from_source(
        rusty_cat::api::InnerErrorCode::HttpError,
        e.to_string(),
        e,
    )
}

enum TaskEvent {
    Progress(f32),
    Complete,
    Failed(String),
    Canceled,
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
                        let _ = progress_tx
                            .send(TaskEvent::Failed(format!("{fail_label} failed: {e}")));
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
                    client.resume(task_id).await?;
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
    tokio::time::timeout(Duration::from_secs(180), wait)
        .await
        .map_err(|e| format!("wait transfer timeout: {e}"))??;
    Ok(task_id)
}

#[tokio::main]
async fn main() -> AnyResult<()> {
    if BUCKET_NAME.trim().is_empty()
        || ACCESS_KEY_ID.trim().is_empty()
        || ACCESS_KEY_SECRET.trim().is_empty()
    {
        println!(
            "please fill BUCKET_NAME, ACCESS_KEY_ID and ACCESS_KEY_SECRET at the top of this file first"
        );
        return Ok(());
    }
    if placeholder(REGION) {
        println!("please fill REGION at the top of this file first");
        return Ok(());
    }
    let file_name = current_file_name();
    let object_key = current_object_key();
    let object_url = object_url(&object_key);
    let upload_path = temp_path(&format!("rusty_cat_aliyun_upload/{file_name}"));
    let download_path = temp_path(&format!("rusty_cat_aliyun_download/{file_name}"));
    remove_if_exists(&upload_path)?;
    remove_if_exists(&download_path)?;
    make_file(&upload_path, UPLOAD_FILE_SIZE)?;
    let client = MeowClient::new(config()?);
    let upload_protocol = AliyunDirectUpload {
        session: Arc::new(Mutex::new(MultipartSession::default())),
    };
    let upload_task = UploadPounceBuilder::new("aliyun-direct-upload.bin", &upload_path, ONE_MB)
        .with_url(object_url.clone())
        .with_breakpoint_upload(Arc::new(upload_protocol))
        .build()?;
    println!("aliyun direct upload url: {object_url}");
    run_task_pause_once_at(
        &client,
        upload_task,
        "aliyun direct upload",
        PAUSE_PROGRESS,
        Duration::from_secs(PAUSE_SECONDS),
    )
    .await?;
    let download_task = DownloadPounceBuilder::new(
        "aliyun-direct-download.bin",
        &download_path,
        ONE_MB,
        object_url.clone(),
    )
    .with_breakpoint_download(Arc::new(AliyunDirectDownload))
    .build();
    println!("aliyun direct download url: {object_url}");
    run_task_pause_once_at(
        &client,
        download_task,
        "aliyun direct download",
        PAUSE_PROGRESS,
        Duration::from_secs(PAUSE_SECONDS),
    )
    .await?;
    client.close().await?;
    remove_if_exists(&upload_path)?;
    remove_if_exists(&download_path)?;
    Ok(())
}
