mod support;

use std::sync::Arc;

use chrono::{Local, Utc};
use rusty_cat::api::{DownloadPounceBuilder, MeowClient, UploadPounceBuilder};
use rusty_cat::azure_blob_direct::{AzureBlobDirectDownload, AzureBlobDirectUpload};

use support::{
    config, make_file, placeholder, remove_if_exists, run_task, temp_path, AnyResult, ONE_MB,
};

const ACCOUNT_NAME: &str = "";
const ACCOUNT_KEY: &str = "";
const CONTAINER_NAME: &str = "test";
const BLOB_NAME_DIR: &str = "rusty-cat/examples";
const UPLOAD_FILE_SIZE: usize = 20 * 1024 * 1024;

fn blob_url(blob_name: &str) -> String {
    format!(
        "https://{ACCOUNT_NAME}.blob.core.windows.net/{}/{}",
        CONTAINER_NAME.trim_matches('/'),
        blob_name.trim_start_matches('/')
    )
}

fn current_blob_name() -> String {
    let date = Local::now().format("%Y-%m%d");
    let random = format!(
        "{:04x}",
        Utc::now().timestamp_nanos_opt().unwrap_or_default() & 0xffff
    );
    format!(
        "{}/direct-azure-feature-{date}-{random}-20mb.bin",
        BLOB_NAME_DIR.trim_matches('/')
    )
}

fn current_file_name() -> String {
    Local::now()
        .format("rusty_cat_azure_direct_feature_%Y%m%d_%H_%M_20mb.bin")
        .to_string()
}

fn ensure_azure_credentials(stage: &str) -> bool {
    if ACCOUNT_NAME.trim().is_empty() || ACCOUNT_KEY.trim().is_empty() {
        println!(
            "Azure direct {stage} cannot start: please fill ACCOUNT_NAME and ACCOUNT_KEY at the top of this file first"
        );
        return false;
    }
    true
}

#[tokio::main]
async fn main() -> AnyResult<()> {
    if !ensure_azure_credentials("upload/download") {
        return Ok(());
    }
    if placeholder(ACCOUNT_NAME) || placeholder(ACCOUNT_KEY) || placeholder(CONTAINER_NAME) {
        println!(
            "fill Azure ACCOUNT_NAME, ACCOUNT_KEY and CONTAINER_NAME constants at the top of this file first"
        );
        return Ok(());
    }

    let file_name = current_file_name();
    let blob_name = current_blob_name();
    let blob_url = blob_url(&blob_name);
    let upload_path = temp_path(&format!(
        "rusty_cat_azure_direct_feature_upload/{file_name}"
    ));
    let download_path = temp_path(&format!(
        "rusty_cat_azure_direct_feature_download/{file_name}"
    ));
    remove_if_exists(&upload_path)?;
    remove_if_exists(&download_path)?;
    make_file(&upload_path, UPLOAD_FILE_SIZE)?;

    let client = MeowClient::new(config()?);
    let upload_protocol = AzureBlobDirectUpload::new(ACCOUNT_NAME, ACCOUNT_KEY);
    let upload_task =
        UploadPounceBuilder::new("azure-direct-feature-upload.bin", &upload_path, ONE_MB)
            .with_url(blob_url.clone())
            .with_breakpoint_upload(Arc::new(upload_protocol))
            .build()?;
    println!("azure feature direct upload url: {blob_url}");
    run_task(&client, upload_task, "azure feature direct upload").await?;

    let download_protocol = AzureBlobDirectDownload::new(ACCOUNT_NAME, ACCOUNT_KEY);
    let download_task = DownloadPounceBuilder::new(
        "azure-direct-feature-download.bin",
        &download_path,
        ONE_MB,
        blob_url.clone(),
    )
    .with_breakpoint_download(Arc::new(download_protocol))
    .build();
    println!("azure feature direct download url: {blob_url}");
    run_task(&client, download_task, "azure feature direct download").await?;

    client.close().await?;
    remove_if_exists(&upload_path)?;
    remove_if_exists(&download_path)?;
    Ok(())
}
