#[path = "dev_server/mod.rs"]
mod dev_server;

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::Method;
use rusty_cat::down_pounce_builder::DownloadPounceBuilder;
use rusty_cat::error::InnerErrorCode;
use rusty_cat::file_transfer_record::FileTransferRecord;
use rusty_cat::meow_config::MeowConfig;
use rusty_cat::transfer_status::TransferStatus;
use rusty_cat::up_pounce_builder::UploadPounceBuilder;
use rusty_cat::MeowClient;

fn temp_path(case: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    p.push(format!("rusty_cat_protocol_edge_{case}_{ts}.bin"));
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

#[tokio::test]
async fn upload_prepare_http_non_success_hits_response_status_error() {
    // 场景说明：
    // 1) 上传 prepare 请求返回 HTTP 500；
    // 2) 任务应直接失败且错误码为 ResponseStatusError；
    // 3) 覆盖 upload_prepare 的非成功状态分支。
    let server = dev_server::ScriptedServer::spawn_download(vec![
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 3\r\nConnection: close\r\n\r\nbad"
            .to_string(),
    ]);
    let src = temp_path("upload_prepare_500_src");
    fs::write(&src, b"abc").expect("write upload source");

    let client = MeowClient::new(MeowConfig::new(1, 1));
    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();
    let task = UploadPounceBuilder::new("u.bin", &src, 1024)
        .with_url(format!("{}/upload/u.bin", server.base_url()))
        .build()
        .expect("build upload task");
    client
        .enqueue(task, move |record: FileTransferRecord| {
            statuses_cb
                .lock()
                .expect("lock statuses")
                .push(record.status().clone());
        }, Some(|_, _| {}))
        .await
        .expect("enqueue upload task");

    let terminal = wait_terminal_status(statuses).await;
    match terminal {
        TransferStatus::Failed(err) => {
            assert_eq!(err.code(), InnerErrorCode::ResponseStatusError as i32)
        }
        other => panic!("expected failed status, got {other:?}"),
    }

    client.close().await.expect("close client");
    fs::remove_file(&src).expect("remove upload source");
    server.shutdown();
}

#[tokio::test]
async fn upload_prepare_invalid_json_hits_response_parse_error() {
    // 场景说明：
    // 1) 上传 prepare 返回 200，但 body 非法 JSON；
    // 2) 默认上传协议解析应失败并返回 ResponseParseError；
    // 3) 覆盖 parse_upload_response 错误分支。
    let server = dev_server::ScriptedServer::spawn_download(vec![
        "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nnot-json".to_string(),
    ]);
    let src = temp_path("upload_prepare_bad_json_src");
    fs::write(&src, b"abc").expect("write upload source");

    let client = MeowClient::new(MeowConfig::new(1, 1));
    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();
    let task = UploadPounceBuilder::new("u.bin", &src, 1024)
        .with_url(format!("{}/upload/u.bin", server.base_url()))
        .build()
        .expect("build upload task");
    client
        .enqueue(task, move |record: FileTransferRecord| {
            statuses_cb
                .lock()
                .expect("lock statuses")
                .push(record.status().clone());
        }, Some(|_, _| {}))
        .await
        .expect("enqueue upload task");

    let terminal = wait_terminal_status(statuses).await;
    match terminal {
        TransferStatus::Failed(err) => {
            assert_eq!(err.code(), InnerErrorCode::ResponseParseError as i32)
        }
        other => panic!("expected failed status, got {other:?}"),
    }

    client.close().await.expect("close client");
    fs::remove_file(&src).expect("remove upload source");
    server.shutdown();
}

#[tokio::test]
async fn upload_prepare_completed_file_id_branch_finishes_without_chunk_http() {
    // 场景说明：
    // 1) 上传 prepare 返回 fileId，表示服务端已完成文件；
    // 2) 客户端应直接进入 Complete，不再发后续分片 HTTP；
    // 3) 覆盖 upload_prepare 的 completed_file_id 分支。
    let body = "{\"fileId\":\"x\",\"nextByte\":999999}";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let server = dev_server::ScriptedServer::spawn_download(vec![response]);
    let src = temp_path("upload_prepare_completed_src");
    fs::write(&src, b"abcdef").expect("write upload source");

    let client = MeowClient::new(MeowConfig::new(1, 1));
    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();
    let task = UploadPounceBuilder::new("u.bin", &src, 1024)
        .with_url(format!("{}/upload/u.bin", server.base_url()))
        .build()
        .expect("build upload task");
    client
        .enqueue(task, move |record: FileTransferRecord| {
            statuses_cb
                .lock()
                .expect("lock statuses")
                .push(record.status().clone());
        }, Some(|_, _| {}))
        .await
        .expect("enqueue upload task");

    let terminal = wait_terminal_status(statuses.clone()).await;
    assert!(
        matches!(terminal, TransferStatus::Complete),
        "prepare fileId branch should complete directly, got {terminal:?}"
    );

    client.close().await.expect("close client");
    fs::remove_file(&src).expect("remove upload source");
    server.shutdown();
}

#[tokio::test]
async fn download_prepare_missing_content_length_hits_error_branch() {
    // 场景说明：
    // 1) HEAD 返回 200 但缺失 Content-Length；
    // 2) 下载 prepare 应返回 MissingOrInvalidContentLengthFromHead；
    // 3) 覆盖 total_size_from_head 的错误分支。
    let server = dev_server::ScriptedServer::spawn_download(vec![
        "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_string(),
    ]);
    let path = temp_path("download_head_no_len");
    let client = MeowClient::new(MeowConfig::new(1, 1));
    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();
    let task = DownloadPounceBuilder::new(
        "d.bin",
        &path,
        1024,
        format!("{}/download/d.bin", server.base_url()),
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
        .expect("enqueue download task");

    let terminal = wait_terminal_status(statuses).await;
    match terminal {
        TransferStatus::Failed(err) => {
            assert_eq!(
                err.code(),
                InnerErrorCode::MissingOrInvalidContentLengthFromHead as i32
            )
        }
        other => panic!("expected failed status, got {other:?}"),
    }

    client.close().await.expect("close client");
    let _ = fs::remove_file(&path);
    server.shutdown();
}

#[tokio::test]
async fn download_prepare_local_larger_than_remote_hits_invalid_range_branch() {
    // 场景说明：
    // 1) 本地文件预先写入 16 字节，HEAD 只返回 8 字节；
    // 2) prepare 阶段应识别“本地大于远端”并返回 InvalidRange；
    // 3) 覆盖 download_prepare 的 start > total 分支。
    let server = dev_server::ScriptedServer::spawn_download(vec![
        "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\n".to_string(),
    ]);
    let path = temp_path("download_local_gt_remote");
    fs::write(&path, vec![0u8; 16]).expect("write oversized local file");

    let client = MeowClient::new(MeowConfig::new(1, 1));
    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();
    let task = DownloadPounceBuilder::new(
        "d.bin",
        &path,
        1024,
        format!("{}/download/d.bin", server.base_url()),
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
        .expect("enqueue download task");

    let terminal = wait_terminal_status(statuses).await;
    match terminal {
        TransferStatus::Failed(err) => {
            assert_eq!(err.code(), InnerErrorCode::InvalidRange as i32)
        }
        other => panic!("expected failed status, got {other:?}"),
    }

    client.close().await.expect("close client");
    fs::remove_file(&path).expect("remove local file");
    server.shutdown();
}

#[tokio::test]
async fn download_chunk_total_changed_and_empty_body_error_branches() {
    // 场景说明：
    // 1) case A: HEAD total=8，但 GET Content-Range total=9，应命中“总长度变化”分支；
    // 2) case B: HEAD total=8，GET 返回 206 + content-range 但 body 为空，应命中空 body 分支；
    // 3) 用同一测试覆盖两个下载 chunk 关键错误分支。

    // case A
    let server_a = dev_server::ScriptedServer::spawn_download(vec![
        "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\n".to_string(),
        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-3/9\r\nContent-Length: 4\r\nConnection: close\r\n\r\nabcd".to_string(),
    ]);
    let path_a = temp_path("download_total_changed");
    let client_a = MeowClient::new(MeowConfig::new(1, 1));
    let statuses_a: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_a_cb = statuses_a.clone();
    let task_a = DownloadPounceBuilder::new(
        "a.bin",
        &path_a,
        4,
        format!("{}/download/a.bin", server_a.base_url()),
        Method::GET,
    )
    .build();
    client_a
        .enqueue(task_a, move |record: FileTransferRecord| {
            statuses_a_cb
                .lock()
                .expect("lock statuses a")
                .push(record.status().clone());
        }, Some(|_, _| {}))
        .await
        .expect("enqueue task a");
    let terminal_a = wait_terminal_status(statuses_a).await;
    match terminal_a {
        TransferStatus::Failed(err) => assert_eq!(err.code(), InnerErrorCode::InvalidRange as i32),
        other => panic!("expected failed status for case A, got {other:?}"),
    }
    client_a.close().await.expect("close client a");
    let _ = fs::remove_file(&path_a);
    server_a.shutdown();

    // case B
    let server_b = dev_server::ScriptedServer::spawn_download(vec![
        "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\n".to_string(),
        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-3/8\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
    ]);
    let path_b = temp_path("download_empty_body");
    let client_b = MeowClient::new(MeowConfig::new(1, 1));
    let statuses_b: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_b_cb = statuses_b.clone();
    let task_b = DownloadPounceBuilder::new(
        "b.bin",
        &path_b,
        4,
        format!("{}/download/b.bin", server_b.base_url()),
        Method::GET,
    )
    .build();
    client_b
        .enqueue(task_b, move |record: FileTransferRecord| {
            statuses_b_cb
                .lock()
                .expect("lock statuses b")
                .push(record.status().clone());
        }, Some(|_, _| {}))
        .await
        .expect("enqueue task b");
    let terminal_b = wait_terminal_status(statuses_b).await;
    match terminal_b {
        TransferStatus::Failed(err) => assert_eq!(err.code(), InnerErrorCode::InvalidRange as i32),
        other => panic!("expected failed status for case B, got {other:?}"),
    }
    client_b.close().await.expect("close client b");
    let _ = fs::remove_file(&path_b);
    server_b.shutdown();
}
