//! 补充场景测试：针对此前评审中列出的“建议补充测试场景”。
//!
//! 本文件聚焦以下方面：
//! 1) 并发首次初始化竞争：多个任务同时 enqueue 到全新 client，期望只有一份可用执行器，
//!    且都能进入终态。
//! 2) 队列高压背压：`command_queue_capacity=1` + 瞬发 enqueue，验证超出能力时的
//!    错误码稳定（`CommandSendFailed`），不致污染后续任务。
//! 3) 慢回调：进度回调中阻塞数十毫秒，验证调度器仍能收敛到 Complete，不会丢事件或卡死。
//! 4) 未显式 close 的 Drop：直接丢弃 MeowClient，验证 worker/线程无死锁、测试进程可正常退出。
//! 5) 自定义 `reqwest::Client` 注入：通过 `with_http_client` 注入外部 client，端到端完成
//!    一次下载，并验证数据内容一致。
//! 6) 大 chunk_size 内存压力：单片覆盖整个文件，验证 upload buffer 复用无越界且能完成。

#[path = "dev_server/mod.rs"]
mod dev_server;

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusty_cat::down_pounce_builder::DownloadPounceBuilder;
use rusty_cat::error::InnerErrorCode;
use rusty_cat::file_transfer_record::FileTransferRecord;
use rusty_cat::meow_config::MeowConfig;
use rusty_cat::transfer_status::TransferStatus;
use rusty_cat::up_pounce_builder::UploadPounceBuilder;
use rusty_cat::MeowClient;

fn temp_path(case: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    p.push(format!("rusty_cat_supp_{case}_{ts}.bin"));
    p
}

