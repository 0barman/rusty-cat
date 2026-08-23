# Custom upload and download protocols

Use custom protocols when your server does not implement the
[default upload wire contract](default-http-upload-protocol.md) and is not
covered by a bundled provider. The scheduler still owns file I/O, chunking,
retry, progress, pause/resume, cancellation, and concurrency; your implementation
owns request semantics and response parsing.

## Custom upload

Implement `BreakpointUpload` and attach an `Arc` with
`UploadPounceBuilder::with_breakpoint_upload`.

```rust,no_run
use async_trait::async_trait;
use rusty_cat::api::{
    BreakpointUpload, MeowError, UploadChunkCtx, UploadPrepareCtx, UploadResumeInfo,
};

struct MyUpload;

#[async_trait]
impl BreakpointUpload for MyUpload {
    async fn prepare(&self, ctx: UploadPrepareCtx<'_>)
        -> Result<UploadResumeInfo, MeowError>
    {
        // Ask the server for its durable offset/session. Do not blindly trust
        // ctx.local_offset after a process restart.
        let _ = (ctx.client, ctx.task, ctx.local_offset);
        Ok(UploadResumeInfo::default())
    }

    async fn upload_chunk(&self, ctx: UploadChunkCtx<'_>)
        -> Result<UploadResumeInfo, MeowError>
    {
        let next = ctx.offset + ctx.chunk.len() as u64;
        // Send ctx.chunk at ctx.offset, then return the server-confirmed cursor.
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(next),
            provider_upload_id: None,
        })
    }
}
```

`completed_file_id` ends the transfer immediately. Otherwise `next_byte` is
merged with local progress; when the total is reached, the executor invokes
`complete_upload`. Override `complete_upload` when uploaded parts require a
commit/merge call and return an optional application payload. Override
`abort_upload` for remote cleanup on user cancellation. The default completion
and abort hooks are no-ops.

`UploadChunkCtx::chunk` is `bytes::Bytes`; cloning it is cheap. Avoid converting
it to `Vec<u8>` on hot paths.

## Parallel upload safety

`supports_parallel_parts()` defaults to `false`. Return `true` only when part
identity is derived from offset, parts may arrive out of order, re-uploading the
same part is idempotent, and completion is safe after every in-flight part has
joined. Protocol state behind `&self` must be thread-safe. A server with a single
"next expected byte" cursor must remain serial.

## Custom download

Implement `BreakpointDownload` when HEAD and range GET URLs or headers need
customization:

```rust,no_run
use rusty_cat::api::{
    BreakpointDownload, DownloadHeadCtx, DownloadRangeGetCtx, MeowError,
    StandardRangeDownload,
};

struct MyDownload;

impl BreakpointDownload for MyDownload {
    fn merge_head_headers(&self, ctx: DownloadHeadCtx<'_>) -> Result<(), MeowError> {
        let _ = (ctx.task, ctx.base);
        Ok(())
    }

    fn merge_range_get_headers(
        &self,
        ctx: DownloadRangeGetCtx<'_>,
    ) -> Result<(), MeowError> {
        StandardRangeDownload.merge_range_get_headers(ctx)
    }
}
```

Override `head_url`, `range_url`, header merge hooks, and
`total_size_from_head` as required. `total_size_hint` skips HEAD, which can be
necessary for GET-only signed URLs, but it also prevents the built-in concurrent
download path from learning a strong validator during prepare; see
[Concurrent chunk transfer](concurrent-chunk-transfer.md).

`supports_parallel_parts()` also defaults to `false` for downloads. Opt in only
when range requests are independent and safe to complete out of order.

## Lower-level extension point

`TransferTrait` is the lower-level prepare/chunk interface used by the executor.
Prefer the breakpoint traits above unless you are implementing an entirely new
transfer engine contract; they preserve more built-in HTTP validation and file
handling.

## Error and retry contract

Return `MeowError` with a stable `InnerErrorCode`. Chunk execution retries
`HttpError`; it also retries `ResponseStatusError` for HTTP 408, 429, or 5xx when
the SDK transport captured a status. The public custom-protocol API currently
cannot attach status metadata itself, so a custom `ResponseStatusError` without
that metadata remains retryable within the chunk budget; return a non-retryable
domain code for a deterministic client-side rejection. Prepare-stage outer retry
is limited to connection-layer `HttpError`. Make every retryable operation
idempotent.
