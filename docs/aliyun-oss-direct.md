# Aliyun OSS direct upload/download guide

This guide explains how to use `rusty-cat` with Aliyun OSS direct AccessKey signing. Direct mode means the application process sends requests directly to OSS and signs each request with Aliyun credentials before it is sent.

Use direct mode only when the process running `rusty-cat` is trusted to hold cloud credentials, such as a backend service, an internal CLI, or a controlled server-side worker. For public desktop, mobile, or browser clients, prefer the presigned URL flow so long-lived AccessKey secrets never leave your backend.

Example source: [../examples/aliyun_oss_direct_chunk_transfer.rs](../examples/aliyun_oss_direct_chunk_transfer.rs)

## Security model

Direct mode requires:

| Secret/value | Required for | Notes |
|---|---|---|
| `bucket` | Upload and download | OSS bucket name, for example `my-bucket`. |
| `access_key_id` | Upload and download | Aliyun AccessKey ID. |
| `access_key_secret` | Upload and download | Aliyun AccessKey secret. Treat this as a secret. |
| `region` | Upload and download | Region string such as `cn-beijing`. |

`rusty-cat` does not persist these values in a database, config file, keychain, or cache. The SDK only keeps the values in the protocol object you create and uses them to sign HTTP requests in memory. Your application is responsible for loading them securely, rotating them, limiting their permissions, and ensuring they are not printed in logs or persisted in transfer records.

For untrusted desktop/mobile clients, prefer a backend-generated presigned URL flow instead of shipping long-lived AccessKey secrets to the client.

## Enable the feature

```toml
[dependencies]
rusty-cat = { version = "0.2.4", features = ["aliyun-oss-direct"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

Run the official example with:

```text
cargo run --example aliyun_oss_direct_chunk_transfer --features aliyun-oss-direct
```

Before running it, fill the constants at the top of [../examples/aliyun_oss_direct_chunk_transfer.rs](../examples/aliyun_oss_direct_chunk_transfer.rs): `BUCKET_NAME`, `ACCESS_KEY_ID`, `ACCESS_KEY_SECRET`, and `REGION`.

## Upload flow

The upload side uses `AliOssDirectUpload`, which implements `BreakpointUpload`.

High-level process:

1. Build the final object URL, usually `https://{bucket}.oss-{region}.aliyuncs.com/{object_key}`.
2. Create `MeowClient` from `MeowConfig`.
3. Create `AliOssDirectUpload::new(bucket, access_key_id, access_key_secret, region)`.
4. Build an upload task with `UploadPounceBuilder`.
5. Attach the protocol with `with_breakpoint_upload(Arc::new(upload_protocol))`.
6. Submit with `MeowClient::enqueue_and_wait(...)` (or manage terminal callbacks yourself).
7. Wait for the terminal result.
8. Persist your own business state if the upload must survive process restarts.
9. Call `client.close().await` during shutdown.

The object URL must point to the exact object you want to create. The direct protocol extracts the object key from this URL and uses it when creating, listing, uploading, and completing the OSS multipart upload.

```rust,no_run
use std::sync::Arc;

use rusty_cat::aliyun_oss_direct::AliOssDirectUpload;
use rusty_cat::api::{MeowClient, MeowConfig, UploadPounceBuilder};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let bucket = "my-bucket";
    let access_key_id = std::env::var("ALIYUN_ACCESS_KEY_ID")?;
    let access_key_secret = std::env::var("ALIYUN_ACCESS_KEY_SECRET")?;
    let region = "cn-beijing";
    let object_key = "uploads/demo.bin";
    let object_url = format!("https://{bucket}.oss-{region}.aliyuncs.com/{object_key}");

    let client = MeowClient::new(MeowConfig::builder().max_upload_concurrency(1).build()?);
    let upload_protocol = AliOssDirectUpload::new(bucket, access_key_id, access_key_secret, region);

    let task = UploadPounceBuilder::new("demo.bin", "./demo.bin", 1024 * 1024)
        .with_url(object_url)
        .with_breakpoint_upload(Arc::new(upload_protocol))
        .with_max_chunk_retries(3)
        .build()?;

    let outcome = client
        .enqueue_and_wait(
            task,
            |record| println!("upload progress={:.2}%", record.progress() * 100.0),
        )
        .await?;

    println!("upload {} complete: {:?}", outcome.task_id, outcome.payload);
    client.close().await?;
    Ok(())
}
```

