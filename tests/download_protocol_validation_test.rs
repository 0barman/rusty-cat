#[path = "dev_server/mod.rs"]
mod dev_server;

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    p.push(format!("rusty_cat_download_protocol_{case}_{ts}.bin"));
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

async fn run_download_case(
    case_name: &str,
    get_response: String,
) -> (
    TransferStatus,
    Vec<u8>,
    dev_server::ScriptedServer,
    PathBuf,
    MeowClient,
) {
    let head_response =
        "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\n".to_string();
    let server = dev_server::ScriptedServer::spawn_download(vec![head_response, get_response]);
    let path = temp_download_path(case_name);
    fs::write(&path, b"abcd").expect("write initial local chunk");

    let task = DownloadPounceBuilder::new(
        "case.bin",
        &path,
        4,
        format!("{}/download", server.base_url()),
    )
    .build();
    let client = MeowClient::new(MeowConfig::new(1, 1));
    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();
    client
        .try_enqueue(
            task,
            move |record: FileTransferRecord| {
                statuses_cb
                    .lock()
                    .expect("lock statuses in callback")
                    .push(record.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("enqueue download task");

    let status = wait_terminal_status(statuses).await;
    let bytes = fs::read(&path).expect("read local file after transfer");
    (status, bytes, server, path, client)
}

#[tokio::test]
async fn download_rejects_status_200_for_range_request() {
    let get_response =
        "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nabcdefgh".to_string();
    let (status, bytes, server, path, client) = run_download_case("status200", get_response).await;
    client.close().await.expect("close client");
    server.shutdown();
    fs::remove_file(&path).expect("remove temp file");

    match status {
        TransferStatus::Failed(err) => assert!(err.msg().contains("requires 206 Partial Content")),
        other => panic!("expected failed status, got {other:?}"),
    }
    assert_eq!(bytes, b"abcd");
}

#[tokio::test]
async fn download_rejects_206_without_content_range() {
    let get_response =
        "HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nConnection: close\r\n\r\nwxyz"
            .to_string();
    let (status, bytes, server, path, client) =
        run_download_case("missing_content_range", get_response).await;
    client.close().await.expect("close client");
    server.shutdown();
    fs::remove_file(&path).expect("remove temp file");

    match status {
        TransferStatus::Failed(err) => assert!(err.msg().contains("missing content-range")),
        other => panic!("expected failed status, got {other:?}"),
    }
    assert_eq!(bytes, b"abcd");
}

#[tokio::test]
async fn download_rejects_content_range_start_mismatch() {
    let get_response = "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-3/8\r\nContent-Length: 4\r\nConnection: close\r\n\r\nwxyz".to_string();
    let (status, bytes, server, path, client) =
        run_download_case("start_mismatch", get_response).await;
    client.close().await.expect("close client");
    server.shutdown();
    fs::remove_file(&path).expect("remove temp file");

    match status {
        TransferStatus::Failed(err) => assert!(err.msg().contains("start mismatch")),
        other => panic!("expected failed status, got {other:?}"),
    }
    assert_eq!(bytes, b"abcd");
}

#[tokio::test]
async fn download_accepts_valid_206_and_appends_at_exact_offset() {
    let get_response = "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 4-7/8\r\nContent-Length: 4\r\nConnection: close\r\n\r\nwxyz".to_string();
    let (status, bytes, server, path, client) = run_download_case("valid_206", get_response).await;
    client.close().await.expect("close client");
    server.shutdown();
    fs::remove_file(&path).expect("remove temp file");

    match status {
        TransferStatus::Complete => {}
        other => panic!("expected complete status, got {other:?}"),
    }
    assert_eq!(bytes, b"abcdwxyz");
}
