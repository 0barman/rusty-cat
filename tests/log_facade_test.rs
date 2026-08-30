use std::error::Error as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rusty_cat::log;
use rusty_cat::{
    debug_log_listener_active, set_debug_log_listener, try_set_debug_log_listener, InnerErrorCode,
    Log, LogLevel, MeowError,
};

struct DebugLogListenerReset;

impl Drop for DebugLogListenerReset {
    fn drop(&mut self) {
        let _ = set_debug_log_listener(None);
    }
}

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
    let captured_messages = Arc::new(Mutex::new(Vec::new()));
    let captured_messages_ref = Arc::clone(&captured_messages);
    try_set_debug_log_listener(move |entry: Log| {
        count_ref.fetch_add(1, Ordering::Relaxed);
        captured_messages_ref
            .lock()
            .expect("captured log messages")
            .push(entry.message().to_owned());
        if entry.tag() == "panic_tag" {
            panic!("listener panic for suppression test");
        }
    })
    .expect("first log listener registration should succeed");
    let listener_reset = DebugLogListenerReset;
    assert!(debug_log_listener_active(), "listener should become active");

    log::emit(Log::new(LogLevel::Info, "normal_tag", "emit path"));
    log::emit_lazy(|| Log::new(LogLevel::Warn, "lazy_tag", "emit_lazy path"));
    // 该调用会触发监听器 panic，但 emit 应该吞掉 panic 并继续返回。
    log::emit(Log::new(
        LogLevel::Debug,
        "panic_tag",
        "panic suppression path",
    ));

    // Error values retain their original diagnostic message for the caller,
    // but constructor-side debug breadcrumbs must never publish signed URL or
    // token values through the global listener.
    const SECRET: &str = "constructor-secret-value";
    let raw = MeowError::new(9999, format!("https://example.invalid/a?sig={SECRET}"));
    let coded = MeowError::from_code(
        InnerErrorCode::HttpError,
        format!("https://example.invalid/b?token={SECRET}"),
    );
    let credential = format!("https://example.invalid/c?X-Amz-Credential={SECRET}");
    let borrowed = MeowError::from_code_str(InnerErrorCode::ResponseStatusError, &credential);
    let sourced = MeowError::from_source(
        InnerErrorCode::HttpError,
        format!("https://example.invalid/d?x-oss-signature={SECRET}"),
        std::io::Error::other(format!(
            "upstream https://example.invalid/e?security-token={SECRET}"
        )),
    );
    for error in [&raw, &coded, &borrowed, &sourced] {
        assert!(
            error.msg().contains(SECRET),
            "log redaction must not change the public error value: {error:?}"
        );
    }
    assert!(
        sourced
            .source()
            .expect("source error remains available")
            .to_string()
            .contains(SECRET),
        "log redaction must not change the public error source"
    );

    assert!(
        call_count.load(Ordering::Relaxed) >= 3,
        "listener should be invoked for normal/lazy/panic-tag emits"
    );

    let second_set = try_set_debug_log_listener(|_entry: Log| {});
    assert!(
        second_set.is_err(),
        "second try_set should fail when listener already exists"
    );
    drop(listener_reset);

    let messages = captured_messages.lock().expect("captured log messages");
    assert!(
        messages.iter().all(|message| !message.contains(SECRET)),
        "error constructor debug logs leaked a secret: {messages:?}"
    );
    let error_breadcrumbs = messages
        .iter()
        .filter(|message| message.contains("MeowError::"))
        .collect::<Vec<_>>();
    assert_eq!(
        error_breadcrumbs.len(),
        4,
        "every exercised constructor must emit one breadcrumb: {messages:?}"
    );
    assert!(
        error_breadcrumbs
            .iter()
            .all(|message| message.contains("REDACTED")),
        "every error breadcrumb carrying a secret must visibly redact it: {messages:?}"
    );
}
