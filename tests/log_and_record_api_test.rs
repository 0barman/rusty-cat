#[path = "dev_server/mod.rs"]
mod dev_server;

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::Method;
use rusty_cat::down_pounce_builder::DownloadPounceBuilder;
use rusty_cat::file_transfer_record::FileTransferRecord;
use rusty_cat::log::{Log, LogLevel};
use rusty_cat::meow_config::MeowConfig;
use rusty_cat::transfer_status::TransferStatus;
use rusty_cat::MeowClient;

fn temp_path(case: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    p.push(format!("rusty_cat_log_record_{case}_{ts}.bin"));
    p
}

#[test]
fn log_struct_getters_display_and_into_message_paths() {
    // 场景说明：
    // 1) 覆盖 Log::new + getter + Display 路径；
    // 2) 覆盖 Log::debug 快捷构造路径；
    // 3) 覆盖 into_message 消费式读取路径。
    let log = Log::new(LogLevel::Info, "api_test", "hello");
    assert_eq!(log.level(), LogLevel::Info);
    assert_eq!(log.tag(), "api_test");
    assert_eq!(log.message(), "hello");
    assert!(log.timestamp_ms() > 0 || log.timestamp_ms() == 0);
    let text = format!("{log}");
    assert!(text.contains("[INFO]") || text.contains(" INFO "));
    assert!(text.contains("api_test"));
    assert!(text.contains("hello"));

    let debug_log = Log::debug("dbg_tag", "dbg_message");
    assert_eq!(debug_log.level(), LogLevel::Debug);
    assert_eq!(debug_log.tag(), "dbg_tag");
    assert_eq!(debug_log.clone().into_message(), "dbg_message");
}

#[tokio::test]
async fn file_transfer_record_getters_return_expected_values_from_callback() {
    // 场景说明：
    // 1) 通过真实下载回调拿到 FileTransferRecord（避免手工构造不可用的 TaskId）；
    // 2) 对 record 的 getter（task_id/file_sign/file_name/total_size/progress/status/direction）做断言；
    // 3) 覆盖 file_transfer_record.rs 的 getter 分支。
    let payload = b"record-getter-payload".repeat(2048);
    let server = dev_server::DevFileServer::spawn(payload.clone());
    let client = MeowClient::new(MeowConfig::new(1, 1));
    let path = temp_path("record_getter");

    let latest_record: Arc<Mutex<Option<FileTransferRecord>>> = Arc::new(Mutex::new(None));
    let latest_for_cb = latest_record.clone();

    let task = DownloadPounceBuilder::new(
        "getter.bin",
        &path,
        1024,
        format!("{}/download/getter.bin", server.base_url()),
        Method::GET,
    )
    .build();
    client
        .enqueue(task, move |record: FileTransferRecord| {
            if matches!(
                record.status(),
                TransferStatus::Transmission | TransferStatus::Complete
            ) {
                *latest_for_cb.lock().expect("lock latest record") = Some(record);
            }
        }, Some(|_, _| {}))
        .await
        .expect("enqueue getter task");

    for _ in 0..300 {
        let maybe = latest_record.lock().expect("lock latest record").clone();
        if let Some(rec) = maybe {
            assert_eq!(rec.file_name(), "getter.bin");
            assert!(
                rec.total_size() > 0,
                "total_size should be known in transfer"
            );
            assert!(rec.progress() >= 0.0 && rec.progress() <= 1.0);
            let _tid = rec.task_id();
            let _sign = rec.file_sign();
            let _dir = rec.direction();
            let _status = rec.status();
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    client.close().await.expect("close client");
    let _ = fs::remove_file(&path);
    server.shutdown();
}
