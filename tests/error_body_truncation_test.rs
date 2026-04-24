#[path = "dev_server/mod.rs"]
mod dev_server;

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    p.push(format!("rusty_cat_error_body_trunc_{case}_{ts}.bin"));
    p
}

fn head_response(total: usize) -> String {
    format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n")
}

fn invalid_get_with_huge_body(body: &str) -> String {
    format!(
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

#[tokio::test]
async fn download_error_body_preview_is_truncated_to_max_bytes() {
    let huge_body = "x".repeat(10_000);
    let server = dev_server::ScriptedServer::spawn_download(vec![
        head_response(16),
        invalid_get_with_huge_body(&huge_body),
    ]);

    let path = temp_download_path("preview");
    let client = MeowClient::new(MeowConfig::new(1, 1));

    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();

    let task = DownloadPounceBuilder::new(
        "preview.bin",
        &path,
        4,
        format!("{}/download", server.base_url()),
    )
    .build();

    client
        .try_enqueue(
            task,
            move |record: FileTransferRecord| {
                statuses_cb
                    .lock()
                    .expect("lock statuses")
                    .push(record.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("enqueue truncation task");

    let mut terminal = None;
    for _ in 0..300 {
        terminal = statuses
            .lock()
            .expect("lock statuses poll")
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
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let err = match terminal {
        Some(TransferStatus::Failed(e)) => e,
        other => panic!("expected failed terminal status, got {other:?}"),
    };

    assert_eq!(err.code(), InnerErrorCode::InvalidRange as i32);
    assert!(
        err.msg().contains("206 Partial Content"),
        "error should mention ranged GET status contract"
    );

    let x4096 = "x".repeat(4096);
    let x4097 = "x".repeat(4097);
    assert!(
        err.msg().contains(&x4096),
        "error message should include body preview up to 4096 bytes"
    );
    assert!(
        !err.msg().contains(&x4097),
        "error message should not include more than 4096 preview bytes"
    );

    client.close().await.expect("close client");
    server.shutdown();
    let _ = fs::remove_file(&path);
}
