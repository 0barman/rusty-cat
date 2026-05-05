use rusty_cat::error::InnerErrorCode;
use rusty_cat::meow_config::MeowConfig;

#[test]
fn zero_concurrency_is_rejected_by_config_builder() {
    // 场景说明：
    // 1) 过去曾用并发=0 表示“只入队不执行”；
    // 2) MeowConfig 改为 build() 集中硬校验后，0 已不再是合法调度语义；
    // 3) 这里锁定构造期错误，避免非法值延迟到运行期或造成静默卡队列。
    let err = MeowConfig::builder()
        .max_upload_concurrency(0)
        .max_download_concurrency(1)
        .build()
        .expect_err("zero upload concurrency must be rejected");
    assert_eq!(err.code(), InnerErrorCode::ParameterEmpty as i32);

    let err = MeowConfig::builder()
        .max_upload_concurrency(1)
        .max_download_concurrency(0)
        .build()
        .expect_err("zero download concurrency must be rejected");
    assert_eq!(err.code(), InnerErrorCode::ParameterEmpty as i32);
}
