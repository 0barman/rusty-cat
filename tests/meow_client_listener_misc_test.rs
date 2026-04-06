use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use rusty_cat::meow_config::MeowConfig;
use rusty_cat::MeowClient;

#[test]
fn unregister_same_listener_twice_second_time_returns_false() {
    // 场景说明：
    // 1) 注册一个全局监听器，首次 unregister 应返回 true；
    // 2) 对同一个 id 再次 unregister，应返回 false（miss 分支）；
    // 3) 该用例覆盖 unregister_global_progress_listener 中“未命中 id”路径。
    let client = MeowClient::new(MeowConfig::new(1, 1));
    let counter = Arc::new(AtomicUsize::new(0));
    let c = counter.clone();

    let id = client
        .register_global_progress_listener(move |_record| {
            c.fetch_add(1, Ordering::Relaxed);
        })
        .expect("register global listener");

    let first = client
        .unregister_global_progress_listener(id)
        .expect("first unregister");
    assert!(first, "first unregister should remove listener");

    let second = client
        .unregister_global_progress_listener(id)
        .expect("second unregister");
    assert!(
        !second,
        "second unregister for same id should return false (not found)"
    );
}
