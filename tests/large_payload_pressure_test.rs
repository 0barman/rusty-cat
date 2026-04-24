#[path = "dev_server/mod.rs"]
mod dev_server;

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusty_cat::down_pounce_builder::DownloadPounceBuilder;
use rusty_cat::file_transfer_record::FileTransferRecord;
use rusty_cat::meow_config::MeowConfig;
use rusty_cat::transfer_status::TransferStatus;
use rusty_cat::up_pounce_builder::UploadPounceBuilder;
use rusty_cat::MeowClient;

fn temp_path(case: &str, idx: usize) -> PathBuf {
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    p.push(format!("rusty_cat_large_pressure_{case}_{idx}_{ts}.bin"));
    p
}

async fn wait_terminal(
    statuses: Arc<Mutex<Vec<TransferStatus>>>,
    timeout: Duration,
) -> TransferStatus {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Some(s) = statuses
            .lock()
            .expect("lock statuses")
            .iter()
            .rev()
            .find(|s| {
                matches!(
                    s,
                    TransferStatus::Complete | TransferStatus::Failed(_) | TransferStatus::Canceled
                )
            })
        {
            return s.clone();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timeout waiting terminal status");
}

#[tokio::test]
async fn large_file_upload_and_download_complete_under_pressure() {
    let upload_payload = b"large-upload-pressure".repeat(256 * 1024); // ~5MB
    let download_payload = b"large-download-pressure".repeat(256 * 1024); // ~5MB

    let upload_path = temp_path("upload_src", 0);
    fs::write(&upload_path, &upload_payload).expect("write upload source");
    let download_path = temp_path("download_dst", 0);

    let upload_server = dev_server::DevFileServer::spawn(Vec::new());
    let download_server = dev_server::DevFileServer::spawn(download_payload.clone());

    let client = MeowClient::new(MeowConfig::new(2, 2));

    let up_statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let up_statuses_cb = up_statuses.clone();
    let upload_task = UploadPounceBuilder::new("large-up.bin", &upload_path, 512 * 1024)
        .with_url(format!("{}/upload/large-up.bin", upload_server.base_url()))
        .build()
        .expect("build large upload task");
    client
        .try_enqueue(
            upload_task,
            move |record: FileTransferRecord| {
                up_statuses_cb
                    .lock()
                    .expect("lock upload statuses")
                    .push(record.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("enqueue large upload");

    let down_statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let down_statuses_cb = down_statuses.clone();
    let download_task = DownloadPounceBuilder::new(
        "large-down.bin",
        &download_path,
        512 * 1024,
        format!("{}/download/large-down.bin", download_server.base_url()),
    )
    .build();
    client
        .try_enqueue(
            download_task,
            move |record: FileTransferRecord| {
                down_statuses_cb
                    .lock()
                    .expect("lock download statuses")
                    .push(record.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("enqueue large download");

    let up_terminal = wait_terminal(up_statuses, Duration::from_secs(30)).await;
    let down_terminal = wait_terminal(down_statuses, Duration::from_secs(30)).await;

    assert!(matches!(up_terminal, TransferStatus::Complete));
    assert!(matches!(down_terminal, TransferStatus::Complete));

    client.close().await.expect("close client");

    let upload_inspect = upload_server.upload_inspector();
    upload_server.shutdown();
    download_server.shutdown();

    assert_eq!(upload_inspect.max_next_byte, upload_payload.len() as u64);
    assert!(upload_inspect.completed);

    let downloaded = fs::read(&download_path).expect("read large downloaded file");
    assert_eq!(downloaded, download_payload);

    let _ = fs::remove_file(&upload_path);
    let _ = fs::remove_file(&download_path);
}

#[tokio::test]
async fn high_concurrency_large_chunks_and_slow_callbacks_still_converge() {
    let payload = b"high-concurrency-large-chunk".repeat(64 * 1024); // ~1.6MB
    let server = dev_server::DevFileServer::spawn(payload.clone());
    let client = MeowClient::new(MeowConfig::new(4, 4));

    let total_tasks = 8usize;
    let mut statuses_list = Vec::with_capacity(total_tasks);
    let mut paths = Vec::with_capacity(total_tasks);

    for i in 0..total_tasks {
        let p = temp_path("concurrent_download", i);
        paths.push(p.clone());
        let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
        let statuses_cb = statuses.clone();
        statuses_list.push(statuses);

        let task = DownloadPounceBuilder::new(
            format!("d{i}.bin"),
            &p,
            512 * 1024,
            format!("{}/download/d{i}.bin", server.base_url()),
        )
        .build();

        client
            .try_enqueue(
                task,
                move |record: FileTransferRecord| {
                    // 模拟慢消费者，覆盖回调阻塞下的大 chunk 场景。
                    std::thread::sleep(Duration::from_millis(5));
                    statuses_cb
                        .lock()
                        .expect("lock statuses")
                        .push(record.status().clone());
                },
                |_, _| {},
            )
            .await
            .expect("enqueue concurrent large chunk task");
    }

    for statuses in statuses_list {
        let terminal = wait_terminal(statuses, Duration::from_secs(30)).await;
        assert!(
            matches!(terminal, TransferStatus::Complete),
            "all tasks should complete under pressure"
        );
    }

    client.close().await.expect("close client");
    server.shutdown();

    for p in paths {
        let bytes = fs::read(&p).expect("read concurrent downloaded file");
        assert_eq!(bytes, payload);
        let _ = fs::remove_file(&p);
    }
}
