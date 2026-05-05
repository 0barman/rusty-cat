//! 对外公开 API 的非法入参 / 非法调用顺序回归测试。
//!
//! 覆盖范围（与既有用例分工）：
//! - [`MeowClient::try_enqueue`]：`PounceTask::is_empty` 触发的 `ParameterEmpty`（下载/上传字段缺失、0 字节上传源等）。
//! - [`MeowClient`] 生命周期：`ClientClosed`、`is_closed`、重复 `close`；关闭后对 `pause`/`resume`/`cancel`/`snapshot` 的拒绝。
//! - [`UploadPounceBuilder::build`]：源文件不存在时的 `io::ErrorKind::NotFound`。
//! - [`MeowConfig`]：`max_upload_concurrency`/`max_download_concurrency` 为 0 时仍可初始化；`command_queue_capacity` /
//!   `worker_event_queue_capacity` 为 0 时当前实现会在首次拉起 executor 时 panic（见 `#[should_panic]` 用例）。
//! - 全局 API：`clear_global_listener`。
//!
//! 以下能力在其它集成测试中已有覆盖，此处不重复：`TaskNotFound`（`task_not_found_test`）、
//! `DuplicateTaskError` / `InvalidTaskState`（`meow_client_duplicate_and_state_test`）、
//! 重复注销监听器 id（`meow_client_listener_misc_test`）、
//! `try_set_debug_log_listener` 重复注册（`log_facade_test`）。

use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use rusty_cat::down_pounce_builder::DownloadPounceBuilder;
use rusty_cat::error::InnerErrorCode;
use rusty_cat::file_transfer_record::FileTransferRecord;
use rusty_cat::meow_config::MeowConfig;
use rusty_cat::up_pounce_builder::UploadPounceBuilder;
use rusty_cat::MeowClient;

fn temp_path(case: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    p.push(format!("rusty_cat_public_api_invalid_{case}_{ts}.bin"));
    p
}

// --- MeowClient::try_enqueue + PounceTask::is_empty ---

#[tokio::test]
async fn enqueue_download_empty_file_name_returns_parameter_empty() {
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(1)
            .build()
            .expect("valid config"),
    );
    let task =
        DownloadPounceBuilder::new("", temp_path("empty_name"), 1024, "http://127.0.0.1:9/x")
            .build();
    let err = client
        .try_enqueue(task, |_r: FileTransferRecord| {}, |_, _| {})
        .await
        .expect_err("empty download display name");
    assert_eq!(err.code(), InnerErrorCode::ParameterEmpty as i32);
    client.close().await.expect("close");
}

#[tokio::test]
async fn enqueue_download_empty_url_returns_parameter_empty() {
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(1)
            .build()
            .expect("valid config"),
    );
    let task = DownloadPounceBuilder::new("a.bin", temp_path("empty_url"), 1024, "").build();
    let err = client
        .try_enqueue(task, |_r: FileTransferRecord| {}, |_, _| {})
        .await
        .expect_err("empty download url");
    assert_eq!(err.code(), InnerErrorCode::ParameterEmpty as i32);
    client.close().await.expect("close");
}

#[tokio::test]
async fn enqueue_download_empty_local_path_returns_parameter_empty() {
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(1)
            .build()
            .expect("valid config"),
    );
    let task = DownloadPounceBuilder::new("a.bin", "", 1024, "http://127.0.0.1:9/x").build();
    let err = client
        .try_enqueue(task, |_r: FileTransferRecord| {}, |_, _| {})
        .await
        .expect_err("empty local path");
    assert_eq!(err.code(), InnerErrorCode::ParameterEmpty as i32);
    client.close().await.expect("close");
}

#[tokio::test]
async fn enqueue_upload_empty_url_returns_parameter_empty() {
    let src = temp_path("upload_empty_url");
    fs::write(&src, b"x").expect("write source");
    let task = UploadPounceBuilder::new("up.bin", &src, 1024)
        .with_url("")
        .build()
        .expect("build upload task");
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(1)
            .build()
            .expect("valid config"),
    );
    let err = client
        .try_enqueue(task, |_r: FileTransferRecord| {}, |_, _| {})
        .await
        .expect_err("empty upload url");
    assert_eq!(err.code(), InnerErrorCode::ParameterEmpty as i32);
    let _ = fs::remove_file(&src);
    client.close().await.expect("close");
}

#[tokio::test]
async fn enqueue_upload_zero_byte_file_returns_parameter_empty() {
    let src = temp_path("upload_zero_bytes");
    fs::write(&src, []).expect("write empty file");
    let task = UploadPounceBuilder::new("zero.bin", &src, 1024)
        .with_url("http://127.0.0.1:9/up")
        .build()
        .expect("build upload task");
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(1)
            .build()
            .expect("valid config"),
    );
    let err = client
        .try_enqueue(task, |_r: FileTransferRecord| {}, |_, _| {})
        .await
        .expect_err("upload with total_size 0");
    assert_eq!(err.code(), InnerErrorCode::ParameterEmpty as i32);
    let _ = fs::remove_file(&src);
    client.close().await.expect("close");
}

#[tokio::test]
async fn enqueue_upload_empty_file_name_returns_parameter_empty() {
    let src = temp_path("upload_empty_fname");
    fs::write(&src, b"x").expect("write source");
    let task = UploadPounceBuilder::new("", &src, 1024)
        .with_url("http://127.0.0.1:9/up")
        .build()
        .expect("build upload task");
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(1)
            .build()
            .expect("valid config"),
    );
    let err = client
        .try_enqueue(task, |_r: FileTransferRecord| {}, |_, _| {})
        .await
        .expect_err("empty upload file name");
    assert_eq!(err.code(), InnerErrorCode::ParameterEmpty as i32);
    let _ = fs::remove_file(&src);
    client.close().await.expect("close");
}

