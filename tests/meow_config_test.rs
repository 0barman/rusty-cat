use std::time::Duration;

use reqwest::Client;
use rusty_cat::http_breakpoint::BreakpointDownloadHttpConfig;
use rusty_cat::meow_config::MeowConfig;

#[test]
fn meow_config_default_and_builder_paths_work_as_expected() {
    // 场景说明：
    // 1) 验证 Default 路径的默认值；
    // 2) 通过 with_* builder 链覆盖 timeout/keepalive/http_client/breakpoint 配置；
    // 3) 验证 getter 返回值与设置值一致，覆盖配置对象主要分支。
    let default_cfg = MeowConfig::default();
    assert_eq!(default_cfg.max_upload_concurrency(), 2);
    assert_eq!(default_cfg.max_download_concurrency(), 2);
    assert_eq!(default_cfg.http_timeout(), Duration::from_secs(5));
    assert_eq!(default_cfg.tcp_keepalive(), Duration::from_secs(30));
    assert_eq!(
        default_cfg.breakpoint_download_http().range_accept,
        "application/octet-stream"
    );

    let custom_bp = BreakpointDownloadHttpConfig {
        range_accept: "application/custom-binary".to_string(),
    };
    let custom_cfg = MeowConfig::new(3, 4)
        .with_http_client(Client::new())
        .with_http_timeout(Duration::from_secs(11))
        .with_tcp_keepalive(Duration::from_secs(17))
        .with_breakpoint_download_http(custom_bp.clone());

    assert_eq!(custom_cfg.max_upload_concurrency(), 3);
    assert_eq!(custom_cfg.max_download_concurrency(), 4);
    assert_eq!(custom_cfg.http_timeout(), Duration::from_secs(11));
    assert_eq!(custom_cfg.tcp_keepalive(), Duration::from_secs(17));
    assert_eq!(custom_cfg.breakpoint_download_http(), &custom_bp);
}
