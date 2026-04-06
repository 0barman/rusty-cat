#[path = "dev_server/mod.rs"]
mod dev_server;

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::Method;
use rusty_cat::down_pounce_builder::DownloadPounceBuilder;
use rusty_cat::error::InnerErrorCode;
use rusty_cat::file_transfer_record::FileTransferRecord;
use rusty_cat::meow_config::MeowConfig;
use rusty_cat::MeowClient;

fn temp_path(case: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    p.push(format!("rusty_cat_executor_queue_{case}_{ts}.bin"));
    p
}

#[tokio::test]
async fn zero_concurrency_queue_pause_resume_cancel_flow() {
    // 场景说明：
    // 1) 使用并发=0（允许任务入队但不会启动执行）；
    // 2) 验证 snapshot 先看到 queued=1 active=0；
    // 3) pause 后 queued 清空；resume 后重新入队；cancel 后任务被移除；
    // 4) 再次 cancel 应返回 TaskNotFound，覆盖队列控制边界分支。
    let payload = b"queue-control-payload".repeat(1024);
    let server = dev_server::DevFileServer::spawn(payload);
    let client = MeowClient::new(MeowConfig::new(0, 0));
    let path = temp_path("zero_concurrency");

    let task = DownloadPounceBuilder::new(
        "q.bin",
        &path,
        1024,
        format!("{}/download/q.bin", server.base_url()),
        Method::GET,
    )
    .build();
    let task_id = client
        .enqueue(task, |_record: FileTransferRecord| {})
        .await
        .expect("enqueue queued task");

    let snap1 = client.snapshot().await.expect("snapshot after enqueue");
    assert_eq!(snap1.queued_groups, 1, "task should be queued");
    assert_eq!(
        snap1.active_groups, 0,
        "task should not start with zero concurrency"
    );

    client.pause(task_id).await.expect("pause queued task");
    let snap2 = client.snapshot().await.expect("snapshot after pause");
    assert_eq!(
        snap2.queued_groups, 0,
        "paused task should be removed from queue"
    );
    assert_eq!(snap2.active_groups, 0, "paused task should not be active");

    client
        .resume(task_id)
        .await
        .expect("resume paused queued task");
    let snap3 = client.snapshot().await.expect("snapshot after resume");
    assert_eq!(snap3.queued_groups, 1, "resumed task should be re-queued");
    assert_eq!(
        snap3.active_groups, 0,
        "still zero concurrency, so inactive"
    );

    client.cancel(task_id).await.expect("cancel queued task");
    let snap4 = client.snapshot().await.expect("snapshot after cancel");
    assert_eq!(snap4.queued_groups, 0, "canceled task should leave queue");
    assert_eq!(snap4.active_groups, 0, "canceled task should not be active");

    let err = client
        .cancel(task_id)
        .await
        .expect_err("second cancel should return task not found");
    assert_eq!(err.code(), InnerErrorCode::TaskNotFound as i32);

    client.close().await.expect("close client");
    let _ = fs::remove_file(&path);
    server.shutdown();
}
