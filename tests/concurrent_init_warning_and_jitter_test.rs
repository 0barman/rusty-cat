#[path = "dev_server/mod.rs"]
mod dev_server;

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusty_cat::api::{Log, LogLevel, MeowClient, MeowConfig};
use rusty_cat::down_pounce_builder::DownloadPounceBuilder;
use rusty_cat::file_transfer_record::FileTransferRecord;
use rusty_cat::transfer_status::TransferStatus;
use rusty_cat::{set_debug_log_listener, DebugLogListener};

fn temp_download_path(case: &str, idx: usize) -> PathBuf {
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    p.push(format!("rusty_cat_init_warn_jitter_{case}_{idx}_{ts}.bin"));
    p
}

fn serial_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .expect("serial guard lock")
}

#[derive(Default)]
struct InitLogCollector {
    executor_drop_warn: AtomicUsize,
    executor_started: AtomicUsize,
}

impl InitLogCollector {
    fn install(self: &Arc<Self>) {
        let me = Arc::clone(self);
        let listener: DebugLogListener = Arc::new(move |log: Log| {
            if log.tag() == "executor_drop" && matches!(log.level(), LogLevel::Warn) {
                me.executor_drop_warn.fetch_add(1, Ordering::SeqCst);
            }
            if log.tag() == "executor" && log.message().contains("executor worker started") {
                me.executor_started.fetch_add(1, Ordering::SeqCst);
            }
        });
        set_debug_log_listener(Some(listener)).expect("set debug log listener");
    }

    fn uninstall() {
        let _ = set_debug_log_listener(None);
    }
}

#[tokio::test]
async fn concurrent_first_init_does_not_emit_executor_drop_warn_or_multi_start_jitter() {
    let _guard = serial_guard();
    let collector = Arc::new(InitLogCollector::default());
    collector.install();

    let payload = b"concurrent-init-log-check".repeat(4096);
    let server = dev_server::DevFileServer::spawn(payload.clone());
    let client = Arc::new(MeowClient::new(MeowConfig::new(4, 4)));

    let task_count = 8usize;
    let mut handles = Vec::with_capacity(task_count);
    let mut paths = Vec::with_capacity(task_count);

    for i in 0..task_count {
        let client = client.clone();
        let p = temp_download_path("race", i);
        paths.push(p.clone());
        let url = format!("{}/download/{i}.bin", server.base_url());
        handles.push(tokio::spawn(async move {
            let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
            let statuses_cb = statuses.clone();
            let task = DownloadPounceBuilder::new(format!("{i}.bin"), &p, 4096, url).build();
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
                .expect("enqueue on concurrent first use");

            for _ in 0..500 {
                let done = statuses
                    .lock()
                    .expect("lock statuses done")
                    .iter()
                    .any(|s| {
                        matches!(
                            s,
                            TransferStatus::Complete
                                | TransferStatus::Failed(_)
                                | TransferStatus::Canceled
                        )
                    });
                if done {
                    return statuses
                        .lock()
                        .expect("lock statuses final")
                        .iter()
                        .rev()
                        .find(|s| {
                            matches!(
                                s,
                                TransferStatus::Complete
                                    | TransferStatus::Failed(_)
                                    | TransferStatus::Canceled
                            )
                        })
                        .cloned();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            None
        }));
    }

    for h in handles {
        let terminal = h.await.expect("join concurrent handle");
        assert!(
            matches!(terminal, Some(TransferStatus::Complete)),
            "each task should complete"
        );
    }

    client.close().await.expect("explicit close");
    drop(client);

    assert_eq!(
        collector.executor_drop_warn.load(Ordering::SeqCst),
        0,
        "explicit close after concurrent init must not emit executor_drop warn"
    );
    assert_eq!(
        collector.executor_started.load(Ordering::SeqCst),
        1,
        "concurrent first use should initialize only one executor worker"
    );

    for p in paths {
        let _ = fs::remove_file(&p);
    }
    server.shutdown();
    InitLogCollector::uninstall();
}
