#[path = "dev_server/mod.rs"]
mod dev_server;

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::Method;
use rusty_cat::debug_log_listener_active;
use rusty_cat::down_pounce_builder::DownloadPounceBuilder;
use rusty_cat::error::InnerErrorCode;
use rusty_cat::file_transfer_record::FileTransferRecord;
use rusty_cat::log;
use rusty_cat::meow_config::MeowConfig;
use rusty_cat::up_pounce_builder::UploadPounceBuilder;
use rusty_cat::{Log, MeowClient};

fn temp_path(case: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    p.push(format!("rusty_cat_meow_api_extra_{case}_{ts}.bin"));
    p
}

fn debug_listener_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[tokio::test]
async fn snapshot_on_fresh_client_reports_zero_groups() {
    // 场景说明：
    // 1) 在“尚未 enqueue 任何任务”的 fresh client 上直接调用 snapshot；
    // 2) snapshot 会触发 executor 初始化，但应返回空态（queued=0, active=0）；
    // 3) 覆盖 meow_client::snapshot + get_exec 初始化分支的空队列路径。
    let client = MeowClient::new(MeowConfig::new(1, 1));
    let snap = client.snapshot().await.expect("snapshot on fresh client");
    assert_eq!(snap.queued_groups, 0);
    assert_eq!(snap.active_groups, 0);
    assert!(snap.active_keys.is_empty());
    client.close().await.expect("close fresh client");
}

#[test]
fn meow_client_set_debug_log_listener_wrapper_supports_set_and_clear() {
    let _guard = debug_listener_test_lock()
        .lock()
        .expect("lock debug listener test mutex");
    // 场景说明：
    // 1) 通过 MeowClient::set_debug_log_listener(Some(...)) 设置监听器；
    // 2) active 应该变为 true；
    // 3) 再调用 set_debug_log_listener(None) 可取消注册，active 变回 false。
    let client = MeowClient::new(MeowConfig::new(1, 1));
    client
        .set_debug_log_listener(Some(Arc::new(|_log: Log| {})))
        .expect("set debug listener");
    assert!(
        debug_log_listener_active(),
        "listener should become active after setting"
    );
    client
        .set_debug_log_listener(None)
        .expect("clear debug listener");
    assert!(
        !debug_log_listener_active(),
        "listener should be inactive after clear"
    );
}

#[test]
fn meow_client_set_debug_log_listener_replaces_previous_listener() {
    let _guard = debug_listener_test_lock()
        .lock()
        .expect("lock debug listener test mutex");
    // 场景说明：
    // 1) 先设置监听器 A 并 emit 一次，A 计数应+1；
    // 2) 再设置监听器 B（替换）并 emit 一次；
    // 3) 第二次 emit 只应命中 B，证明是覆盖而不是叠加。
    let client = MeowClient::new(MeowConfig::new(1, 1));
    client
        .set_debug_log_listener(None)
        .expect("clear listener before replacement test");

    let a = Arc::new(AtomicUsize::new(0));
    let b = Arc::new(AtomicUsize::new(0));

    let a_ref = a.clone();
    client
        .set_debug_log_listener(Some(Arc::new(move |_log: Log| {
            a_ref.fetch_add(1, Ordering::Relaxed);
        })))
        .expect("set listener A");
    log::emit(Log::debug("replace_test", "first emit"));

    let b_ref = b.clone();
    client
        .set_debug_log_listener(Some(Arc::new(move |_log: Log| {
            b_ref.fetch_add(1, Ordering::Relaxed);
        })))
        .expect("replace listener with B");
    log::emit(Log::debug("replace_test", "second emit"));

    assert_eq!(
        a.load(Ordering::Relaxed),
        1,
        "listener A should only receive first emit"
    );
    assert_eq!(
        b.load(Ordering::Relaxed),
        1,
        "listener B should only receive second emit after replacement"
    );

    client
        .set_debug_log_listener(None)
        .expect("clear listener after replacement test");
}

#[tokio::test]
async fn upload_file_deleted_after_build_hits_file_not_found_branch() {
    // 场景说明：
    // 1) 先创建文件并执行 UploadPounceBuilder::build（此时 metadata 可读）；
    // 2) 在 enqueue 前删除源文件；
    // 3) InnerTask::from_pounce 打开文件失败，应返回 FileNotFound。
    let src = temp_path("upload_deleted_before_enqueue");
    fs::write(&src, b"temp-upload-content").expect("write temp source");

    let task = UploadPounceBuilder::new("gone.bin", &src, 1024)
        .with_url("http://127.0.0.1:9/upload")
        .build()
        .expect("build upload task before deletion");
    fs::remove_file(&src).expect("remove source before enqueue");

    let client = MeowClient::new(MeowConfig::new(1, 1));
    let err = client
        .enqueue(task, |_record: FileTransferRecord| {}, Some(|_, _| {}))
        .await
        .expect_err("enqueue should fail due to missing source");
    assert_eq!(err.code(), InnerErrorCode::FileNotFound as i32);
}

#[tokio::test]
async fn task_and_listener_id_debug_format_paths_are_observable() {
    // 场景说明：
    // 1) 注册全局监听器获取 GlobalProgressListenerId；
    // 2) enqueue 下载任务获取 TaskId；
    // 3) 对两个 id 做 Debug 格式化，覆盖 ids.rs 中 Debug 实现路径。
    let payload = b"id-format-payload".repeat(1024);
    let server = dev_server::DevFileServer::spawn(payload);
    let client = MeowClient::new(MeowConfig::new(1, 1));

    let listener_id = client
        .register_global_progress_listener(|_record: FileTransferRecord| {})
        .expect("register global listener");
    let listener_text = format!("{listener_id:?}");
    assert!(
        !listener_text.is_empty(),
        "listener id debug text should not be empty"
    );

    let path = temp_path("id_debug_task");
    let task = DownloadPounceBuilder::new(
        "id.bin",
        &path,
        2048,
        format!("{}/download/id.bin", server.base_url()),
        Method::GET,
    )
    .build();
    let task_id = client
        .enqueue(task, |_record: FileTransferRecord| {}, Some(|_, _| {}))
        .await
        .expect("enqueue task for id debug");
    let task_text = format!("{task_id:?}");
    assert!(
        !task_text.is_empty(),
        "task id debug text should not be empty"
    );

    client.close().await.expect("close client");
    let _ = fs::remove_file(&path);
    server.shutdown();
}

#[test]
fn meow_client_debug_impl_exposes_config_and_hides_listener_details() {
    // 场景说明：
    // 1) 直接格式化 MeowClient 的 Debug 输出；
    // 2) 验证输出包含结构名和 config 字段；
    // 3) 验证 global_progress_listener 被占位为 ".."（避免泄露闭包细节）。
    let client = MeowClient::new(MeowConfig::new(3, 7));
    let text = format!("{client:?}");
    assert!(
        text.contains("MeowClient"),
        "debug should contain struct name"
    );
    assert!(text.contains("config"), "debug should contain config field");
    assert!(
        text.contains("global_progress_listener"),
        "debug should contain listener field placeholder"
    );
    assert!(
        text.contains(".."),
        "listener details should be hidden as '..'"
    );
}
