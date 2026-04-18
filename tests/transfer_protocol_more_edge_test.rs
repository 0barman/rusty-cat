#[path = "dev_server/mod.rs"]
mod dev_server;

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::Method;
use rusty_cat::down_pounce_builder::DownloadPounceBuilder;
use rusty_cat::file_transfer_record::FileTransferRecord;
use rusty_cat::meow_config::MeowConfig;
use rusty_cat::transfer_status::TransferStatus;
use rusty_cat::MeowClient;

fn temp_path(case: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    p.push(format!("rusty_cat_more_edge_{case}_{ts}.bin"));
    p
}

async fn run_download_case_with_responses(
    case_name: &str,
    responses: Vec<String>,
    init_local: Option<&[u8]>,
) -> TransferStatus {
    let server = dev_server::ScriptedServer::spawn_download(responses);
    let path = temp_path(case_name);
    if let Some(bytes) = init_local {
        fs::write(&path, bytes).expect("write initial local bytes");
    }
    let client = MeowClient::new(MeowConfig::new(1, 1));
    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();

    let task = DownloadPounceBuilder::new(
        "edge.bin",
        &path,
        4,
        format!("{}/download/edge.bin", server.base_url()),
        Method::GET,
    )
    .build();
    client
        .enqueue(task, move |record: FileTransferRecord| {
            statuses_cb
                .lock()
                .expect("lock statuses")
                .push(record.status().clone());
        }, Some(|_, _| {}))
        .await
        .expect("enqueue edge task");

    let terminal = loop {
        if let Some(last) = statuses.lock().expect("lock statuses").last().cloned() {
            if matches!(
                last,
                TransferStatus::Complete | TransferStatus::Failed(_) | TransferStatus::Canceled
            ) {
                break last;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };

    client.close().await.expect("close client");
    let _ = fs::remove_file(&path);
    server.shutdown();
    terminal
}

#[tokio::test]
async fn download_head_non_success_hits_prepare_status_error_branch() {
    // 场景说明：
    // 1) HEAD 返回 404；
    // 2) prepare 阶段应失败并走 ResponseStatusError；
    // 3) 覆盖 download_prepare 的 `!status.is_success()` 分支。
    let status = run_download_case_with_responses(
        "head_404",
        vec![
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
        ],
        None,
    )
    .await;

    match status {
        TransferStatus::Failed(err) => assert!(
            err.msg().contains("HEAD failed"),
            "expect HEAD failed msg, got {}",
            err.msg()
        ),
        other => panic!("expected failed status, got {other:?}"),
    }
}

#[tokio::test]
async fn download_invalid_content_range_unit_and_order_branches() {
    // 场景说明：
    // 1) case A: content-range unit 非 bytes，触发 parse_content_range unit 错误分支；
    // 2) case B: content-range end < start，触发 invalid order 分支；
    // 3) 两个分支都应进入 Failed(InvalidRange)。

    // case A: invalid unit
    let status_a = run_download_case_with_responses(
        "invalid_unit",
        vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\n".to_string(),
            "HTTP/1.1 206 Partial Content\r\nContent-Range: items 0-3/8\r\nContent-Length: 4\r\nConnection: close\r\n\r\nabcd".to_string(),
        ],
        Some(b""),
    )
    .await;
    match status_a {
        TransferStatus::Failed(err) => assert!(
            err.msg().contains("invalid content-range"),
            "expected invalid content-range message, got {}",
            err.msg()
        ),
        other => panic!("expected failed status for invalid unit, got {other:?}"),
    }

    // case B: invalid order
    let status_b = run_download_case_with_responses(
        "invalid_order",
        vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\n".to_string(),
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 3-0/8\r\nContent-Length: 4\r\nConnection: close\r\n\r\nabcd".to_string(),
        ],
        Some(b""),
    )
    .await;
    match status_b {
        TransferStatus::Failed(err) => assert!(
            err.msg().contains("invalid content-range order"),
            "expected invalid content-range order message, got {}",
            err.msg()
        ),
        other => panic!("expected failed status for invalid order, got {other:?}"),
    }
}

#[tokio::test]
async fn download_body_length_mismatch_branch_is_detected() {
    // 场景说明：
    // 1) HEAD 声明总长 8；
    // 2) GET 的 Content-Range 声明 0-3 共 4 字节，但 body 实际只给 3 字节；
    // 3) 应触发 body length mismatch 分支并失败。
    let status = run_download_case_with_responses(
        "body_len_mismatch",
        vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\n".to_string(),
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-3/8\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc".to_string(),
        ],
        Some(b""),
    )
    .await;

    match status {
        TransferStatus::Failed(err) => assert!(
            err.msg().contains("body length mismatch"),
            "expected body length mismatch message, got {}",
            err.msg()
        ),
        other => panic!("expected failed status, got {other:?}"),
    }
}