#[tokio::test]
async fn enqueue_upload_empty_source_path_returns_parameter_empty() {
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(1)
            .build()
            .expect("valid config"),
    );
    let task = match UploadPounceBuilder::new("up.bin", PathBuf::new(), 1024)
        .with_url("http://127.0.0.1:9/up")
        .build()
    {
        Ok(t) => t,
        Err(e) => {
            assert!(
                matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::InvalidInput
                ),
                "unexpected io error building upload task with empty path: {e:?}"
            );
            client.close().await.expect("close");
            return;
        }
    };
    let err = client
        .try_enqueue(task, |_r: FileTransferRecord| {}, |_, _| {})
        .await
        .expect_err("empty upload source path");
    assert_eq!(err.code(), InnerErrorCode::ParameterEmpty as i32);
    client.close().await.expect("close");
}

// --- UploadPounceBuilder::build (std::io::Error) ---

#[test]
fn upload_builder_build_missing_file_returns_not_found_io_error() {
    let missing = temp_path("upload_missing_for_build");
    let err = UploadPounceBuilder::new("nope.bin", &missing, 1024)
        .with_url("http://127.0.0.1:9/x")
        .build()
        .expect_err("metadata on missing file");
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
}

// --- MeowClient 关闭后 API ---

#[tokio::test]
async fn enqueue_after_client_close_returns_client_closed() {
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(1)
            .build()
            .expect("valid config"),
    );
    client.close().await.expect("close");
    let task = DownloadPounceBuilder::new(
        "late.bin",
        temp_path("late"),
        1024,
        "http://127.0.0.1:9/late",
    )
    .build();
    let err = client
        .try_enqueue(task, |_r: FileTransferRecord| {}, |_, _| {})
        .await
        .expect_err("enqueue after close");
    assert_eq!(err.code(), InnerErrorCode::ClientClosed as i32);
}

#[tokio::test]
async fn snapshot_after_client_close_returns_client_closed() {
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(1)
            .build()
            .expect("valid config"),
    );
    client.close().await.expect("close");
    let err = client.snapshot().await.expect_err("snapshot after close");
    assert_eq!(err.code(), InnerErrorCode::ClientClosed as i32);
}

#[tokio::test]
async fn second_close_returns_client_closed() {
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(1)
            .build()
            .expect("valid config"),
    );
    client.close().await.expect("first close");
    let err = client.close().await.expect_err("second close");
    assert_eq!(err.code(), InnerErrorCode::ClientClosed as i32);
}

#[tokio::test]
async fn is_closed_reflects_successful_close() {
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(1)
            .build()
            .expect("valid config"),
    );
    assert!(!client.is_closed());
    client.close().await.expect("close");
    assert!(client.is_closed());
}

// --- 全局进度监听器：空列表 clear（合法但覆盖 API 表面）---

#[test]
fn clear_global_listener_on_fresh_client_succeeds() {
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(1)
            .build()
            .expect("valid config"),
    );
    client.clear_global_listener().expect("clear when empty");
}

// --- MeowConfig：队列容量为 0 ---
// `MeowConfig` 通过 builder 的 build() 集中硬校验，非法值应在构造期返回 MeowError，
// 不能延迟到 executor 初始化时由 Tokio panic。

#[test]
fn meow_config_zero_command_queue_capacity_is_rejected_by_builder() {
    let err = MeowConfig::builder()
        .command_queue_capacity(0)
        .build()
        .expect_err("zero command queue capacity must be rejected");
    assert_eq!(err.code(), InnerErrorCode::ParameterEmpty as i32);
}

#[test]
fn meow_config_zero_worker_event_queue_capacity_is_rejected_by_builder() {
    let err = MeowConfig::builder()
        .worker_event_queue_capacity(0)
        .build()
        .expect_err("zero worker event queue capacity must be rejected");
    assert_eq!(err.code(), InnerErrorCode::ParameterEmpty as i32);
}

#[test]
fn meow_config_zero_transfer_concurrency_is_rejected_by_builder() {
    let err = MeowConfig::builder()
        .max_upload_concurrency(0)
        .max_download_concurrency(1)
        .build()
        .expect_err("zero upload concurrency must be rejected");
    assert_eq!(err.code(), InnerErrorCode::ParameterEmpty as i32);

    let err = MeowConfig::builder()
        .max_upload_concurrency(1)
        .max_download_concurrency(0)
        .build()
        .expect_err("zero download concurrency must be rejected");
    assert_eq!(err.code(), InnerErrorCode::ParameterEmpty as i32);
}

#[tokio::test]
async fn control_apis_after_close_return_client_closed_even_with_prior_task_id() {
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(1)
            .build()
            .expect("valid config"),
    );
    let task = DownloadPounceBuilder::new(
        "ctrl-after-close.bin",
        temp_path("ctrl_after_close"),
        1024,
        "http://127.0.0.1:9/unused",
    )
    .build();
    let task_id = client
        .try_enqueue(task, |_r: FileTransferRecord| {}, |_, _| {})
        .await
        .expect("enqueue");

    client.close().await.expect("close");

    let pause_err = client.pause(task_id).await.expect_err("pause after close");
    assert_eq!(pause_err.code(), InnerErrorCode::ClientClosed as i32);

    let resume_err = client
        .resume(task_id)
        .await
        .expect_err("resume after close");
    assert_eq!(resume_err.code(), InnerErrorCode::ClientClosed as i32);

    let cancel_err = client
        .cancel(task_id)
        .await
        .expect_err("cancel after close");
    assert_eq!(cancel_err.code(), InnerErrorCode::ClientClosed as i32);
}
