//! Verifies that abort/cleanup failures are surfaced at `WARN` level instead of
//! being silently swallowed, so billing-relevant orphaned multipart/uncommitted
//! block conditions stay observable.

use std::sync::{Arc, Mutex};

use rusty_cat::api::{set_debug_log_listener, DebugLogListener, Log, LogLevel};

type Captured = Arc<Mutex<Vec<(LogLevel, String, String)>>>;

#[test]
fn warn_macro_emits_warn_level_and_flow_log_stays_debug() {
    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    let listener: DebugLogListener = Arc::new(move |log: Log| {
        sink.lock().unwrap().push((
            log.level(),
            log.tag().to_string(),
            log.message().to_string(),
        ));
    });
    set_debug_log_listener(Some(listener)).expect("set listener");

    // Mirrors the executor cancel path: abort failed but cleanup continues.
    rusty_cat::meow_warn_log!(
        "cancel_group",
        "protocol abort failed but continue cleanup: uploadId={}",
        "uid-1"
    );
    rusty_cat::meow_flow_log!("cancel_group", "plain debug breadcrumb {}", 1);

    set_debug_log_listener(None).expect("clear listener");

    let logs = captured.lock().unwrap();

    let warn = logs
        .iter()
        .find(|(_, tag, msg)| tag == "cancel_group" && msg.contains("protocol abort failed"))
        .expect("warn entry captured");
    assert_eq!(warn.0, LogLevel::Warn, "abort failure must be WARN level");
    assert!(
        warn.2.contains("uid-1"),
        "warn message should carry the provider session id for cleanup"
    );

    let debug = logs
        .iter()
        .find(|(_, _, msg)| msg.contains("plain debug breadcrumb"))
        .expect("debug entry captured");
    assert_eq!(
        debug.0,
        LogLevel::Debug,
        "flow log must remain DEBUG level for contrast"
    );
}
