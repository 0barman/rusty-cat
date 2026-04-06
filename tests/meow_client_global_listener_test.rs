#[path = "dev_server/mod.rs"]
mod dev_server;

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::Method;
use rusty_cat::down_pounce_builder::DownloadPounceBuilder;
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
    p.push(format!("rusty_cat_meow_listener_{case}_{ts}.bin"));
    p
}

async fn wait_transfer_done(
    client: &MeowClient,
    path: &PathBuf,
    url: String,
    status_cb: impl Fn(FileTransferRecord) + Send + Sync + 'static,
) {
    let task = DownloadPounceBuilder::new("listener.bin", path, 2048, url, Method::GET).build();
    let _task_id = client
        .enqueue(task, status_cb)
        .await
        .expect("enqueue listener test task");

    for _ in 0..300 {
        let snap = client
            .snapshot()
            .await
            .expect("snapshot during listener test");
        if snap.active_groups == 0 && snap.queued_groups == 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("listener test task did not finish in time");
}

#[tokio::test]
async fn unregister_global_listener_stops_future_events() {
    // 场景说明：
    // 1) 注册两个全局监听器，均应收到第一轮任务的事件；
    // 2) 卸载 listener1 后再跑第二轮任务；
    // 3) listener1 计数应保持不变，listener2 继续增长，证明 unregister 生效。
    let payload = b"listener-unregister-payload".repeat(4096);
    let server = dev_server::DevFileServer::spawn(payload);
    let client = MeowClient::new(MeowConfig::new(1, 1));

    let l1_count = Arc::new(AtomicUsize::new(0));
    let l2_count = Arc::new(AtomicUsize::new(0));
    let l1_c = l1_count.clone();
    let l2_c = l2_count.clone();

    let l1_id = client
        .register_global_progress_listener(move |_record| {
            l1_c.fetch_add(1, Ordering::Relaxed);
        })
        .expect("register listener1");
    let _l2_id = client
        .register_global_progress_listener(move |_record| {
            l2_c.fetch_add(1, Ordering::Relaxed);
        })
        .expect("register listener2");

    let first_path = temp_download_path("unregister_round1");
    wait_transfer_done(
        &client,
        &first_path,
        format!("{}/download/a.bin", server.base_url()),
        |_record: FileTransferRecord| {},
    )
    .await;
    let l1_after_round1 = l1_count.load(Ordering::Relaxed);
    let l2_after_round1 = l2_count.load(Ordering::Relaxed);
    assert!(l1_after_round1 > 0, "listener1 should receive events");
    assert!(l2_after_round1 > 0, "listener2 should receive events");

    let removed = client
        .unregister_global_progress_listener(l1_id)
        .expect("unregister listener1");
    assert!(removed, "listener1 should be removable");

    let second_path = temp_download_path("unregister_round2");
    wait_transfer_done(
        &client,
        &second_path,
        format!("{}/download/b.bin", server.base_url()),
        |_record: FileTransferRecord| {},
    )
    .await;

    assert_eq!(
        l1_count.load(Ordering::Relaxed),
        l1_after_round1,
        "listener1 must not receive events after unregister"
    );
    assert!(
        l2_count.load(Ordering::Relaxed) > l2_after_round1,
        "listener2 should still receive events after listener1 removed"
    );

    client.close().await.expect("close client");
    let _ = fs::remove_file(&first_path);
    let _ = fs::remove_file(&second_path);
    server.shutdown();
}

#[tokio::test]
async fn clear_global_listener_removes_all_registered_callbacks() {
    // 场景说明：
    // 1) 注册多个全局监听器；
    // 2) 调用 clear_global_listener 一次性清空；
    // 3) 后续任务执行时，所有全局监听器都不应再收到任何事件。
    let payload = b"listener-clear-payload".repeat(2048);
    let server = dev_server::DevFileServer::spawn(payload);
    let client = MeowClient::new(MeowConfig::new(1, 1));

    let l1_count = Arc::new(AtomicUsize::new(0));
    let l2_count = Arc::new(AtomicUsize::new(0));
    let l1_c = l1_count.clone();
    let l2_c = l2_count.clone();
    client
        .register_global_progress_listener(move |_record| {
            l1_c.fetch_add(1, Ordering::Relaxed);
        })
        .expect("register listener1");
    client
        .register_global_progress_listener(move |_record| {
            l2_c.fetch_add(1, Ordering::Relaxed);
        })
        .expect("register listener2");

    client
        .clear_global_listener()
        .expect("clear all global listeners");

    let path = temp_download_path("clear_all");
    let task_statuses = Arc::new(std::sync::Mutex::new(Vec::<TransferStatus>::new()));
    let statuses_cb = task_statuses.clone();
    wait_transfer_done(
        &client,
        &path,
        format!("{}/download/clear.bin", server.base_url()),
        move |record: FileTransferRecord| {
            statuses_cb
                .lock()
                .expect("lock task statuses")
                .push(record.status().clone());
        },
    )
    .await;

    assert_eq!(
        l1_count.load(Ordering::Relaxed),
        0,
        "listener1 should not receive events after clear"
    );
    assert_eq!(
        l2_count.load(Ordering::Relaxed),
        0,
        "listener2 should not receive events after clear"
    );
    assert!(
        task_statuses
            .lock()
            .expect("lock task statuses")
            .iter()
            .any(|s| matches!(s, TransferStatus::Complete)),
        "task callback should still work after clearing global listeners"
    );

    client.close().await.expect("close client");
    let _ = fs::remove_file(&path);
    server.shutdown();
}
