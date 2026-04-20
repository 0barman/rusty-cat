#[path = "dev_server/mod.rs"]
mod dev_server;

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    p.push(format!("rusty_cat_chunk_retry_{case}_{ts}.bin"));
    p
}

async fn wait_terminal_status(statuses: Arc<Mutex<Vec<TransferStatus>>>) -> TransferStatus {
    for _ in 0..150 {
        if let Some(last) = statuses.lock().expect("lock statuses").last().cloned() {
            if matches!(
                last,
                TransferStatus::Complete | TransferStatus::Failed(_) | TransferStatus::Canceled
            ) {
                return last;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("did not receive terminal status in time");
}

#[tokio::test]
async fn default_retry_3_recovers_after_two_chunk_failures() {
    let server = dev_server::FlakyDownloadServer::spawn_one_chunk(2, b"wxyz".to_vec());
    let path = temp_download_path("default_retry");
    let task = DownloadPounceBuilder::new(
        "retry.bin",
        &path,
        4,
        format!("{}/download", server.base_url()),
        Method::GET,
    )
    .build();
    let client = MeowClient::new(MeowConfig::new(1, 1));

    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();
    client
        .enqueue(
            task,
            move |record: FileTransferRecord| {
                statuses_cb
                    .lock()
                    .expect("lock statuses callback")
                    .push(record.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("enqueue task");

    let terminal = wait_terminal_status(statuses).await;
    client.close().await.expect("close client");
    server.shutdown();
    let bytes = fs::read(&path).expect("read downloaded file");
    fs::remove_file(&path).expect("remove temp file");

    assert!(matches!(terminal, TransferStatus::Complete));
    assert_eq!(bytes, b"wxyz");
}

#[tokio::test]
async fn configured_retry_0_fails_on_first_chunk_failure() {
    let server = dev_server::FlakyDownloadServer::spawn_one_chunk(1, b"wxyz".to_vec());
    let path = temp_download_path("retry_zero");
    let task = DownloadPounceBuilder::new(
        "retry.bin",
        &path,
        4,
        format!("{}/download", server.base_url()),
        Method::GET,
    )
    .with_max_chunk_retries(0)
    .build();
    let client = MeowClient::new(MeowConfig::new(1, 1));

    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();
    client
        .enqueue(
            task,
            move |record: FileTransferRecord| {
                statuses_cb
                    .lock()
                    .expect("lock statuses callback")
                    .push(record.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("enqueue task");

    let terminal = wait_terminal_status(statuses).await;
    client.close().await.expect("close client");
    server.shutdown();
    let _ = fs::remove_file(&path);

    match terminal {
        TransferStatus::Failed(err) => assert_eq!(err.code(), InnerErrorCode::HttpError as i32),
        other => panic!("expected failed terminal status, got {other:?}"),
    }
}
