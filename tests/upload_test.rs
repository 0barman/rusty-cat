#[path = "dev_server/mod.rs"]
mod dev_server;

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusty_cat::file_transfer_record::FileTransferRecord;
use rusty_cat::meow_config::MeowConfig;
use rusty_cat::transfer_status::TransferStatus;
use rusty_cat::up_pounce_builder::UploadPounceBuilder;
use rusty_cat::MeowClient;

fn temp_upload_path() -> PathBuf {
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    p.push(format!("rusty_cat_upload_e2e_src_{ts}.bin"));
    p
}

#[tokio::test]
async fn test_upload() {
    let upload_payload = b"dev-upload-payload-abcdefghijklmnopqrstuvwxyz".repeat(2048);
    let upload_path = temp_upload_path();
    fs::write(&upload_path, &upload_payload).expect("write upload source fixture");

    let server = dev_server::DevFileServer::spawn(Vec::new());
    let client = MeowClient::new(MeowConfig::new(1, 2));
    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_for_cb = statuses.clone();

    let task = UploadPounceBuilder::new("u.bin", &upload_path, 2048)
        .with_url(format!("{}/upload/u.bin", server.base_url()))
        .build()
        .expect("build upload task");
    client
        .enqueue(task, move |record: FileTransferRecord| {
            println!("upload progress: {:?}", record);
            statuses_for_cb
                .lock()
                .expect("lock statuses")
                .push(record.status().clone());
        }, Some(|_, _| {}))
        .await
        .expect("enqueue upload task");

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
    let inspect = server.upload_inspector();
    server.shutdown();
    fs::remove_file(&upload_path).expect("remove temp upload source file");

    match terminal {
        Some(TransferStatus::Complete) => {}
        Some(other) => panic!("expected complete status, got {other:?}"),
        None => panic!("upload test did not reach terminal status in time"),
    }
    assert!(
        inspect.prepare_calls >= 1,
        "expected at least one upload prepare request, got {}",
        inspect.prepare_calls
    );
    assert!(
        inspect.chunk_calls >= 1,
        "expected at least one upload chunk request, got {}",
        inspect.chunk_calls
    );
    assert_eq!(
        inspect.max_next_byte,
        upload_payload.len() as u64,
        "server observed uploaded bytes do not match source length"
    );
    assert!(
        inspect.completed,
        "server never marked upload completed according to totalSize"
    );
}
