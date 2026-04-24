#[path = "dev_server/mod.rs"]
mod dev_server;

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusty_cat::down_pounce_builder::DownloadPounceBuilder;
use rusty_cat::file_transfer_record::FileTransferRecord;
use rusty_cat::ids::GlobalProgressListenerId;
use rusty_cat::meow_config::MeowConfig;
use rusty_cat::transfer_status::TransferStatus;
use rusty_cat::MeowClient;

fn temp_download_path(case: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    p.push(format!("rusty_cat_listener_isolation_{case}_{ts}.bin"));
    p
}

async fn run_download_and_wait_complete(
    client: &MeowClient,
    path: &PathBuf,
    url: String,
) -> Vec<TransferStatus> {
    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();
    let task = DownloadPounceBuilder::new("listener.bin", path, 2048, url).build();

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
        .expect("enqueue listener-isolation task");

    for _ in 0..500 {
        let done = statuses
            .lock()
            .expect("lock statuses done")
            .iter()
            .any(|s| {
                matches!(
                    s,
                    TransferStatus::Complete | TransferStatus::Failed(_) | TransferStatus::Canceled
                )
            });
        if done {
            return statuses.lock().expect("lock statuses final").clone();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("task did not finish in time");
}

#[tokio::test]
async fn panic_in_one_global_listener_does_not_block_other_listeners() {
    let payload = b"listener-panic-isolation".repeat(2048);
    let server = dev_server::DevFileServer::spawn(payload);
    let client = MeowClient::new(MeowConfig::new(1, 1));

    let panic_once = Arc::new(AtomicBool::new(true));
    let panic_once_cb = panic_once.clone();
    client
        .register_global_progress_listener(move |_record| {
            if panic_once_cb.swap(false, Ordering::AcqRel) {
                panic!("intentional panic in one global listener");
            }
        })
        .expect("register panic listener");

    let healthy_hits = Arc::new(AtomicUsize::new(0));
    let healthy_hits_cb = healthy_hits.clone();
    client
        .register_global_progress_listener(move |_record| {
            healthy_hits_cb.fetch_add(1, Ordering::Relaxed);
        })
        .expect("register healthy listener");

    let path = temp_download_path("panic_isolation");
    let statuses = run_download_and_wait_complete(
        &client,
        &path,
        format!("{}/download/a.bin", server.base_url()),
    )
    .await;

    assert!(
        statuses
            .iter()
            .any(|s| matches!(s, TransferStatus::Complete)),
        "task should still complete even when one listener panics"
    );
    assert!(
        healthy_hits.load(Ordering::Relaxed) > 0,
        "healthy listener should continue to receive events"
    );

    client.close().await.expect("close client");
    server.shutdown();
    let _ = fs::remove_file(&path);
}

#[tokio::test]
async fn listeners_can_unregister_self_and_each_other_inside_callback() {
    let payload = b"listener-self-and-mutual-unregister".repeat(2048);
    let server = dev_server::DevFileServer::spawn(payload);
    let client = Arc::new(MeowClient::new(MeowConfig::new(1, 1)));

    let a_hits = Arc::new(AtomicUsize::new(0));
    let b_hits = Arc::new(AtomicUsize::new(0));
    let c_hits = Arc::new(AtomicUsize::new(0));

    let a_id_slot: Arc<Mutex<Option<GlobalProgressListenerId>>> = Arc::new(Mutex::new(None));
    let c_id_slot: Arc<Mutex<Option<GlobalProgressListenerId>>> = Arc::new(Mutex::new(None));

    let client_a = client.clone();
    let a_hits_cb = a_hits.clone();
    let a_slot_cb = a_id_slot.clone();
    let a_id = client
        .register_global_progress_listener(move |_record| {
            let prev = a_hits_cb.fetch_add(1, Ordering::SeqCst);
            if prev == 0 {
                let id = *a_slot_cb.lock().expect("lock a slot");
                if let Some(id) = id {
                    let _ = client_a.unregister_global_progress_listener(id);
                }
            }
        })
        .expect("register listener a");
    *a_id_slot.lock().expect("lock a id slot set") = Some(a_id);

    let client_b = client.clone();
    let b_hits_cb = b_hits.clone();
    let c_slot_cb = c_id_slot.clone();
    client
        .register_global_progress_listener(move |_record| {
            let prev = b_hits_cb.fetch_add(1, Ordering::SeqCst);
            if prev == 0 {
                let id = *c_slot_cb.lock().expect("lock c slot");
                if let Some(id) = id {
                    let _ = client_b.unregister_global_progress_listener(id);
                }
            }
        })
        .expect("register listener b");

    let c_hits_cb = c_hits.clone();
    let c_id = client
        .register_global_progress_listener(move |_record| {
            c_hits_cb.fetch_add(1, Ordering::SeqCst);
        })
        .expect("register listener c");
    *c_id_slot.lock().expect("lock c id slot set") = Some(c_id);

    let path1 = temp_download_path("unregister_round1");
    let statuses1 = run_download_and_wait_complete(
        &client,
        &path1,
        format!("{}/download/r1.bin", server.base_url()),
    )
    .await;
    assert!(
        statuses1
            .iter()
            .any(|s| matches!(s, TransferStatus::Complete)),
        "round1 should complete"
    );

    let a_after_r1 = a_hits.load(Ordering::SeqCst);
    let b_after_r1 = b_hits.load(Ordering::SeqCst);
    let c_after_r1 = c_hits.load(Ordering::SeqCst);

    let path2 = temp_download_path("unregister_round2");
    let statuses2 = run_download_and_wait_complete(
        &client,
        &path2,
        format!("{}/download/r2.bin", server.base_url()),
    )
    .await;
    assert!(
        statuses2
            .iter()
            .any(|s| matches!(s, TransferStatus::Complete)),
        "round2 should complete"
    );

    assert_eq!(
        a_hits.load(Ordering::SeqCst),
        a_after_r1,
        "listener A self-unregistered in round1 and should not receive round2 events"
    );
    assert!(
        b_hits.load(Ordering::SeqCst) > b_after_r1,
        "listener B should continue receiving events"
    );
    assert_eq!(
        c_hits.load(Ordering::SeqCst),
        c_after_r1,
        "listener C was unregistered by B and should stop receiving round2 events"
    );

    client.close().await.expect("close client");
    server.shutdown();
    let _ = fs::remove_file(&path1);
    let _ = fs::remove_file(&path2);
}
