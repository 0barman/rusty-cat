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
    p.push(format!("rusty_cat_meow_enqueue_snapshot_{case}_{ts}.bin"));
    p
}

#[tokio::test]
async fn enqueue_rejects_empty_task_with_parameter_empty() {
    // 场景说明：
    // 1) 构造一个 file_name/url 为空的下载任务；
    // 2) 调用 enqueue 应立即失败（不依赖网络），错误码必须是 ParameterEmpty；
    // 3) 该用例保证输入校验边界稳定，避免空任务进入调度层。
    let client = MeowClient::new(MeowConfig::new(1, 1));
    let path = temp_download_path("empty_task");
    let empty_task = DownloadPounceBuilder::new("", &path, 1024, "", Method::GET).build();

    let err = client
        .enqueue(empty_task, |_record: FileTransferRecord| {})
        .await
        .expect_err("empty task should be rejected");
    assert_eq!(err.code(), InnerErrorCode::ParameterEmpty as i32);
    assert!(
        !client.is_closed().await,
        "client should stay open on bad input"
    );
}

#[tokio::test]
async fn snapshot_reports_activity_and_returns_to_zero_after_completion() {
    // 场景说明：
    // 1) 启动真实下载任务并在传输期间轮询 snapshot；
    // 2) 至少观测到一次 active_groups > 0（证明 snapshot 能反映运行态）；
    // 3) 任务完成后 snapshot 应回到 active=0/queued=0（证明状态能收敛）。
    let payload = b"snapshot-observe-payload".repeat(4096);
    let server = dev_server::DevFileServer::spawn(payload.clone());
    let client = MeowClient::new(MeowConfig::new(1, 1));
    let path = temp_download_path("snapshot");
    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();

    let task = DownloadPounceBuilder::new(
        "snapshot.bin",
        &path,
        1024,
        format!("{}/download/snapshot.bin", server.base_url()),
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
        .expect("enqueue snapshot task");

    let mut saw_active = false;
    for _ in 0..200 {
        let snap = client.snapshot().await.expect("snapshot during transfer");
        if snap.active_groups > 0 {
            saw_active = true;
        }
        let done = statuses
            .lock()
            .expect("lock statuses")
            .iter()
            .any(|s| matches!(s, TransferStatus::Complete));
        if done {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        saw_active,
        "snapshot should observe at least one active group"
    );

    for _ in 0..200 {
        let snap = client.snapshot().await.expect("snapshot when draining");
        if snap.active_groups == 0 && snap.queued_groups == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let final_snap = client.snapshot().await.expect("final snapshot");
    assert_eq!(
        final_snap.active_groups, 0,
        "final active groups must be zero"
    );
    assert_eq!(
        final_snap.queued_groups, 0,
        "final queued groups must be zero"
    );

    client.close().await.expect("close client");
    let bytes = fs::read(&path).expect("read snapshot result file");
    assert_eq!(bytes, payload, "download content should remain correct");
    fs::remove_file(&path).expect("remove temp snapshot file");
    server.shutdown();
}
