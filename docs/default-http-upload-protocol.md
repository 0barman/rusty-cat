# Default HTTP upload protocol contract

When an upload task has **no** custom `BreakpointUpload` attached (you did not call
`with_breakpoint_upload(...)`), `rusty-cat` uses the built-in `DefaultStyleUpload`
protocol. This page documents the exact wire contract so you can implement a
compatible server, including the optional resume support.

If you use a cloud provider plugin (Aliyun OSS / Azure Blob / presigned), that
plugin defines its own wire format and this page does not apply — see the
provider guides instead.

## Transport

- **Method**: taken from the task — `UploadPounceBuilder::with_method(...)`, default
  `POST`.
- **URL**: taken from the task — `with_url(...)`. The same URL is used for both the
  prepare request and every chunk request.
- **Headers**: the task's base headers (`with_headers(...)`) are sent on every
  request.
- **Body**: `multipart/form-data` for both prepare and chunk requests.

## Request 1: prepare (sent once, before any chunk)

The prepare request announces the file and asks the server where to resume. It
carries **no file bytes**.

| Form field | Type | Value |
|---|---|---|
| `fileMd5` | text | The file signature — see [File signature](#file-signature-filemd5). |
| `fileName` | text | `task.file_name()`. |
| `category` | text | The `DefaultStyleUpload.category` string (empty by default). |
| `totalSize` | text | Total object size in bytes. |

## Request 2..N: chunk (one per chunk)

Each chunk request carries one slice of the file plus the same identifying fields.

| Form field | Type | Value |
|---|---|---|
| `file` | file part | The chunk bytes. Part file name is `upload_chunk_data`, MIME `application/octet-stream`. |
| `fileMd5` | text | Same signature as prepare. |
| `fileName` | text | `task.file_name()`. |
| `category` | text | Same category as prepare. |
| `offset` | text | Start byte offset of this chunk in the full file. |
| `partSize` | text | Byte length of this chunk. |
| `totalSize` | text | Total object size in bytes. |

## Response (for both prepare and chunk)

The server replies with a small JSON object. Both fields are optional:

| JSON field | Type | Meaning |
|---|---|---|
| `nextByte` | integer | The next byte offset the server still needs. The SDK resumes uploading from here. A negative value is treated as `0`. Omitted/empty → start from `0`. |
| `fileId` | string | If present, the server considers the upload **already complete**; the SDK finishes the task immediately without sending more chunks. |

```jsonc
// "I already have the first 8 MiB; send from there":
{ "nextByte": 8388608 }

// "This file is already fully stored":
{ "fileId": "server-object-id-123" }

// "Start from the beginning" (either of these):
{ "nextByte": 0 }
{}
```

An empty response body is also accepted and treated as "start from `0`".

## Resume behavior

On upload, the resume offset is `local_offset.max(nextByte)`. On a fresh process
`local_offset` is `0`, so **resume is driven entirely by the server's `nextByte`**.
To support resume after a process restart or crash, your server must remember, per
`(fileMd5, fileName)`, how many **contiguous** bytes it has already stored and
return that count as `nextByte`. If your server cannot do this, the default
protocol simply re-uploads from the start.

See [Resuming uploads and downloads after a restart](resume-after-restart.md) for
the end-to-end recovery flow.

## File signature (`fileMd5`)

For upload tasks the SDK computes `fileMd5` **automatically**; there is no
`with_client_file_sign(...)` on `UploadPounceBuilder`:

- **File source** (`UploadPounceBuilder::new(...)`): a streaming MD5 of the whole
  file (read in 64 KiB blocks), computed when the task is enqueued.
- **In-memory source** (`UploadPounceBuilder::from_bytes(...)`): the MD5 of the
  byte buffer.
- Zero-byte uploads are rejected during enqueue with `ParameterEmpty`; the wire
  protocol never receives an empty public upload task.

This signature is also the task's **deduplication key** (re-enqueuing the same
content while a task is live yields `DuplicateTaskError`). Because it is a
whole-file hash, it is stable across restarts, which makes it a natural key for
your server to look up resume progress.

## Customizing `category`

`category` lets you tag uploads for server-side routing. It defaults to empty. To
set it, attach a configured `DefaultStyleUpload` explicitly:

```rust,no_run
use std::sync::Arc;
use rusty_cat::api::{DefaultStyleUpload, MeowClient, MeowConfig, UploadPounceBuilder};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = MeowClient::new(MeowConfig::default());

    let task = UploadPounceBuilder::new("clip.mp4", "./clip.mp4", 4 * 1024 * 1024)
        .with_url("https://api.example.com/upload")
        .with_breakpoint_upload(Arc::new(DefaultStyleUpload { category: "video".into() }))
        .build()?;

    let outcome = client.enqueue_and_wait(task, |_record| {}).await?;
    println!("upload {} complete", outcome.task_id);
    client.close().await?;
    Ok(())
}
```

## Implementing a compatible server: checklist

1. Accept `multipart/form-data` on your upload URL for the task's HTTP method.
2. On the prepare request (no `file` field), look up `(fileMd5, fileName)` and
   return `{ "nextByte": <contiguous bytes stored> }`, or `{ "fileId": "..." }`
   if the object is already complete.
3. On each chunk request, write the `file` bytes at `offset`, validate `partSize`,
   and return the updated `nextByte` (or `fileId` once the final byte arrives).
4. Key your storage on `fileMd5` (+ `fileName`) so a resumed upload from a new
   process is recognized.
5. Return a non-2xx status to signal failure; the SDK surfaces it as
   `ResponseStatusError`. Chunk requests retry 408, 429, and 5xx within the task
   budget; other statuses fail immediately. Prepare response statuses are not
   retried by the outer prepare retry loop.
