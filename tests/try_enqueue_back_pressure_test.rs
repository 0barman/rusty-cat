//! 验证 `MeowClient::try_enqueue` 的 **非阻塞背压** 语义：
//!
//! - 当内部命令队列（由 [`rusty_cat::meow_config::MeowConfig::with_command_queue_capacity`]
//!   控制）被打满时，`try_enqueue` 会立即返回 `CommandSendFailed` 而非挂起
//!   调用方。
//! - 重新让出调度、等待后台消费后，再次 `try_enqueue` 可以成功。
//!
//! 复现思路：
//! 1. 调度并发设为 0，保证任务一旦入队就停留在 queued 队列，不会被
//!    消费掉；
//! 2. 命令队列容量设为 1，提交第 1 个 enqueue 后队列已满；
//! 3. 连续提交第 2 个 enqueue，应在一瞬间拿到 `CommandSendFailed`。
//!
//! 参考文档：`MeowClient::try_enqueue` doc 中的 “Back-pressure semantics” 节。

#[path = "dev_server/mod.rs"]
mod dev_server;

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusty_cat::down_pounce_builder::DownloadPounceBuilder;
use rusty_cat::error::InnerErrorCode;
use rusty_cat::file_transfer_record::FileTransferRecord;
use rusty_cat::meow_config::MeowConfig;
use rusty_cat::MeowClient;

fn temp_path(case: &str, i: usize) -> PathBuf {
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    p.push(format!("rusty_cat_try_enqueue_bp_{case}_{i}_{ts}.bin"));
    p
}

fn make_task(i: usize, base_url: &str) -> rusty_cat::api::PounceTask {
    let path = temp_path("burst", i);
    DownloadPounceBuilder::new(
        format!("bp_{i}.bin"),
        &path,
        1024,
        format!("{base_url}/download/bp_{i}.bin"),
    )
    .build()
}

#[tokio::test]
async fn try_enqueue_returns_command_send_failed_when_queue_is_full() {
    // 并发=0 → 调度器不会消费 queued 任务；
    // command_queue_capacity=1 → 命令通道只能缓冲一条命令。
    // 第 1 条 Enqueue 命令被立刻消费（worker 处理 Enqueue 会把 task 放进 queued），
    // 第 2 条 Enqueue 在极短时间窗口内如果赶上“worker 尚未 recv”那一刻，
    // try_send 就会拿到 Full 错误。为了稳定复现，我们一次性同步发起多次 try_enqueue，
    // 并要求「至少一次」快速失败。
    let payload = b"try-enqueue-back-pressure".repeat(8);
    let server = dev_server::DevFileServer::spawn(payload);
    let client = MeowClient::new(
        MeowConfig::new(0, 0)
            .with_command_queue_capacity(1)
            .with_worker_event_queue_capacity(1),
    );

    // 把一批 Enqueue 命令几乎在同一 tick 内发出去，worker 只能消费 1 条/tick，
    // 后续应至少出现一次 Full。
    let burst = 32usize;
    let mut results = Vec::with_capacity(burst);
    for i in 0..burst {
        let base_url = server.base_url().to_string();
        let task = make_task(i, &base_url);
        let res = client
            .try_enqueue(task, |_r: FileTransferRecord| {}, |_, _| {})
            .await;
        results.push(res);
    }

    let failed: Vec<_> = results.iter().filter_map(|r| r.as_ref().err()).collect();
    assert!(
        !failed.is_empty(),
        "expected at least one try_enqueue to fail with CommandSendFailed \
         under a 1-slot command queue + 0 concurrency, got all successes"
    );
    for err in &failed {
        assert_eq!(
            err.code(),
            InnerErrorCode::CommandSendFailed as i32,
            "fail-fast error must be CommandSendFailed; got: {err:?}"
        );
    }

    client.close().await.expect("close client");
    server.shutdown();
}

#[tokio::test]
async fn try_enqueue_is_fail_fast_not_waiting_for_queue_capacity() {
    // 直接断言“fail-fast”时间上界：挤满队列后立刻再提交一次，
    // 从调用到返回错误的墙钟时间应当非常短（远小于 send().await 阻塞所需的时长）。
    let payload = b"fail-fast".repeat(8);
    let server = dev_server::DevFileServer::spawn(payload);
    let client = MeowClient::new(
        MeowConfig::new(0, 0)
            .with_command_queue_capacity(1)
            .with_worker_event_queue_capacity(1),
    );

    // 通过一轮并发 burst 把队列挤满。只要其中任何一次 enqueue 失败，
    // 就证明我们进入了“队列已满”状态。
    let mut saw_failure = false;
    for i in 0..64usize {
        let base_url = server.base_url().to_string();
        let task = make_task(i, &base_url);
        let start = Instant::now();
        let res = client
            .try_enqueue(task, |_r: FileTransferRecord| {}, |_, _| {})
            .await;
        let elapsed = start.elapsed();

        if let Err(err) = res {
            assert_eq!(err.code(), InnerErrorCode::CommandSendFailed as i32);
            // 关键断言：失败必须是立即返回的，而不是等待队列有空位后才失败。
            // 给出一个宽松的上界（500ms）避免 CI 抖动；真实路径应在毫秒级。
            assert!(
                elapsed < Duration::from_millis(500),
                "try_enqueue should fail fast on Full; elapsed={elapsed:?}"
            );
            saw_failure = true;
            break;
        }
    }
    assert!(
        saw_failure,
        "could not trigger a queue-full condition; consider increasing burst size"
    );

    client.close().await.expect("close client");
    server.shutdown();
}