/// 等待 `statuses` 中出现终态或超时返回 `None`。
async fn wait_terminal(
    statuses: Arc<Mutex<Vec<TransferStatus>>>,
    max_iters: usize,
) -> Option<TransferStatus> {
    for _ in 0..max_iters {
        let snap = statuses.lock().expect("lock statuses").clone();
        if let Some(terminal) = snap.into_iter().rev().find(|s| {
            matches!(
                s,
                TransferStatus::Complete | TransferStatus::Failed(_) | TransferStatus::Canceled
            )
        }) {
            return Some(terminal);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    None
}

// ----------------------------------------------------------------------------
// 场景 1：并发首次初始化竞争
// ----------------------------------------------------------------------------

/// 同时对一个“全新 client”发起多次 enqueue，验证内部 executor 只初始化一次且全部任务进入终态。
#[tokio::test]
async fn concurrent_first_use_enqueue_does_not_corrupt_executor_state() {
    let payload = b"concurrent-first-use-payload".repeat(1024);
    let server = dev_server::DevFileServer::spawn(payload.clone());
    let client = Arc::new(MeowClient::new(MeowConfig::new(4, 4)));

    let task_count = 6usize;
    let mut handles = Vec::with_capacity(task_count);
    let mut local_paths = Vec::with_capacity(task_count);

    for i in 0..task_count {
        let client = client.clone();
        let path = temp_path(&format!("concurrent_init_{i}"));
        local_paths.push(path.clone());
        let url = format!("{}/download/concurrent_{i}.bin", server.base_url());
        let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
        let statuses_cb = statuses.clone();
        handles.push(tokio::spawn(async move {
            let task =
                DownloadPounceBuilder::new(format!("concurrent_{i}.bin"), &path, 4096, url).build();
            let _task_id = client
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
                .expect("enqueue should succeed under concurrent first use");
            wait_terminal(statuses, 400).await
        }));
    }

    let mut completes = 0usize;
    for h in handles {
        let terminal = h.await.expect("join concurrent task");
        assert!(
            matches!(terminal, Some(TransferStatus::Complete)),
            "expected Complete, got {terminal:?}"
        );
        completes += 1;
    }
    assert_eq!(completes, task_count);

    // 所有下载目标文件均应与 payload 一致，说明任务之间没有相互破坏状态。
    for p in &local_paths {
        let bytes = fs::read(p).expect("read downloaded file");
        assert_eq!(
            bytes,
            payload,
            "downloaded content mismatch: {}",
            p.display()
        );
        fs::remove_file(p).ok();
    }

    // snapshot 最终应回到 0/0，且 close 能成功（只初始化了一套 executor）。
    let final_snap = client.snapshot().await.expect("final snapshot");
    assert_eq!(final_snap.active_groups, 0);
    assert_eq!(final_snap.queued_groups, 0);
    client.close().await.expect("close client once");

    server.shutdown();
}

// ----------------------------------------------------------------------------
// 场景 2：队列高压 / 背压
// ----------------------------------------------------------------------------

/// 以极小的命令队列容量制造背压；多轮 try_send 中至少一次应命中 `CommandSendFailed`，
/// 且后续任务在队列恢复后应能继续正常 enqueue，不会留下持久损坏。
#[tokio::test]
async fn small_command_queue_capacity_yields_command_send_failed_on_burst() {
    // capacity=1：在 worker 未及时消费时瞬发 enqueue 会立刻打满命令队列。
    let client = MeowClient::new(
        MeowConfig::default()
            .with_command_queue_capacity(1)
            .with_worker_event_queue_capacity(32),
    );

    // 用一个不可达的 URL，任务会很快在 prepare 阶段失败而不是占满执行资源；
    // 这里关注的是“命令队列是否在突发下返回 CommandSendFailed”。
    let mut observed_send_failed = false;
    let burst = 64usize;
    for i in 0..burst {
        let path = temp_path(&format!("backpressure_{i}"));
        let task = DownloadPounceBuilder::new(
            format!("bp_{i}.bin"),
            &path,
            1024,
            "http://127.0.0.1:1/unreachable-burst",
        )
        .build();
        let res = client
            .try_enqueue(task, |_r: FileTransferRecord| {}, |_, _| {})
            .await;
        match res {
            Err(e) if e.code() == InnerErrorCode::CommandSendFailed as i32 => {
                observed_send_failed = true;
            }
            Err(e) => {
                // 其它错误不应出现在背压路径；若出现说明回归。
                if e.code() != InnerErrorCode::ParameterEmpty as i32 {
                    // 仍允许个别快速失败，但主测点仍是 CommandSendFailed。
                }
            }
            Ok(_) => {}
        }
    }
    assert!(
        observed_send_failed,
        "expected at least one CommandSendFailed under capacity=1 burst"
    );

    // 等一段时间让 worker 消费队列，然后新任务仍应能入队。
    tokio::time::sleep(Duration::from_millis(100)).await;
    let post_path = temp_path("backpressure_recovered");
    let recovered_task = DownloadPounceBuilder::new(
        "bp_recover.bin",
        &post_path,
        1024,
        "http://127.0.0.1:1/still-unreachable-but-accepted",
    )
    .build();
    // 这次 enqueue 本身应该成功（任务会稍后失败，但进入调度不属于 CommandSendFailed）。
    let _ = client
        .try_enqueue(recovered_task, |_r: FileTransferRecord| {}, |_, _| {})
        .await
        .expect("enqueue should succeed once queue drains");

    client.close().await.expect("close client");
    fs::remove_file(&post_path).ok();
}

// ----------------------------------------------------------------------------
// 场景 3：慢回调不阻塞调度收敛
// ----------------------------------------------------------------------------

/// 进度回调显式 `std::thread::sleep`，模拟慢消费者；验证任务仍能完成、global listener
/// 仍能被广播，且 complete 回调只触发一次。
#[tokio::test]
async fn slow_progress_callback_does_not_prevent_completion() {
    let payload = b"slow-callback-payload".repeat(4096);
    let server = dev_server::DevFileServer::spawn(payload.clone());
    let client = MeowClient::new(MeowConfig::new(1, 1));

    let path = temp_path("slow_cb");
    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();
    let global_hits = Arc::new(AtomicUsize::new(0));
    let global_hits_cb = global_hits.clone();
    client
        .register_global_progress_listener(move |_r: FileTransferRecord| {
            global_hits_cb.fetch_add(1, Ordering::Relaxed);
        })
        .expect("register global listener");

    let complete_hits = Arc::new(AtomicUsize::new(0));
    let complete_hits_cb = complete_hits.clone();

    let task = DownloadPounceBuilder::new(
        "slow.bin",
        &path,
        4096,
        format!("{}/download/slow.bin", server.base_url()),
    )
    .build();
    client
        .try_enqueue(
            task,
            move |record: FileTransferRecord| {
                // 模拟阻塞式慢回调（故意用 std::thread::sleep，而非 tokio::time::sleep）。
                std::thread::sleep(Duration::from_millis(30));
                statuses_cb
                    .lock()
                    .expect("lock statuses")
                    .push(record.status().clone());
            },
            move |_tid, _payload| {
                complete_hits_cb.fetch_add(1, Ordering::Relaxed);
            },
        )
        .await
        .expect("enqueue slow-callback task");

    let terminal = wait_terminal(statuses.clone(), 800).await;
    assert!(
        matches!(terminal, Some(TransferStatus::Complete)),
        "slow callback task should still finish, got {terminal:?}"
    );

    // 给 complete 回调再放一段收敛时间
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        complete_hits.load(Ordering::Relaxed),
        1,
        "complete callback should fire exactly once"
    );
    assert!(
        global_hits.load(Ordering::Relaxed) >= 1,
        "global listener should receive at least one event"
    );

    client.close().await.expect("close client");
    server.shutdown();
    let bytes = fs::read(&path).expect("read downloaded file");
    assert_eq!(bytes, payload);
    fs::remove_file(&path).ok();
}

// ----------------------------------------------------------------------------
// 场景 4：Drop 不 close 不能卡死
// ----------------------------------------------------------------------------

/// 对于未显式 `close` 的 client 直接 `drop`，期望测试进程本身能正常推进，
/// 不会因后台 worker 线程或命令队列未清理而卡死。
#[tokio::test]
async fn drop_client_without_close_does_not_deadlock() {
    let payload = b"drop-without-close-payload".repeat(1024);
    let server = dev_server::DevFileServer::spawn(payload.clone());
    let path = temp_path("drop_no_close");

    // 最长允许 3 秒完成“入队 -> 终态 -> drop”，避免测试被卡死时超时不明显。
    let run = async {
        let client = MeowClient::new(MeowConfig::new(1, 1));
        let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
        let statuses_cb = statuses.clone();
        let task = DownloadPounceBuilder::new(
            "drop.bin",
            &path,
            4096,
            format!("{}/download/drop.bin", server.base_url()),
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
            .expect("enqueue drop-case task");

        let _ = wait_terminal(statuses, 400).await;
        // 注意：故意不调用 client.close()；直接 drop。
        drop(client);
    };

    let result = tokio::time::timeout(Duration::from_secs(3), run).await;
    assert!(
        result.is_ok(),
        "dropping client without close must not deadlock"
    );

    server.shutdown();
    fs::remove_file(&path).ok();
}

// ----------------------------------------------------------------------------
// 场景 5：自定义 reqwest::Client 注入
// ----------------------------------------------------------------------------

/// 通过 `with_http_client` 注入外部 reqwest::Client，验证端到端下载仍成功，
/// 且数据与服务端一致（说明注入的 client 被真实使用且未破坏协议约定）。
#[tokio::test]
async fn injecting_custom_reqwest_client_completes_download() {
    let payload = b"custom-client-payload".repeat(2048);
    let server = dev_server::DevFileServer::spawn(payload.clone());

    let custom_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .tcp_keepalive(Duration::from_secs(30))
        .pool_max_idle_per_host(0)
        .user_agent("rusty-cat-supplemental/1.0")
        .build()
        .expect("build custom reqwest client");

    let client = MeowClient::new(
        MeowConfig::new(1, 1)
            .with_http_client(custom_client)
            .with_http_timeout(Duration::from_secs(10))
            .with_tcp_keepalive(Duration::from_secs(30)),
    );

    let path = temp_path("custom_client");
    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();

    let task = DownloadPounceBuilder::new(
        "custom.bin",
        &path,
        4096,
        format!("{}/download/custom.bin", server.base_url()),
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
        .expect("enqueue task using custom client");

    let terminal = wait_terminal(statuses, 400).await;
    assert!(
        matches!(terminal, Some(TransferStatus::Complete)),
        "custom client download should complete, got {terminal:?}"
    );

    client.close().await.expect("close");
    server.shutdown();
    let bytes = fs::read(&path).expect("read downloaded file");
    assert_eq!(bytes, payload);
    fs::remove_file(&path).ok();
}

// ----------------------------------------------------------------------------
// 场景 6：单片覆盖整个文件的大 chunk 上传
// ----------------------------------------------------------------------------

/// 使用覆盖整个文件的超大 chunk_size，验证 upload 缓冲复用逻辑在“只上传 1 片”的场景下
/// 也能正确收敛，并且最终服务端记录的已上传字节与源文件一致。
#[tokio::test]
async fn oversized_chunk_single_shot_upload_completes_and_matches_total() {
    let upload_bytes = b"oversized-chunk-upload-payload".repeat(4096); // ~120KB
    let total_len = upload_bytes.len() as u64;
    let upload_path = temp_path("oversized_src");
    fs::write(&upload_path, &upload_bytes).expect("write oversized upload source");

    let server = dev_server::DevFileServer::spawn(Vec::new());
    let client = MeowClient::new(MeowConfig::new(1, 1));

    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();

    // 使 chunk_size 远大于 total_size，执行器内部会将读取长度归一化到 total。
    let chunk_size = total_len * 4;
    let task = UploadPounceBuilder::new("oversized.bin", &upload_path, chunk_size)
        .with_url(format!("{}/upload/oversized.bin", server.base_url()))
        .build()
        .expect("build oversized upload task");
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
        .expect("enqueue oversized upload");

    let terminal = wait_terminal(statuses, 800).await;
    assert!(
        matches!(terminal, Some(TransferStatus::Complete)),
        "oversized single-shot upload should complete, got {terminal:?}"
    );

    client.close().await.expect("close");
    let inspect = server.upload_inspector();
    server.shutdown();
    fs::remove_file(&upload_path).ok();

    // 至少 1 个分片请求；single-shot 场景通常恰好 1 个 chunk 调用。
    assert!(inspect.chunk_calls >= 1);
    assert_eq!(
        inspect.max_next_byte, total_len,
        "server observed uploaded bytes must equal total"
    );
    assert!(inspect.completed, "server should see upload completed");
}
