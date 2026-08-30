# Aliyun OSS presigned upload/download guide

This guide explains how to use `rusty-cat` with Aliyun OSS presigned URLs. Presigned mode means a trusted backend keeps the Aliyun AccessKey credentials and creates short-lived URLs. The client performs the HTTP upload/download requests authorized by those URLs; optional completion or abort callbacks can call your backend.

This is the recommended integration model for untrusted clients. The client does not need to know the AccessKey ID or AccessKey secret, and the backend can restrict each URL to a specific object, method, part number, and expiration time.

Runnable download scenario:
[`test-app/src/download/aliyun_presigned.rs`](../../test-app/src/download/aliyun_presigned.rs)

## Security model

The SDK does not generate OSS Signature Version 4 signatures in this module and does not receive or persist Aliyun credentials. Your backend should:

1. Authenticate the user.
2. Decide the bucket, object key, file size, chunk size, and permissions.
3. Initiate OSS multipart upload when needed.
4. Generate short-lived presigned `UploadPart` URLs for each part.
5. Generate a short-lived range-download URL, or a URL that supports `Range` requests.
6. Optionally expose a completion endpoint that verifies parts and completes the OSS multipart upload.

The client receives only temporary URLs and metadata. `rusty-cat` does not persist those URLs; if you need restart recovery, persist your own transfer record and request fresh URLs from your backend when needed.

## Enable the feature

```toml
[dependencies]
rusty-cat = { version = "0.3.6", features = ["aliyun-oss-presigned"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

Run the configured signed-download scenario from the repository root:

```text
cargo run --manifest-path test-app/Cargo.toml -- aliyun-presigned
```

The bundled URL is temporary. If it has expired, provide a fresh signed GET URL
through `RC_ALIYUN_DOWNLOAD_URL`; optionally set
`RC_ALIYUN_DOWNLOAD_EXPIRES_AT` to its Unix expiry time and
`RC_EXPECTED_SIZE` to the expected object size. The scenario covers presigned
range download only. Presigned multipart upload still requires the backend
metadata described below: upload ID, part URLs, part boundaries, required
headers, and completion semantics.

## Upload flow

The upload side uses the provider-neutral `PresignedMultipartUpload` abstraction plus Aliyun helper functions from `rusty_cat::aliyun_oss_presigned`.

High-level process:

1. Your backend creates one presigned URL per upload part.
2. Your client converts those URLs into part descriptors with `aliyun::upload_part(part_number, offset, size, url)`.
3. Create a `PresignedMultipartUploadPlan` with total size, chunk size, and part descriptors.
4. Create `PresignedMultipartUpload::new(plan)`.
5. Attach it to `UploadPounceBuilder` with `with_breakpoint_upload(...)`.
6. Submit with `enqueue_and_wait(...)` (or manage terminal callbacks yourself).
7. Complete the multipart upload either through a configured completion request or through your backend after all parts have been verified.

The `offset` and `size` values in every part descriptor must match the local chunk plan. If the backend and client disagree about chunk boundaries, the SDK will send bytes to the wrong part URL or fail plan validation.

```rust,no_run
use std::sync::Arc;

