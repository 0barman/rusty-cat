use rusty_cat::api::*;

#[test]
fn api_facade_re_exports_core_public_types() {
    let _client = MeowClient::new(MeowConfig::default());
    let _upload_protocol: DefaultStyleUpload = DefaultStyleUpload::default();
    let _download_protocol: StandardRangeDownload = StandardRangeDownload;
    let _status = TransferStatus::Pending;
    let _level = LogLevel::Info;
    let _err = MeowError::from_code1(InnerErrorCode::Unknown);
}
