#[path = "dev_server/mod.rs"]
mod dev_server;

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusty_cat::error::InnerErrorCode;
use rusty_cat::file_transfer_record::FileTransferRecord;
use rusty_cat::meow_config::MeowConfig;
use rusty_cat::transfer_status::TransferStatus;
use rusty_cat::up_pounce_builder::UploadPounceBuilder;
use rusty_cat::MeowClient;

fn temp_upload_path(case: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    p.push(format!("rusty_cat_upload_sign_dup_{case}_{ts}.bin"));
    p
}

#[tokio::test]
async fn upload_record_file_sign_matches_source_md5() {
    // 场景说明：
    // 1) 上传一个固定内容文件；
    // 2) 在回调中采集 file_sign；
    // 3) 期望回调中的 file_sign 与源文件 MD5 一致，覆盖 sign 计算链路。
    let payload = b"upload-sign-md5-check".repeat(4096);
    let expected_sign = format!("{:x}", md5::compute(&payload));
    let upload_path = temp_upload_path("md5_match");
    fs::write(&upload_path, &payload).expect("write upload source fixture");

    let server = dev_server::DevFileServer::spawn(Vec::new());
    let client = MeowClient::new(MeowConfig::new(1, 1));

    let terminal: Arc<Mutex<Option<TransferStatus>>> = Arc::new(Mutex::new(None));
    let observed_signs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let terminal_cb = terminal.clone();
    let signs_cb = observed_signs.clone();

    let task = UploadPounceBuilder::new("md5.bin", &upload_path, 2048)
        .with_url(format!("{}/upload/md5.bin", server.base_url()))
        .build()
        .expect("build upload task");
    client
        .enqueue(task, move |record: FileTransferRecord| {
            signs_cb
                .lock()
                .expect("lock signs")
                .push(record.file_sign().to_string());
            if matches!(
                record.status(),
                TransferStatus::Complete | TransferStatus::Failed(_) | TransferStatus::Canceled
            ) {
                *terminal_cb.lock().expect("lock terminal") = Some(record.status().clone());
            }
        })
        .await
        .expect("enqueue upload task");

    for _ in 0..400 {
        if terminal.lock().expect("lock terminal").is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let final_status = terminal
        .lock()
        .expect("lock terminal final")
        .clone()
        .expect("upload should reach terminal status");
    match final_status {
        TransferStatus::Complete => {}
        other => panic!("expected complete status, got {other:?}"),
    }

    let signs = observed_signs.lock().expect("lock signs final").clone();
    assert!(
        !signs.is_empty(),
        "should capture at least one callback sign"
    );
    assert!(
        signs.iter().all(|s| s == &expected_sign),
        "all callback signs should equal source MD5"
    );

    client.close().await.expect("close client");
    let _ = fs::remove_file(&upload_path);
    server.shutdown();
}

#[tokio::test]
async fn second_upload_with_same_sign_hits_duplicate_branch() {
    // 场景说明：
    // 1) 连续 enqueue 两个上传任务，使用同一个源文件（file_sign 相同）；
    // 2) URL 与 file_name 故意不同，验证 dedupe 键确实基于 sign；
    // 3) 第二任务应在回调里收到 Failed(DuplicateTaskError)。
    let payload = b"upload-duplicate-sign".repeat(256 * 1024);
    let upload_path = temp_upload_path("duplicate_sign");
    fs::write(&upload_path, &payload).expect("write upload source fixture");

    let server = dev_server::DevFileServer::spawn(Vec::new());
    let client = MeowClient::new(MeowConfig::new(1, 1));

    let statuses2: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let s2 = statuses2.clone();

    let task1 = UploadPounceBuilder::new("dup_a.bin", &upload_path, 1024)
        .with_url(format!("{}/upload/dup_a.bin", server.base_url()))
        .build()
        .expect("build first upload task");
    let task2 = UploadPounceBuilder::new("dup_b.bin", &upload_path, 1024)
        .with_url(format!("{}/upload/dup_b.bin", server.base_url()))
        .build()
        .expect("build second upload task");

    client
        .enqueue(task1, |_record: FileTransferRecord| {})
        .await
        .expect("enqueue first upload");
    client
        .enqueue(task2, move |record: FileTransferRecord| {
            s2.lock()
                .expect("lock statuses2")
                .push(record.status().clone());
        })
        .await
        .expect("enqueue second upload should still return task id");

    let mut seen_duplicate = false;
    for _ in 0..250 {
        seen_duplicate = statuses2.lock().expect("lock statuses2").iter().any(|s| {
            matches!(
                s,
                TransferStatus::Failed(err) if err.code() == InnerErrorCode::DuplicateTaskError as i32
            )
        });
        if seen_duplicate {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        seen_duplicate,
        "second upload with same sign should hit DuplicateTaskError callback path"
    );

    client.close().await.expect("close client");
    let _ = fs::remove_file(&upload_path);
    server.shutdown();
}
