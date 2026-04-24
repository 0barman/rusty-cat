#[path = "dev_server/mod.rs"]
mod dev_server;

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusty_cat::direction::Direction;
use rusty_cat::down_pounce_builder::DownloadPounceBuilder;
use rusty_cat::file_transfer_record::FileTransferRecord;
use rusty_cat::meow_config::MeowConfig;
use rusty_cat::transfer_status::TransferStatus;
use rusty_cat::up_pounce_builder::UploadPounceBuilder;
use rusty_cat::MeowClient;

fn temp_path(case: &str, idx: usize, ext: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    p.push(format!("rusty_cat_concurrency_{case}_{idx}_{ts}.{ext}"));
    p
}

#[derive(Default)]
struct ObserveState {
    // key 使用 task_id 的 Debug 文本，避免对 TaskId 的 Hash 约束依赖。
    latest: HashMap<String, (Direction, TransferStatus)>,
    max_upload_transmission: usize,
    max_download_transmission: usize,
}

#[tokio::test]
async fn meow_config_concurrency_limits_and_queue_behavior_work() {
    // 场景目标：
    // 1) 配置 max_upload_concurrency / max_download_concurrency；
    // 2) 同时入队“很多”上传与下载任务；
    // 3) 验证并发峰值能分别达到两个上限；
    // 4) 当达到上限后，后续任务会进入等待队列，并且队列任务最终继续正常完成。
    let max_upload = 2usize;
    let max_download = 3usize;
    let upload_task_count = max_upload + 3;
    let download_task_count = max_download + 4;
    let total_tasks = upload_task_count + download_task_count;

    // 用较大载荷 + 小 chunk，拉长传输窗口，提升并发与排队观察稳定性。
    let download_payload = b"meow-config-concurrency-download".repeat(32 * 1024);
    let server = dev_server::DevFileServer::spawn(download_payload);
    let client = MeowClient::new(MeowConfig::new(max_upload, max_download));

    let observe = Arc::new(Mutex::new(ObserveState::default()));
    let observe_cb = observe.clone();
    let _listener_id = client
        .register_global_progress_listener(move |record: FileTransferRecord| {
            let mut g = observe_cb.lock().expect("lock observe");
            let key = format!("{:?}", record.task_id());
            g.latest
                .insert(key, (record.direction(), record.status().clone()));

            let mut up_now = 0usize;
            let mut down_now = 0usize;
            for (dir, status) in g.latest.values() {
                if matches!(status, TransferStatus::Transmission) {
                    match dir {
                        Direction::Upload => up_now += 1,
                        Direction::Download => down_now += 1,
                    }
                }
            }
            g.max_upload_transmission = g.max_upload_transmission.max(up_now);
            g.max_download_transmission = g.max_download_transmission.max(down_now);
        })
        .expect("register global observer");

    let mut upload_paths = Vec::new();
    let mut download_paths = Vec::new();

    for i in 0..upload_task_count {
        let path = temp_path("upload_src", i, "bin");
        let mut payload = b"meow-config-concurrency-upload".repeat(32 * 1024);
        payload[0] = (i % 251) as u8;
        fs::write(&path, payload).expect("write upload source");
        upload_paths.push(path.clone());

        let task = UploadPounceBuilder::new(format!("up_{i}.bin"), &path, 1024)
            .with_url(format!("{}/upload/up_{i}.bin", server.base_url()))
            .build()
            .expect("build upload task");
        client
            .try_enqueue(task, |_record: FileTransferRecord| {}, |_, _| {})
            .await
            .expect("enqueue upload task");
    }

    for i in 0..download_task_count {
        let path = temp_path("download_dst", i, "bin");
        download_paths.push(path.clone());
        let task = DownloadPounceBuilder::new(
            format!("down_{i}.bin"),
            &path,
            1024,
            format!("{}/download/down_{i}.bin", server.base_url()),
        )
        .build();
        client
            .try_enqueue(task, |_record: FileTransferRecord| {}, |_, _| {})
            .await
            .expect("enqueue download task");
    }

    let mut seen_queue_non_empty = false;
    let mut seen_queue_drain_after_non_empty = false;
    let start = Instant::now();
    loop {
        let snap = client
            .snapshot()
            .await
            .expect("snapshot during concurrency run");
        if snap.queued_groups > 0 {
            seen_queue_non_empty = true;
        } else if seen_queue_non_empty {
            seen_queue_drain_after_non_empty = true;
        }

        let (max_up, max_down, terminal_count, failed_count, canceled_count) = {
            let g = observe.lock().expect("lock observe in loop");
            let mut terminal = 0usize;
            let mut failed = 0usize;
            let mut canceled = 0usize;
            for (_, status) in g.latest.values() {
                match status {
                    TransferStatus::Complete => terminal += 1,
                    TransferStatus::Failed(_) => {
                        terminal += 1;
                        failed += 1;
                    }
                    TransferStatus::Canceled => {
                        terminal += 1;
                        canceled += 1;
                    }
                    _ => {}
                }
            }
            (
                g.max_upload_transmission,
                g.max_download_transmission,
                terminal,
                failed,
                canceled,
            )
        };

        if terminal_count == total_tasks {
            assert_eq!(failed_count, 0, "all tasks should finish without Failed");
            assert_eq!(
                canceled_count, 0,
                "all tasks should finish without Canceled"
            );
            assert_eq!(
                max_up, max_upload,
                "upload concurrent peak should reach configured max_upload_concurrency"
            );
            assert_eq!(
                max_down, max_download,
                "download concurrent peak should reach configured max_download_concurrency"
            );
            assert!(
                seen_queue_non_empty,
                "when tasks exceed concurrency limits, queue should be non-empty at some point"
            );
            assert!(
                seen_queue_drain_after_non_empty,
                "queued tasks should later be drained and continue to completion"
            );
            break;
        }

        if start.elapsed() > Duration::from_secs(45) {
            panic!("concurrency+queue validation timed out");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    client.close().await.expect("close client");
    for p in upload_paths.into_iter().chain(download_paths.into_iter()) {
        let _ = fs::remove_file(&p);
    }
    server.shutdown();
}
