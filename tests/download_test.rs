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

fn temp_download_path() -> PathBuf {
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    p.push(format!("rusty_cat_download_e2e_{ts}.bin"));
    p
}

#[tokio::test]
async fn test_download() {
    let payload = b"dev-download-payload-abcdefghijklmnopqrstuvwxyz".repeat(2048);
    let server = dev_server::DevFileServer::spawn(payload.clone());

    let local_path = temp_download_path();
    let client = MeowClient::new(MeowConfig::new(1, 2));
    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_for_cb = statuses.clone();

    let task = DownloadPounceBuilder::new(
        "d.mp4",
        &local_path,
        2048,
        format!("{}/download/d.mp4", server.base_url()),
        Method::GET,
    )
    .build();

    client
        .enqueue(task, move |record: FileTransferRecord| {
            println!("progress: {:?}", record);
            statuses_for_cb
                .lock()
                .expect("lock statuses")
                .push(record.status().clone());
        }, Some(|_, _| {}))
        .await
        .expect("enqueue download task");

    let mut terminal = None;
    for _ in 0..400 {
        let snapshot = statuses.lock().expect("lock statuses").clone();
        terminal = snapshot
            .iter()
            .rev()
            .find(|s| {
                matches!(
                    s,
                    TransferStatus::Complete | TransferStatus::Failed(_) | TransferStatus::Canceled
                )
            })
            .cloned();
        if terminal.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    client.close().await.expect("close client");
    server.shutdown();

    match terminal {
        Some(TransferStatus::Complete) => {}
        Some(other) => panic!("expected complete status, got {other:?}"),
        None => panic!("download test did not reach terminal status in time"),
    }
    let bytes = fs::read(&local_path).expect("read local downloaded file");
    fs::remove_file(&local_path).expect("remove temp download file");
    assert_eq!(bytes, payload);
}
