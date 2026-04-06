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
use rusty_cat::MeowClient;

fn temp_download_path(case: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    p.push(format!("rusty_cat_meow_lifecycle_{case}_{ts}.bin"));
    p
}

#[tokio::test]
async fn close_without_executor_initialization_marks_client_closed() {
    // 场景说明：
    // 1) 客户端刚创建，还没有 enqueue 过任务，因此内部 executor 尚未初始化；
    // 2) 直接调用 close 应该成功，并将 closed 标记置为 true；
    // 3) 第二次 close 应该返回 ClientClosed，证明 close 语义是可观测且一致的。
    let client = MeowClient::new(MeowConfig::new(1, 1));

    assert!(!client.is_closed().await, "new client must be open");
    client.close().await.expect("first close should succeed");
    assert!(
        client.is_closed().await,
        "client must be closed after close"
    );

    let second_close = client
        .close()
        .await
        .expect_err("second close should be rejected");
    assert_eq!(second_close.code(), InnerErrorCode::ClientClosed as i32);
}

#[tokio::test]
async fn all_control_apis_return_client_closed_after_close() {
    // 场景说明：
    // 1) 先入队一个真实任务拿到 task_id，确保 pause/resume/cancel 有合法入参；
    // 2) 调用 close 关闭客户端；
    // 3) 对 enqueue/pause/resume/cancel/snapshot 全部再次调用，统一期望 ClientClosed。
    let payload = b"meow-lifecycle-payload".repeat(4096);
    let server = dev_server::DevFileServer::spawn(payload);
    let client = MeowClient::new(MeowConfig::new(1, 1));
    let path = temp_download_path("after_close");

    let task = DownloadPounceBuilder::new(
        "lifecycle.bin",
        &path,
        2048,
        format!("{}/download/lifecycle.bin", server.base_url()),
        Method::GET,
    )
    .build();
    let task_id = client
        .enqueue(task, |_record: FileTransferRecord| {})
        .await
        .expect("enqueue initial task");

    client.close().await.expect("close should succeed");
    assert!(client.is_closed().await, "client should be closed now");

    let new_path = temp_download_path("enqueue_closed");
    let enqueue_after_close = client
        .enqueue(
            DownloadPounceBuilder::new(
                "closed.bin",
                &new_path,
                1024,
                format!("{}/download/closed.bin", server.base_url()),
                Method::GET,
            )
            .build(),
            |_record: FileTransferRecord| {},
        )
        .await
        .expect_err("enqueue after close must fail");
    assert_eq!(
        enqueue_after_close.code(),
        InnerErrorCode::ClientClosed as i32
    );

    let pause_after_close = client
        .pause(task_id)
        .await
        .expect_err("pause after close must fail");
    assert_eq!(
        pause_after_close.code(),
        InnerErrorCode::ClientClosed as i32
    );

    let resume_after_close = client
        .resume(task_id)
        .await
        .expect_err("resume after close must fail");
    assert_eq!(
        resume_after_close.code(),
        InnerErrorCode::ClientClosed as i32
    );

    let cancel_after_close = client
        .cancel(task_id)
        .await
        .expect_err("cancel after close must fail");
    assert_eq!(
        cancel_after_close.code(),
        InnerErrorCode::ClientClosed as i32
    );

    let snapshot_after_close = client
        .snapshot()
        .await
        .expect_err("snapshot after close must fail");
    assert_eq!(
        snapshot_after_close.code(),
        InnerErrorCode::ClientClosed as i32
    );

    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(&new_path);
    server.shutdown();
}

#[tokio::test]
async fn close_active_transfer_should_emit_pause_or_terminal_status() {
    // 场景说明：
    // 1) 启动一个较大下载任务并等待其进入传输态；
    // 2) 在任务执行中调用 close；
    // 3) close 后应至少观测到 Paused/Complete/Canceled/Failed 之一，证明状态可收敛可观测。
    // 说明：close 流程会主动发 Paused 并清理任务，不强制要求固定终态。
    let payload = b"meow-close-active".repeat(8192);
    let server = dev_server::DevFileServer::spawn(payload);
    let client = MeowClient::new(MeowConfig::new(1, 1));
    let path = temp_download_path("close_active");
    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();

    let task = DownloadPounceBuilder::new(
        "active.bin",
        &path,
        1024,
        format!("{}/download/active.bin", server.base_url()),
        Method::GET,
    )
    .build();
    client
        .enqueue(task, move |record: FileTransferRecord| {
            statuses_cb
                .lock()
                .expect("lock statuses")
                .push(record.status().clone());
        })
        .await
        .expect("enqueue active task");

    for _ in 0..100 {
        let seen_transmission = statuses
            .lock()
            .expect("lock statuses")
            .iter()
            .any(|s| matches!(s, TransferStatus::Transmission));
        if seen_transmission {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    client.close().await.expect("close client during transfer");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let observed = statuses.lock().expect("lock statuses after close").clone();
    assert!(
        observed.iter().any(|s| {
            matches!(
                s,
                TransferStatus::Paused
                    | TransferStatus::Canceled
                    | TransferStatus::Failed(_)
                    | TransferStatus::Complete
            )
        }),
        "after close there should be at least one paused/terminal status observable"
    );

    let _ = fs::remove_file(&path);
    server.shutdown();
}
