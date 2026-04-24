#[path = "dev_server/mod.rs"]
mod dev_server;

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

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
    p.push(format!("rusty_cat_meow_dup_state_{case}_{ts}.bin"));
    p
}

#[tokio::test]
async fn second_enqueue_with_same_download_url_hits_duplicate_branch() {
    // 场景说明：
    // 1) 连续 enqueue 两个下载任务，故意使用相同 URL（命中 dedupe key）；
    // 2) 第一任务正常执行，第二任务 enqueue 返回 task_id，但回调状态应为 Failed(DuplicateTaskError)；
    // 3) 该用例覆盖 worker 里的 duplicate 分支行为。
    let payload = b"duplicate-branch-payload".repeat(8192);
    let server = dev_server::DevFileServer::spawn(payload);
    let client = MeowClient::new(MeowConfig::new(1, 1));

    let url = format!("{}/download/dup.bin", server.base_url());
    let path1 = temp_download_path("dup1");
    let path2 = temp_download_path("dup2");

    let statuses1: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses2: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let s1 = statuses1.clone();
    let s2 = statuses2.clone();

    let task1 = DownloadPounceBuilder::new("dup.bin", &path1, 1024, url.clone()).build();
    let task2 = DownloadPounceBuilder::new("dup.bin", &path2, 1024, url).build();

    client
        .try_enqueue(
            task1,
            move |record: FileTransferRecord| {
                s1.lock()
                    .expect("lock statuses1")
                    .push(record.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("enqueue first task");
    client
        .try_enqueue(
            task2,
            move |record: FileTransferRecord| {
                s2.lock()
                    .expect("lock statuses2")
                    .push(record.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("enqueue second task should still return task id");

    let mut seen_duplicate = false;
    for _ in 0..200 {
        seen_duplicate = statuses2.lock().expect("lock statuses2").iter().any(|s| {
            matches!(
                s,
                TransferStatus::Failed(err) if err.code() == InnerErrorCode::DuplicateTaskError as i32
            )
        });
        if seen_duplicate {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        seen_duplicate,
        "second enqueue should produce DuplicateTaskError in callback path"
    );

    client.close().await.expect("close client");
    let _ = fs::remove_file(&path1);
    let _ = fs::remove_file(&path2);
    server.shutdown();
}

#[tokio::test]
async fn resume_without_pause_hits_invalid_task_state_branch() {
    // 场景说明：
    // 1) 入队一个真实运行中的任务（task_id 存在且未 pause）；
    // 2) 直接调用 resume（逻辑上非法）；
    // 3) 应返回 InvalidTaskState，覆盖 resume_group 中“not paused”分支。
    let payload = b"invalid-state-payload".repeat(8192);
    let server = dev_server::DevFileServer::spawn(payload);
    let client = MeowClient::new(MeowConfig::new(1, 1));
    let path = temp_download_path("invalid_resume");

    let task = DownloadPounceBuilder::new(
        "invalid.bin",
        &path,
        1024,
        format!("{}/download/invalid.bin", server.base_url()),
    )
    .build();
    let task_id = client
        .try_enqueue(task, |_record: FileTransferRecord| {}, |_, _| {})
        .await
        .expect("enqueue task");

    let err = client
        .resume(task_id)
        .await
        .expect_err("resume without pause should fail");
    assert_eq!(err.code(), InnerErrorCode::InvalidTaskState as i32);

    client.close().await.expect("close client");
    let _ = fs::remove_file(&path);
    server.shutdown();
}
