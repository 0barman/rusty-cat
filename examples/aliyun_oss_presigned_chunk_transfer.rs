mod support;

use std::sync::Arc;

use rusty_cat::aliyun_oss_presigned as aliyun;
use rusty_cat::api::{
    DownloadPounceBuilder, MeowClient, PresignedMultipartUpload, PresignedMultipartUploadPlan,
    PresignedRangeDownload, PresignedRangeDownloadPlan, UploadPounceBuilder,
};

use support::{
    config, make_file, placeholder, remove_if_exists, run_task, temp_path, AnyResult, FIVE_MB,
    ONE_MB,
};

const UPLOAD_FILE_NAME: &str = "rusty_cat_aliyun_presigned_upload_5mb.bin";
const DOWNLOAD_FILE_NAME: &str = "rusty_cat_aliyun_presigned_download_5mb.bin";
const DOWNLOAD_URL: &str = "https://example.com/CHANGE_ME_ALIYUN_PRESIGNED_DOWNLOAD_URL";
const UPLOAD_PART_URLS: [&str; 5] = [
    "https://example.com/CHANGE_ME_ALIYUN_UPLOAD_PART_1",
    "https://example.com/CHANGE_ME_ALIYUN_UPLOAD_PART_2",
    "https://example.com/CHANGE_ME_ALIYUN_UPLOAD_PART_3",
    "https://example.com/CHANGE_ME_ALIYUN_UPLOAD_PART_4",
    "https://example.com/CHANGE_ME_ALIYUN_UPLOAD_PART_5",
];

#[tokio::main]
async fn main() -> AnyResult<()> {
    if placeholder(DOWNLOAD_URL) || UPLOAD_PART_URLS.iter().any(|url| placeholder(url)) {
        println!("fill Aliyun presigned URL constants at the top of this file first");
        return Ok(());
    }
    let upload_path = temp_path(UPLOAD_FILE_NAME);
    let download_path = temp_path(DOWNLOAD_FILE_NAME);
    remove_if_exists(&upload_path)?;
    remove_if_exists(&download_path)?;
    make_file(&upload_path, FIVE_MB)?;
    let parts = UPLOAD_PART_URLS
        .iter()
        .enumerate()
        .map(|(i, url)| aliyun::upload_part((i + 1) as u64, i as u64 * ONE_MB, ONE_MB, *url))
        .collect::<Vec<_>>();
    let upload_protocol = PresignedMultipartUpload::new(PresignedMultipartUploadPlan::new(
        FIVE_MB as u64,
        ONE_MB,
        parts,
    ));
    let download_protocol = PresignedRangeDownload::new(
        PresignedRangeDownloadPlan::new(DOWNLOAD_URL).with_total_size(FIVE_MB as u64),
    );
    let client = MeowClient::new(config()?);
    let upload_task = UploadPounceBuilder::new("aliyun-presigned-upload.bin", &upload_path, ONE_MB)
        .with_url(UPLOAD_PART_URLS[0])
        .with_breakpoint_upload(Arc::new(upload_protocol))
        .build()?;
    run_task(&client, upload_task, "aliyun presigned upload").await?;
    let download_task = DownloadPounceBuilder::new(
        "aliyun-presigned-download.bin",
        &download_path,
        ONE_MB,
        DOWNLOAD_URL,
    )
    .with_breakpoint_download(Arc::new(download_protocol))
    .build();
    run_task(&client, download_task, "aliyun presigned download").await?;
    client.close().await?;
    remove_if_exists(&upload_path)?;
    remove_if_exists(&download_path)?;
    Ok(())
}
