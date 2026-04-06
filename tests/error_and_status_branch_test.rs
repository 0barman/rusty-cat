use rusty_cat::error::{InnerErrorCode, MeowError};
use rusty_cat::transfer_status::TransferStatus;

#[test]
fn meow_error_constructors_display_and_source_branches() {
    // 场景说明：
    // 1) 分别走 MeowError::new / from_code1 / from_code / from_source 构造路径；
    // 2) 校验 display 在“空消息/非空消息”两条分支的格式；
    // 3) 校验 from_source 路径能正确暴露 source，覆盖 StdError::source 分支。
    let via_new = MeowError::new(321, "new-path".to_string());
    assert_eq!(via_new.code(), 321);
    assert_eq!(via_new.msg(), "new-path");
    assert!(format!("{via_new}").contains("msg=new-path"));

    let via_code1 = MeowError::from_code1(InnerErrorCode::ParameterEmpty);
    assert_eq!(via_code1.code(), InnerErrorCode::ParameterEmpty as i32);
    let code1_text = format!("{via_code1}");
    assert!(
        code1_text.contains("MeowError(code=") && !code1_text.contains("msg="),
        "from_code1 should hit display branch without msg"
    );

    let via_code = MeowError::from_code(
        InnerErrorCode::InvalidTaskState,
        "invalid state branch".to_string(),
    );
    assert_eq!(via_code.code(), InnerErrorCode::InvalidTaskState as i32);
    assert!(format!("{via_code}").contains("invalid state branch"));

    let io_src = std::io::Error::other("io source branch");
    let via_source = MeowError::from_source(InnerErrorCode::IoError, "io failed", io_src);
    assert_eq!(via_source.code(), InnerErrorCode::IoError as i32);
    assert!(
        std::error::Error::source(&via_source).is_some(),
        "from_source branch should retain source error"
    );
}

#[test]
fn meow_error_partial_eq_and_neq_branches() {
    // 场景说明：
    // 1) 验证 MeowError PartialEq 对“相同 code+msg”返回 true；
    // 2) 验证 code 不同、msg 不同两个分支均返回 false；
    // 3) 该用例覆盖 error 对象比较逻辑的正反路径。
    let a1 = MeowError::from_code_str(InnerErrorCode::HttpError, "same");
    let a2 = MeowError::from_code_str(InnerErrorCode::HttpError, "same");
    assert_eq!(a1, a2, "same code and msg should be equal");

    let b1 = MeowError::from_code_str(InnerErrorCode::HttpError, "same");
    let b2 = MeowError::from_code_str(InnerErrorCode::IoError, "same");
    assert_ne!(b1, b2, "different code should not be equal");

    let c1 = MeowError::from_code_str(InnerErrorCode::HttpError, "m1");
    let c2 = MeowError::from_code_str(InnerErrorCode::HttpError, "m2");
    assert_ne!(c1, c2, "different msg should not be equal");
}

#[test]
fn transfer_status_as_i32_covers_all_enum_variants() {
    // 场景说明：
    // 1) 枚举构造 TransferStatus 所有分支；
    // 2) 验证 as_i32 映射值与协议约定一致；
    // 3) 该用例确保新增状态或重构时不会破坏对外数值兼容性。
    assert_eq!(TransferStatus::None.as_i32(), -1);
    assert_eq!(TransferStatus::Pending.as_i32(), 0);
    assert_eq!(TransferStatus::Transmission.as_i32(), 1);
    assert_eq!(TransferStatus::Paused.as_i32(), 2);
    assert_eq!(TransferStatus::Complete.as_i32(), 3);
    assert_eq!(
        TransferStatus::Failed(MeowError::from_code1(InnerErrorCode::Unknown)).as_i32(),
        4
    );
    assert_eq!(TransferStatus::Canceled.as_i32(), 5);
}
