mod support;

use std::sync::Arc;

use rusty_cat::api::{
    DownloadPounceBuilder, MeowClient, PresignedMultipartUpload, PresignedMultipartUploadPlan,
    PresignedRangeDownload, PresignedRangeDownloadPlan, UploadPounceBuilder,
};
use rusty_cat::azure_blob_sas as azure;

use support::{
    config, make_file, placeholder, remove_if_exists, run_task, temp_path, AnyResult, FIVE_MB,
    ONE_MB,
};

const UPLOAD_FILE_NAME: &str = "rusty_cat_azure_sas_upload_5mb.bin";
const DOWNLOAD_FILE_NAME: &str = "rusty_cat_azure_sas_download_5mb.bin";
const BLOB_SAS_URL: &str = "https://example.blob.core.windows.net/container/blob.bin?CHANGE_ME_SAS";
const DOWNLOAD_SAS_URL: &str =
    "https://example.blob.core.windows.net/container/blob.bin?CHANGE_ME_DOWNLOAD_SAS";

#[tokio::main]
async fn main() -> AnyResult<()> {
    if placeholder(BLOB_SAS_URL) || placeholder(DOWNLOAD_SAS_URL) {
        println!("fill Azure SAS URL constants at the top of this file first");
        return Ok(());
    }
    let upload_path = temp_path(UPLOAD_FILE_NAME);
    let download_path = temp_path(DOWNLOAD_FILE_NAME);
    remove_if_exists(&upload_path)?;
    remove_if_exists(&download_path)?;
    make_file(&upload_path, FIVE_MB)?;
    let parts = (0..5)
        .map(|i| azure::put_block_from_blob_url(BLOB_SAS_URL, i, i as u64 * ONE_MB, ONE_MB))
        .collect::<Result<Vec<_>, _>>()?;
    let block_ids = (0..5).map(azure::block_id_by_index).collect::<Vec<_>>();
    let block_id_refs = block_ids.iter().map(String::as_str).collect::<Vec<_>>();
    let complete_request = azure::put_block_list_request(BLOB_SAS_URL, block_id_refs)?;
    let upload_protocol = PresignedMultipartUpload::new(
        PresignedMultipartUploadPlan::new(FIVE_MB as u64, ONE_MB, parts)
            .with_complete_request(complete_request),
    );
    let download_protocol = PresignedRangeDownload::new(
        PresignedRangeDownloadPlan::new(DOWNLOAD_SAS_URL).with_total_size(FIVE_MB as u64),
    );
    let client = MeowClient::new(config()?);
    let upload_task = UploadPounceBuilder::new("azure-sas-upload.bin", &upload_path, ONE_MB)
        .with_url(BLOB_SAS_URL)
        .with_breakpoint_upload(Arc::new(upload_protocol))
        .build()?;
    run_task(&client, upload_task, "azure sas upload").await?;
    let download_task = DownloadPounceBuilder::new(
        "azure-sas-download.bin",
        &download_path,
        ONE_MB,
        DOWNLOAD_SAS_URL,
    )
    .with_breakpoint_download(Arc::new(download_protocol))
    .build();
    run_task(&client, download_task, "azure sas download").await?;
    client.close().await?;
    remove_if_exists(&upload_path)?;
    remove_if_exists(&download_path)?;
    Ok(())
}
