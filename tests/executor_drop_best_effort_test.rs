//! 验证 `MeowClient` 在未调用 `close()` 直接被 Drop 时，内部 `Executor` 会
//! 走 best-effort 关闭路径：
//!
//! 1. 通过全局日志监听器观察到 tag=`executor_drop`、Level=`Warn` 的提示；
//! 2. 正常 `close().await` 之后再 Drop 不会产生该 Warn；
//! 3. Drop 路径是非阻塞的，不会挂起测试进程。
//!
//! 说明：debug log listener 是进程级单例，因此把这些断言放在独立的
//! integration test 文件（= 独立测试二进制）里，避免和其他测试并发冲突。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use rusty_cat::api::{Log, LogLevel, MeowClient, MeowConfig};
use rusty_cat::{set_debug_log_listener, DebugLogListener};

/// 全局日志监听器是进程级单例，而 Rust 集成测试默认在同一二进制内并行执行。
/// 为避免各用例互相覆盖监听器，使用一把专属互斥量串行化这些用例。
fn serial_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .expect("serial guard lock")
}

/// 线程安全地聚合 `executor_drop` 相关日志，供断言使用。
#[derive(Default)]
struct DropLogCollector {
    warn_count: AtomicUsize,
    debug_messages: Mutex<Vec<String>>,
}

impl DropLogCollector {
    fn install(self: &Arc<Self>) {
        let me = Arc::clone(self);
        let listener: DebugLogListener = Arc::new(move |log: Log| {
            if log.tag() != "executor_drop" {
                return;
            }
            match log.level() {
                LogLevel::Warn => {
                    me.warn_count.fetch_add(1, Ordering::SeqCst);
                }
                _ => {
                    me.debug_messages
                        .lock()
                        .expect("lock debug_messages")
                        .push(log.message().to_string());
                }
            }
        });
        set_debug_log_listener(Some(listener))
            .expect("set_debug_log_listener should succeed in isolated test binary");
    }

    fn uninstall() {
        // 清理监听器，避免对同一进程内的后续测试造成副作用。
        let _ = set_debug_log_listener(None);
    }

    fn warn_count(&self) -> usize {
        self.warn_count.load(Ordering::SeqCst)
    }

    fn debug_contains(&self, needle: &str) -> bool {
        self.debug_messages
            .lock()
            .expect("lock debug_messages")
            .iter()
            .any(|m| m.contains(needle))
    }
}

#[tokio::test]
async fn dropping_meow_client_without_close_emits_warn_and_does_not_hang() {
    let _guard = serial_guard();
    let collector = Arc::new(DropLogCollector::default());
    collector.install();

    {
        let client = MeowClient::new(MeowConfig::new(1, 1));
        // 强制触发内部 executor 懒初始化；否则 `OnceLock` 里为空，Drop 根本
        // 不会走到 Executor::drop，测不出 best-effort 路径。
        let _ = client
            .snapshot()
            .await
            .expect("snapshot should succeed on a fresh client");
        // `client` 在这里离开作用域，没有调用 `close()`。
    }

    // Drop 是同步且非阻塞的，上一行返回即意味着 Warn 日志已经 emit。
    assert_eq!(
        collector.warn_count(),
        1,
        "exactly one Warn log with tag=executor_drop should fire when close() was skipped"
    );
    // best-effort 路径会额外记录一条 flow 日志（任一分支均可接受）。
    assert!(
        collector.debug_contains("best-effort Close command queued during Drop")
            || collector.debug_contains("best-effort Close try_send skipped"),
        "best-effort drop path should emit a follow-up flow log; \
         got: {:?}",
        collector.debug_messages.lock().unwrap()
    );

    DropLogCollector::uninstall();
}

#[tokio::test]
async fn dropping_meow_client_after_close_does_not_emit_warn() {
    let _guard = serial_guard();
    let collector = Arc::new(DropLogCollector::default());
    collector.install();

    {
        let client = MeowClient::new(MeowConfig::new(1, 1));
        let _ = client
            .snapshot()
            .await
            .expect("snapshot should succeed on a fresh client");
        client.close().await.expect("explicit close should succeed");
        // 此刻再 Drop client，不应再触发 Warn。
    }

    assert_eq!(
        collector.warn_count(),
        0,
        "explicit close() must suppress the best-effort Drop warning"
    );
    assert!(
        collector.debug_contains("executor dropped after explicit close()"),
        "explicit close() path should still leave a debug trail in logs"
    );

    DropLogCollector::uninstall();
}

#[tokio::test]
async fn dropping_meow_client_without_ever_touching_executor_is_silent() {
    // 从未使用过的 client（executor 懒初始化槽位仍为空）被 Drop，
    // 不应触发任何 executor_drop 相关日志：因为根本没有后台线程需要回收。
    let _guard = serial_guard();
    let collector = Arc::new(DropLogCollector::default());
    collector.install();

    {
        let _client = MeowClient::new(MeowConfig::new(1, 1));
    }

    assert_eq!(
        collector.warn_count(),
        0,
        "untouched client has no executor, Drop must stay silent"
    );
    assert!(
        collector
            .debug_messages
            .lock()
            .expect("lock debug_messages")
            .is_empty(),
        "no executor_drop debug log expected either"
    );

    DropLogCollector::uninstall();
}