### What the SDK does internally

`AliOssDirectUpload` performs the OSS multipart workflow:

1. Validates the object URL and decides whether to initiate or adopt a multipart upload session.
2. Signs OSS requests with OSS Signature Version 4 signing using the credentials you provided.
3. Checks uploaded parts when resuming.
4. Uploads each chunk as an OSS multipart part.
5. Completes the multipart upload after all chunks succeed.
6. Lets the executor apply the task retry policy when a chunk fails.

## Download flow

The download side uses `AliOssDirectDownload`, which implements `BreakpointDownload`. The executor still performs the normal resumable download sequence, but the provider plugin adds signed Aliyun OSS headers to the `HEAD` and range `GET` requests.

High-level process:

1. Build or receive the final OSS object URL.
2. Create `AliOssDirectDownload::new(bucket, access_key_id, access_key_secret, region)`.
3. Build a `DownloadPounceBuilder` task with a local output path and chunk size.
4. Attach the provider plugin with `with_breakpoint_download(Arc::new(download_protocol))`.
5. Submit with `enqueue_and_wait(...)` and monitor `FileTransferRecord` updates.
6. Close the client explicitly when your application shuts down.

```rust,no_run
use std::sync::Arc;

use rusty_cat::aliyun_oss_direct::AliOssDirectDownload;
use rusty_cat::api::{DownloadPounceBuilder, MeowClient, MeowConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let bucket = "my-bucket";
    let access_key_id = std::env::var("ALIYUN_ACCESS_KEY_ID")?;
    let access_key_secret = std::env::var("ALIYUN_ACCESS_KEY_SECRET")?;
    let region = "cn-beijing";
    let object_url = "https://my-bucket.oss-cn-beijing.aliyuncs.com/uploads/demo.bin";

    let client = MeowClient::new(MeowConfig::default());
    let download_protocol = AliOssDirectDownload::new(bucket, access_key_id, access_key_secret, region);

    let task = DownloadPounceBuilder::new("demo.bin", "./downloads/demo.bin", 1024 * 1024, object_url)
        .with_breakpoint_download(Arc::new(download_protocol))
        .with_max_chunk_retries(3)
        .build();

    let outcome = client
        .enqueue_and_wait(
            task,
            |record| println!("download progress={:.2}%", record.progress() * 100.0),
        )
        .await?;

    println!("download {} complete", outcome.task_id);
    client.close().await?;
    Ok(())
}
```

## Restart recovery and database notes

`rusty-cat` does not provide a database. If you need recovery after restart:

1. Persist your own record containing object URL, object key, local path, direction, chunk size, and current status.
2. Persist credential references, not raw secrets, whenever possible.
3. On restart, recreate the download protocol and rebuild the same logical download; it can recover from the partial file or a validated concurrent-download sidecar.
4. Treat a direct upload differently: the current public API cannot inject a prior multipart session into a new protocol instance. Clean up the orphaned upload and start a new session.

The direct upload protocol exposes the in-flight OSS multipart `UploadId` via `AliOssDirectUpload::current_upload_id()`. Persisting it lets you abort an orphaned multipart session out of band (so uncommitted parts stop accruing storage cost); it does not make that session injectable into a newly constructed SDK task. For the full restart/crash recovery matrix, see [Resume after a process restart](resume-after-restart.md).

Do not store raw `access_key_secret` values in the same transfer table unless your security policy explicitly allows it. A safer pattern is to store a credential reference and resolve the actual secret from a secret manager when the task is rebuilt.

## Troubleshooting

| Symptom | Common cause | Fix |
|---|---|---|
| `403` from OSS | Invalid AccessKey, wrong region, clock skew, or object URL mismatch. | Verify bucket, region, endpoint, credentials, and local clock. |
| Multipart session conflict | Multiple active multipart uploads for the same object. | Clean stale OSS multipart uploads or use a unique object key. |
| Download cannot determine size | `HEAD` request is blocked or signed incorrectly. | Confirm OSS permissions and direct download signing configuration. |
| Slow callbacks | Database or logging work inside progress callback. | Send records to your own queue and process them outside the SDK callback path. |
