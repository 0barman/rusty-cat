# Resuming uploads and downloads after a process restart or crash

This guide is for the very common situation where your application is **killed,
crashes, or is closed** in the middle of a large transfer, and you want the next
run to **continue from where it stopped** instead of starting over.

It is written to be followed step by step. If you only read one section, read
[The mental model](#the-mental-model) and then jump to the direction you need:
[downloads](#1-downloads-resume-from-the-partial-file-easiest) or
[uploads](#2-uploads-resume-needs-a-checkpoint-the-remote-side-knows-about).

A complete, runnable program for the download case lives in
[`examples/resume_after_restart.rs`](../examples/resume_after_restart.rs).

## The mental model

`rusty-cat` has **no built-in database and keeps nothing on disk about your
tasks**. When your process exits, the SDK forgets every task it was running.
There is no "task store" to reload.

So "resume after a restart" is always the same three steps:

1. **Persist** enough information to rebuild each task (you do this *while* the
   transfer runs, using progress callbacks — see
   [Persistence and custom database integration](../README.md#persistence-and-custom-database-integration)).
2. On the next run, **rebuild** the same task with the same builder values.
3. **Enqueue it again** (or import it paused and `resume(...)` it later). The SDK
   then continues from the last checkpoint.

The only thing that changes between upload and download is **where the checkpoint
lives**:

| Transfer | Where the resume checkpoint lives | What you must persist yourself |
|---|---|---|
| Download | The **partial file on disk** (its byte length). | Just the metadata needed to rebuild the task. Progress is implied by the file. |
| Default HTTP upload | Your **server's `nextByte`** response. | Metadata to rebuild the task + a stable file signature your server keys on. |
| Presigned multipart upload | The **list of uploaded parts** you saved, plus the provider `upload_id`. | The uploaded-part records and the `upload_id`. |

The SDK never skips bytes it cannot prove were transferred, so an incomplete or
slightly stale checkpoint is always safe: at worst a little data is re-sent.

---

## 1. Downloads: resume from the partial file (easiest)

Downloads are the simplest case because **the partially downloaded file on disk
is the checkpoint**. During preparation the SDK calls `stat` on the target path,
reads its current length, and issues a `Range` request starting at that offset.
You do not persist progress numbers at all — the file *is* the progress.

> Implementation detail: `download_prepare` ignores any in-memory offset and uses
> the on-disk file length (`tokio::fs::metadata(path).len()`). A missing file
> means "start fresh from byte 0". A file already at/over the remote size means
> "already complete".

### What you must persist

Persist only what you need to rebuild an identical `DownloadPounceBuilder`:

- `file_name`
- `file_path` (this must point at the **same** partial file next run)
- `chunk_size`
- `url`
- any custom headers, `with_client_file_sign(...)`, or custom
  `BreakpointDownload` you used originally

You do **not** persist "bytes downloaded so far". The file length supplies it.

### Step-by-step

```rust,no_run
use rusty_cat::api::{DownloadPounceBuilder, FileTransferRecord, MeowClient, MeowConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = MeowClient::new(MeowConfig::default());

    // 1. Rebuild the SAME task from your database row after a restart.
    //    The key is that `file_path` points at the same partial file the
    //    previous (killed) run was writing to.
    let task = DownloadPounceBuilder::new(
        "report.bin",                 // file_name
        "./downloads/report.bin",     // file_path  <- same path as before
        1024 * 1024,                  // chunk_size  <- same value as before
        "https://example.com/report.bin",
    )
    .build();

    // 2. Enqueue it again. If "./downloads/report.bin" already holds, say,
    //    40 MiB from the previous run, the SDK resumes the download at byte
    //    41,943,040 instead of starting over.
    let _task_id = client
        .try_enqueue(
            task,
            |record: FileTransferRecord| {
                // Persist progress so the *next* restart can also resume.
                println!("progress {:.1}%", record.progress() * 100.0);
            },
            |task_id, _payload| println!("download {task_id} complete"),
        )
        .await?;

    // ... wait for the completion callback (see the full example for one way) ...

    client.close().await?;
    Ok(())
}
```

That is the entire download story. No special "resume API" is required — resume is
the default behavior of `try_enqueue` when a partial file is present.

### Variation: let the user choose which downloads to resume

If you restored ten interrupted downloads but the user should pick which ones
start now, import them **paused** and resume only the chosen subset. Use
[`try_enqueue_paused(...)`](../README.md#importing-tasks-in-the-paused-state-selective-restore).
Importing paused performs **no** network or file I/O until you call `resume(...)`.
See [`examples/restore_import_paused.rs`](../examples/restore_import_paused.rs).

### Download pitfalls

- **Do not move, rename, or truncate the partial file** between runs. The path
  must match and the bytes already on disk are trusted as correct.
- If the remote object **changed** since the last run (different content for the
  same URL), resuming would splice old and new bytes. If your content can change
  under a stable URL, version the URL or download to a fresh path.
- A custom `BreakpointDownload` (OSS/Azure/etc.) must be rebuilt the same way next
  run, because it supplies the headers/signing for the range `GET`.

---

## 2. Uploads: resume needs a checkpoint the remote side knows about

Uploads are different. Your local source file is **whole** — its length tells you
nothing about how much the *server* already accepted. So upload resume cannot
read a local file length; it depends on a checkpoint the **remote side** reports
or that **you** persisted.

> Implementation detail: upload preparation computes
> `next = local_offset.max(server_next_byte)`. On a fresh process `local_offset`
> is `0`, so the resume offset comes entirely from the remote checkpoint.

There are two upload styles, covered next.

### 2a. Default HTTP upload — the server reports `nextByte`

The bundled `DefaultStyleUpload` protocol sends a **prepare** request before
chunks. That prepare is a `multipart/form-data` POST carrying:

| Field | Value |
|---|---|
| `fileMd5` | `task.file_sign()` — your stable per-file signature |
| `fileName` | `task.file_name()` |
| `category` | the `DefaultStyleUpload.category` string (empty by default) |
| `totalSize` | `task.total_size()` |

Your server replies with JSON. The SDK reads two optional fields:

| Response field | Meaning |
|---|---|
| `nextByte` | The next byte offset the server still needs. The SDK resumes here. `0` (or omitted) means "send everything". |
| `fileId`   | If present, the server considers the upload **already complete**; the SDK finishes immediately. |

So resumable upload with the default protocol is a **server contract**: your
backend must remember, per `fileMd5`/`fileName`, how many contiguous bytes it has
stored, and return that as `nextByte`.

```jsonc
// Example prepare response telling the SDK to resume at 8 MiB:
{ "nextByte": 8388608 }

// Example prepare response telling the SDK the file is already there:
{ "fileId": "server-side-object-id-123" }
```

What you persist on the client is only what is needed to rebuild the task:

```rust,no_run
use rusty_cat::api::{MeowClient, MeowConfig, UploadPounceBuilder};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = MeowClient::new(MeowConfig::default());

    // Rebuilt from your database after a restart.
    let task = UploadPounceBuilder::new(
        "video.mp4",            // file_name
        "./uploads/video.mp4",  // file_path (the full local source)
        4 * 1024 * 1024,        // chunk_size
    )
    .with_url("https://api.example.com/upload")
    .build()?;

    // The prepare request hits your server, which returns `nextByte`; the SDK
    // resumes uploading from there.
    let _task_id = client
        .try_enqueue(task, |_r| {}, |id, _payload| println!("upload {id} complete"))
        .await?;

    client.close().await?;
    Ok(())
}
```

If your server cannot report `nextByte`, the default protocol cannot resume and
will re-upload from the start. In that case use a presigned multipart flow
(below) where the client persists the part list itself.

> For the full request/response field contract (prepare **and** chunk requests,
> plus how `fileMd5` is derived), see
> [Default HTTP upload protocol contract](default-http-upload-protocol.md).

### 2b. Presigned multipart upload — persist the part list and `upload_id`

With presigned multipart upload (feature `aliyun-oss-presigned`, `azure-blob-sas`,
or the provider-neutral `presigned`), each part is `PUT` directly to storage. The
protocol records every successfully uploaded part as a `PresignedUploadedPart`:

```rust,ignore
pub struct PresignedUploadedPart {
    pub part_number: u64,             // provider part number
    pub provider_part_id: Option<String>, // e.g. Azure block id
    pub offset: u64,                  // start offset in the file
    pub size: u64,                    // bytes uploaded
    pub etag: Option<String>,         // e.g. OSS/S3 ETag
}
```

This struct already derives `serde::Serialize`/`Deserialize`, so you can store it
straight to JSON. **To resume after a restart you persist two things:**

1. The provider **`upload_id`** (the multipart session id). It is *not* a secret
   and is safe to store. It is also surfaced at runtime via
   `UploadResumeInfo.provider_upload_id`.
2. Every **`PresignedUploadedPart`** as it completes.

#### Capturing parts while the upload runs

`PresignedMultipartUpload` is `Clone` and stores its parts behind a shared handle,
so keep a clone and read `uploaded_parts().await` to persist progress. Read it
from a background tick or from your progress-callback's worker — never block the
callback thread on a slow database write.

```rust,no_run
use std::sync::Arc;
use rusty_cat::api::{
    MeowClient, MeowConfig, PresignedMultipartUpload, PresignedMultipartUploadPlan,
    PresignedUploadPart, UploadPounceBuilder,
};

# async fn build_plan() -> PresignedMultipartUploadPlan { unreachable!() }
# async fn save_parts(_: Vec<rusty_cat::api::PresignedUploadedPart>) {}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = MeowClient::new(MeowConfig::default());

    // Your backend builds the plan (and owns/persists the upload_id).
    let plan: PresignedMultipartUploadPlan = build_plan().await;

    let upload = PresignedMultipartUpload::new(plan);
    let upload_handle = upload.clone(); // shares the same parts list

    let task = UploadPounceBuilder::new("big.bin", "./uploads/big.bin", 5 * 1024 * 1024)
        .with_url("https://logical-target")
        .with_breakpoint_upload(Arc::new(upload))
        .build()?;

    let _id = client.try_enqueue(task, |_r| {}, |_id, _p| {}).await?;

    // Persist the part list periodically so a crash mid-upload is recoverable.
    tokio::spawn(async move {
        loop {
            let parts = upload_handle.uploaded_parts().await;
            save_parts(parts).await; // upsert into your own DB
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    });

    client.close().await?;
    Ok(())
}
```

#### Resuming on the next run

On restart, ask your backend for a **fresh plan** (presigned URLs expire, so you
usually cannot reuse the old ones), making sure it carries the **same**
`upload_id`. Then re-inject the parts you saved with `with_resumed_parts(...)`:

```rust,no_run
use std::sync::Arc;
use rusty_cat::api::{
    MeowClient, MeowConfig, PresignedMultipartUpload, PresignedMultipartUploadPlan,
    PresignedUploadedPart, UploadPounceBuilder,
};

# async fn fetch_fresh_plan_with_same_upload_id() -> PresignedMultipartUploadPlan { unreachable!() }
# fn load_saved_parts() -> Vec<PresignedUploadedPart> { Vec::new() }
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = MeowClient::new(MeowConfig::default());

    // 1. Fresh presigned URLs, same provider upload_id as last run.
    let plan: PresignedMultipartUploadPlan = fetch_fresh_plan_with_same_upload_id().await;

    // 2. The parts you persisted as they completed last run.
    let saved_parts: Vec<PresignedUploadedPart> = load_saved_parts();

    // 3. Re-inject them. `prepare` will resume past the already-uploaded,
    //    contiguous, plan-matching prefix and re-send anything after the first
    //    gap. `complete_upload` includes these parts in the final part list.
    let upload = PresignedMultipartUpload::new(plan).with_resumed_parts(saved_parts);

    let task = UploadPounceBuilder::new("big.bin", "./uploads/big.bin", 5 * 1024 * 1024)
        .with_url("https://logical-target")
        .with_breakpoint_upload(Arc::new(upload))
        .build()?;

    let _id = client.try_enqueue(task, |_r| {}, |_id, _p| {}).await?;
    client.close().await?;
    Ok(())
}
```

How the resume offset is computed (so you can trust it): `with_resumed_parts`
de-duplicates by `offset`, then `prepare` walks the parts **sorted by offset** and
counts only the longest run that is **contiguous from byte 0** *and* matches a
plan part of the same size. It stops at the first gap, overlap, or size mismatch.
The result is the resume offset. Consequences:

- A part list missing the very first chunk resumes from `0` (the prefix is empty).
- Out-of-order or partially-saved lists are safe — only the verified prefix is
  skipped; everything after the first gap is re-uploaded.
- `complete_upload` sorts and de-duplicates the parts by `part_number`
  unconditionally, so a resumed completion never submits an out-of-order or
  duplicated part list.

#### Cleaning up orphaned sessions

If a user abandons a resumable upload for good, the provider may keep uncommitted
parts that still cost storage. Persisting the `upload_id` lets you call the
provider's "abort multipart upload" out of band later. (Canceling a live task via
`cancel(...)` already triggers the protocol's `abort_upload`.)

### Upload pitfalls

- `chunk_size`, `total_size`, `file_path`, and the logical target must match the
  original task. The presigned `prepare` rejects a plan whose `total_size` or
  `chunk_size` disagrees with the task.
- **Never persist cloud secrets.** Persist the `upload_id` and part records, and
  regenerate short-lived presigned/SAS URLs from your backend on restart.
- For long uploads, attach a `PresignedUploadUrlRefresher` so part URLs are
  refreshed before they expire mid-transfer.

---

## 3. What to persist — quick reference

| Scenario | Persist these fields | Resume call |
|---|---|---|
| Download | `file_name`, `file_path`, `chunk_size`, `url`, headers/sign/custom protocol | rebuild + `try_enqueue` (or `try_enqueue_paused` then `resume`) |
| Default HTTP upload | `file_name`, `file_path`, `chunk_size`, `url`, `method`, headers, stable `fileMd5` signature | rebuild + `try_enqueue`; server returns `nextByte` |
| Presigned multipart upload | provider `upload_id` + every `PresignedUploadedPart` (`part_number`, `provider_part_id`, `offset`, `size`, `etag`) | fresh plan (same `upload_id`) + `with_resumed_parts(...)` + `try_enqueue` |

---

## 4. Runnable examples

| Example | Demonstrates |
|---|---|
| [`examples/resume_after_restart.rs`](../examples/resume_after_restart.rs) | A download that continues from a partial file left by a previous (killed) run. |
| [`examples/restore_import_paused.rs`](../examples/restore_import_paused.rs) | Importing many tasks **paused** on restart and resuming only the user-selected subset. |

## 5. Related reading

- [Persistence and custom database integration](../README.md#persistence-and-custom-database-integration)
- [Importing tasks in the paused state (selective restore)](../README.md#importing-tasks-in-the-paused-state-selective-restore)
- [Aliyun OSS presigned upload/download](aliyun-oss-presigned.md)
- [Azure Blob SAS upload/download](azure-blob-sas.md)