use rusty_cat::aliyun_oss_presigned as aliyun;
use rusty_cat::api::{
    MeowClient, MeowConfig, PresignedMultipartUpload, PresignedMultipartUploadPlan,
    UploadPounceBuilder,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let chunk_size = 1024 * 1024;
    let total_size = 5 * chunk_size;

    // In a real application, these URLs come from your backend.
    let urls = vec![
        "https://oss.example.com/object?partNumber=1&uploadId=...&signature=...",
        "https://oss.example.com/object?partNumber=2&uploadId=...&signature=...",
        "https://oss.example.com/object?partNumber=3&uploadId=...&signature=...",
        "https://oss.example.com/object?partNumber=4&uploadId=...&signature=...",
        "https://oss.example.com/object?partNumber=5&uploadId=...&signature=...",
    ];

    let parts = urls
        .iter()
        .enumerate()
        .map(|(i, url)| aliyun::upload_part((i + 1) as u64, i as u64 * chunk_size, chunk_size, *url))
        .collect::<Vec<_>>();

    let upload_protocol = PresignedMultipartUpload::new(
        PresignedMultipartUploadPlan::new(total_size, chunk_size, parts)
            .with_upload_id("backend-upload-id"),
    );

    let client = MeowClient::new(MeowConfig::default());
    let task = UploadPounceBuilder::new("aliyun-presigned.bin", "./aliyun-presigned.bin", chunk_size)
        .with_url(urls[0])
        .with_breakpoint_upload(Arc::new(upload_protocol))
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

### URL expiration and refresh

`PresignedMultipartUploadPlan` supports `with_refresh_before_secs(...)`. When part metadata includes expiration information, the SDK can refresh before the URL expires if you provide a refresher implementation. If your URLs are short-lived and no refresher is configured, make them long enough for the expected upload duration or request a new plan from your backend and re-enqueue.

For production systems, prefer short-lived URLs plus a backend refresh endpoint. Long-lived presigned URLs are easier to test but increase the impact of URL leakage.

### Resuming after a restart

`rusty-cat` keeps no state across process restarts, so resuming a presigned multipart upload after a kill/crash means persisting two things yourself:

1. The provider **`upload_id`** your backend created (also surfaced at runtime via `UploadResumeInfo::provider_upload_id`). It is not a secret.
2. Every `PresignedUploadedPart` as it completes — read them from a clone of the `PresignedMultipartUpload` via `uploaded_parts().await`.

On restart, request a **fresh** plan from your backend (presigned URLs expire) carrying the **same** `upload_id`, reconcile it with provider state, then re-inject confirmed parts with `PresignedMultipartUpload::new(plan).with_resumed_parts(saved_parts)` before enqueue. The SDK resumes past the verified contiguous prefix and re-sends the rest. Persisting the `upload_id` also lets you abort an orphaned multipart session out of band if the user abandons the upload.

See [Presigned multipart lifecycle](presigned-lifecycle.md) and [Resume after a process restart](resume-after-restart.md).

## Download flow

The download side uses `PresignedRangeDownload` and `PresignedRangeDownloadPlan`. The plan tells the SDK which URL to use for range requests and whether the remote object size is already known.

High-level process:

1. Your backend creates a presigned URL that permits `HEAD` and/or range `GET`, or returns the known object size so the client can skip `HEAD`.
2. Create `PresignedRangeDownloadPlan::new(download_url)`.
3. If the backend already knows object size, call `with_total_size(size)`.
4. Attach the protocol with `with_breakpoint_download(...)`.
5. Submit with `enqueue_and_wait(...)` and await the terminal result.
6. If the URL expires during a long download, request a refreshed plan from your backend and retry or re-enqueue according to your application policy.

`with_total_size` skips HEAD. It therefore cannot authenticate or reuse an old
`.rcdl` checkpoint; existing local bytes are fetched again from byte zero. For
cross-process resume, provide a HEAD-capable metadata URL that returns a strong
ETag. Whether serial or parallel, a run needing multiple ranges must receive one
stable strong ETag on every 206 response; a one-range run may omit it only when
HEAD did not prepare a validator.

```rust,no_run
use std::sync::Arc;

use rusty_cat::api::{
    DownloadPounceBuilder, MeowClient, MeowConfig, PresignedRangeDownload,
    PresignedRangeDownloadPlan,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let download_url = "https://oss.example.com/object?signature=...";
    let total_size = 5 * 1024 * 1024;

    let download_protocol = PresignedRangeDownload::new(
        PresignedRangeDownloadPlan::new(download_url).with_total_size(total_size),
    );

    let client = MeowClient::new(MeowConfig::default());
    let task = DownloadPounceBuilder::new(
        "aliyun-presigned.bin",
        "./downloads/aliyun-presigned.bin",
        1024 * 1024,
        download_url,
    )
    .with_breakpoint_download(Arc::new(download_protocol))
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

## Backend contract checklist

For upload, return total file size, chunk size, upload/session ID, one URL per part, each part number/offset/size, expiration timestamp if relevant, and optional completion/abort endpoint metadata. The backend should also remember enough server-side state to complete or abort the OSS multipart session safely.

For download, return range URL, optional dedicated `HEAD` URL, optional object size, expiration timestamp if relevant, and extra headers if required. Supplying the object size is useful when the presigned URL does not allow `HEAD`.

## Troubleshooting

| Symptom | Common cause | Fix |
|---|---|---|
| `403` during upload | URL expired or generated for a different method/part number. | Regenerate URLs and verify part metadata. |
| Part range error | Backend chunk plan does not match local `chunk_size` or total size. | Ensure offsets and sizes exactly cover the file. |
| Download fails at prepare | Presigned URL does not permit `HEAD`. | Provide `with_total_size(...)` or a dedicated `with_head_url(...)`. |
| Upload completes locally but object is not visible | Backend did not complete multipart upload. | Add a completion request or complete multipart upload on your backend. |
