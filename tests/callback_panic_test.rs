#[path = "dev_server/mod.rs"]
mod dev_server;

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    p.push(format!("rusty_cat_callback_panic_{case}_{ts}.bin"));
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

fn valid_range_get_response() -> String {
    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 4-7/8\r\nContent-Length: 4\r\nConnection: close\r\n\r\nwxyz".to_string()
}

fn head_response() -> String {
    "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\n".to_string()
}

#[tokio::test]
async fn task_callback_panic_does_not_break_scheduler() {
    let server = dev_server::ScriptedServer::spawn_download(vec![
        head_response(),
        valid_range_get_response(),
    ]);
    let path = temp_download_path("task_cb");
    fs::write(&path, b"abcd").expect("write local prefix");

    let client = MeowClient::new(MeowConfig::new(1, 1));
    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_for_global = statuses.clone();
    client
        .register_global_progress_listener(move |record: FileTransferRecord| {
            statuses_for_global
                .lock()
                .expect("lock statuses")
                .push(record.status().clone());
        })
        .expect("register global listener");

    let should_panic_once = Arc::new(AtomicBool::new(true));
    let panic_flag = should_panic_once.clone();
    let task = DownloadPounceBuilder::new(
        "case.bin",
        &path,
        4,
        format!("{}/download", server.base_url()),
        Method::GET,
    )
    .build();
    client
        .enqueue(task, move |_record: FileTransferRecord| {
            if panic_flag.swap(false, Ordering::AcqRel) {
                panic!("task progress callback panic in test");
            }
        }, Some(|_, _| {}))
        .await
        .expect("enqueue task");

    let status = wait_terminal_status(statuses).await;
    client.close().await.expect("close client");
    server.shutdown();
    let bytes = fs::read(&path).expect("read final file");
    fs::remove_file(&path).expect("remove temp file");

    match status {
        TransferStatus::Complete => {}
        other => panic!("expected complete status, got {other:?}"),
    }
    assert_eq!(bytes, b"abcdwxyz");
}

#[tokio::test]
async fn global_callback_panic_does_not_break_scheduler() {
    let server = dev_server::ScriptedServer::spawn_download(vec![
        head_response(),
        valid_range_get_response(),
    ]);
    let path = temp_download_path("global_cb");
    fs::write(&path, b"abcd").expect("write local prefix");

    let client = MeowClient::new(MeowConfig::new(1, 1));
    let should_panic_once = Arc::new(AtomicBool::new(true));
    let panic_flag = should_panic_once.clone();
    client
        .register_global_progress_listener(move |_record: FileTransferRecord| {
            if panic_flag.swap(false, Ordering::AcqRel) {
                panic!("global progress callback panic in test");
            }
        })
        .expect("register global listener");

    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_for_task = statuses.clone();
    let task = DownloadPounceBuilder::new(
        "case.bin",
        &path,
        4,
        format!("{}/download", server.base_url()),
        Method::GET,
    )
    .build();
    client
        .enqueue(task, move |record: FileTransferRecord| {
            statuses_for_task
                .lock()
                .expect("lock statuses")
                .push(record.status().clone());
        }, Some(|_, _| {}))
        .await
        .expect("enqueue task");

    let status = wait_terminal_status(statuses).await;
    client.close().await.expect("close client");
    server.shutdown();
    let bytes = fs::read(&path).expect("read final file");
    fs::remove_file(&path).expect("remove temp file");

    match status {
        TransferStatus::Complete => {}
        other => panic!("expected complete status, got {other:?}"),
    }
    assert_eq!(bytes, b"abcdwxyz");
}

#[tokio::test]
async fn complete_callback_panic_does_not_break_scheduler() {
    let server = dev_server::ScriptedServer::spawn_download(vec![
        head_response(),
        valid_range_get_response(),
    ]);
    let path = temp_download_path("complete_cb");
    fs::write(&path, b"abcd").expect("write local prefix");

    let client = MeowClient::new(MeowConfig::new(1, 1));
    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_for_task = statuses.clone();
    let should_panic_once = Arc::new(AtomicBool::new(true));
    let panic_flag = should_panic_once.clone();

    let task = DownloadPounceBuilder::new(
        "case.bin",
        &path,
        4,
        format!("{}/download", server.base_url()),
        Method::GET,
    )
    .build();
    client
        .enqueue(
            task,
            move |record: FileTransferRecord| {
                statuses_for_task
                    .lock()
                    .expect("lock statuses")
                    .push(record.status().clone());
            },
            Some(move |_, _| {
                if panic_flag.swap(false, Ordering::AcqRel) {
                    panic!("complete callback panic in test");
                }
            }),
        )
        .await
        .expect("enqueue task");

    let status = wait_terminal_status(statuses).await;
    client.close().await.expect("close client");
    server.shutdown();
    let bytes = fs::read(&path).expect("read final file");
    fs::remove_file(&path).expect("remove temp file");

    match status {
        TransferStatus::Complete => {}
        other => panic!("expected complete status, got {other:?}"),
    }
    assert_eq!(bytes, b"abcdwxyz");
}
