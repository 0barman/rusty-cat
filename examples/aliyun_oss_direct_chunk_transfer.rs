mod support;

use std::sync::Arc;

use chrono::{Local, Utc};
use rusty_cat::aliyun_oss_direct::{AliOssDirectDownload, AliOssDirectUpload};
use rusty_cat::api::{DownloadPounceBuilder, MeowClient, UploadPounceBuilder};

use support::{
    config, make_file, placeholder, remove_if_exists, run_task, temp_path, AnyResult, ONE_MB,
};

const BUCKET_NAME: &str = "";
const ACCESS_KEY_ID: &str = "";
const ACCESS_KEY_SECRET: &str = "";
const REGION: &str = "cn-beijing";
const OBJECT_KEY_DIR: &str = "/test";
const UPLOAD_FILE_SIZE: usize = 20 * 1024 * 1024;

fn object_url(object_key: &str) -> String {
    format!("{}/{}", endpoint(), object_key.trim_start_matches('/'))
}

fn current_object_key() -> String {
    let date = Local::now().format("%Y-%m%d");
    let random = format!(
        "{:04x}",
        Utc::now().timestamp_nanos_opt().unwrap_or_default() & 0xffff
    );
    format!(
        "{}/direct-aliyun-feature-{date}-{random}-20mb.bin",
        OBJECT_KEY_DIR.trim_matches('/')
    )
}

fn bucket_host() -> String {
    format!("{BUCKET_NAME}.oss-{REGION}.aliyuncs.com")
}

fn endpoint() -> String {
    format!("https://{}", bucket_host())
}

fn current_file_name() -> String {
    Local::now()
        .format("rusty_cat_aliyun_direct_feature_%Y%m%d_%H_%M_20mb.bin")
        .to_string()
}

#[tokio::main]
async fn main() -> AnyResult<()> {
    if BUCKET_NAME.trim().is_empty()
        || ACCESS_KEY_ID.trim().is_empty()
        || ACCESS_KEY_SECRET.trim().is_empty()
    {
        println!(
            "please fill BUCKET_NAME, ACCESS_KEY_ID and ACCESS_KEY_SECRET at the top of this file first"
        );
        return Ok(());
    }
    if placeholder(REGION) {
        println!("please fill REGION at the top of this file first");
        return Ok(());
    }

    let file_name = current_file_name();
    let object_key = current_object_key();
    let object_url = object_url(&object_key);
    let upload_path = temp_path(&format!(
        "rusty_cat_aliyun_direct_feature_upload/{file_name}"
    ));
    let download_path = temp_path(&format!(
        "rusty_cat_aliyun_direct_feature_download/{file_name}"
    ));
    remove_if_exists(&upload_path)?;
    remove_if_exists(&download_path)?;
    make_file(&upload_path, UPLOAD_FILE_SIZE)?;

    let client = MeowClient::new(config()?);
    let upload_protocol =
        AliOssDirectUpload::new(BUCKET_NAME, ACCESS_KEY_ID, ACCESS_KEY_SECRET, REGION);
    let upload_task =
        UploadPounceBuilder::new("aliyun-direct-feature-upload.bin", &upload_path, ONE_MB)
            .with_url(object_url.clone())
            .with_breakpoint_upload(Arc::new(upload_protocol))
            .build()?;
    println!("aliyun feature direct upload url: {object_url}");
    run_task(&client, upload_task, "aliyun feature direct upload").await?;

    let download_protocol =
        AliOssDirectDownload::new(BUCKET_NAME, ACCESS_KEY_ID, ACCESS_KEY_SECRET, REGION);
    let download_task = DownloadPounceBuilder::new(
        "aliyun-direct-feature-download.bin",
        &download_path,
        ONE_MB,
        object_url.clone(),
    )
    .with_breakpoint_download(Arc::new(download_protocol))
    .build();
    println!("aliyun feature direct download url: {object_url}");
    run_task(&client, download_task, "aliyun feature direct download").await?;

    client.close().await?;
    remove_if_exists(&upload_path)?;
    remove_if_exists(&download_path)?;
    Ok(())
}
