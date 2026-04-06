use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use rusty_cat::log;
use rusty_cat::{
    debug_log_listener_active, set_debug_log_listener, try_set_debug_log_listener, Log, LogLevel,
};

#[test]
fn log_listener_register_duplicate_and_panic_suppression_paths() {
    // 场景说明：
    // 1) 初始状态应未注册监听器；emit/emit_lazy 在无监听器时应直接返回；
    // 2) 注册监听器后 active 变为 true，emit/emit_lazy 都应触发回调；
    // 3) 监听器内部 panic 不应向外传播（emit 会 catch_unwind）；
    // 4) 第二次 try_set 注册应返回错误，覆盖 try_set 的“已存在”分支。
    set_debug_log_listener(None).expect("ensure no listener before test");
    assert!(
        !debug_log_listener_active(),
        "fresh test process should not have log listener yet"
    );

    // 无监听器时这两行应无副作用且不 panic。
    log::emit(Log::new(LogLevel::Info, "log_test", "no listener path"));
    log::emit_lazy(|| Log::debug("log_test", "no listener lazy path"));

    let call_count = Arc::new(AtomicUsize::new(0));
    let count_ref = call_count.clone();
    try_set_debug_log_listener(move |entry: Log| {
        count_ref.fetch_add(1, Ordering::Relaxed);
        if entry.tag() == "panic_tag" {
            panic!("listener panic for suppression test");
        }
    })
    .expect("first log listener registration should succeed");
    assert!(debug_log_listener_active(), "listener should become active");

    log::emit(Log::new(LogLevel::Info, "normal_tag", "emit path"));
    log::emit_lazy(|| Log::new(LogLevel::Warn, "lazy_tag", "emit_lazy path"));
    // 该调用会触发监听器 panic，但 emit 应该吞掉 panic 并继续返回。
    log::emit(Log::new(
        LogLevel::Debug,
        "panic_tag",
        "panic suppression path",
    ));

    assert!(
        call_count.load(Ordering::Relaxed) >= 3,
        "listener should be invoked for normal/lazy/panic-tag emits"
    );

    let second_set = try_set_debug_log_listener(|_entry: Log| {});
    assert!(
        second_set.is_err(),
        "second try_set should fail when listener already exists"
    );
    set_debug_log_listener(None).expect("clear listener after test");
}
