# Concurrent chunked transfer guide (single-file parallel parts)

This guide explains how to upload or download **one file** as **several chunks at
the same time** with `rusty-cat`, using the `with_max_parts_in_flight(n)` knob. It
is written for beginners: every rule is spelled out, every recipe is copy-pasteable,
and every surprising behavior has a "why".

`with_max_parts_in_flight(n)` lets up to `n` chunks of a *single* file be in flight
at once — `n` parallel `PUT`/`Put Block`/`UploadPart` requests for an upload, or `n`
parallel HTTP `Range` `GET` requests for a download. The default is `1`, which means
**strict serial**: one chunk finishes before the next starts, byte-for-byte identical
to the classic single-stream path. You opt into concurrency by passing a value
greater than `1`.

**When should you use it?** Reach for `n > 1` when you are moving a **large** file
(tens of MiB or more) over a **fast link that a single stream does not saturate** —
for example a multi-gigabit server pulling a big object from object storage. A higher
`n` overlaps request latency and keeps the pipe full. **Leave it at the default `1`**
for small files, for links that a single stream already saturates, or when you want to
minimize memory (see [Peak memory and choosing `n`](#peak-memory-and-choosing-n)). Any
speedup is workload- and link-dependent; the SDK ships **no measured intra-file
benchmark numbers**, so treat parallelism as a tool to try and measure, not a
guaranteed multiplier.

> **Example source.** None of the shipped `examples/*.rs` set
> `with_max_parts_in_flight` — they all run serial. The runnable, source-of-truth
> references for this feature are the integration tests:
> [`tests/intra_file_parallel_test.rs`](../tests/intra_file_parallel_test.rs),
> [`tests/aliyun_oss_direct_parallel_test.rs`](../tests/aliyun_oss_direct_parallel_test.rs),
> [`tests/azure_direct_parallel_test.rs`](../tests/azure_direct_parallel_test.rs),
> [`tests/presigned_parallel_test.rs`](../tests/presigned_parallel_test.rs), and
> [`tests/concurrent_download_test.rs`](../tests/concurrent_download_test.rs).

---

## Table of contents

1. [Two kinds of concurrency (do not confuse them)](#two-kinds-of-concurrency-do-not-confuse-them)
2. [The three preconditions (the concurrency gate)](#the-three-preconditions-the-concurrency-gate)
3. [Which protocols are parallel-safe](#which-protocols-are-parallel-safe)
4. [Enable the feature](#enable-the-feature)
5. [Concurrent upload recipes](#concurrent-upload-recipes)
6. [Concurrent download recipe](#concurrent-download-recipe)
7. [Peak memory and choosing `n`](#peak-memory-and-choosing-n)
8. [Progress, pause, and cancel under concurrency](#progress-pause-and-cancel-under-concurrency)
9. [Resuming a concurrent download (the `.rcdl` sidecar)](#resuming-a-concurrent-download-the-rcdl-sidecar)
10. [Resuming a concurrent upload](#resuming-a-concurrent-upload)
11. [Retry, backoff, and the stable-URL rule](#retry-backoff-and-the-stable-url-rule)
12. [Troubleshooting](#troubleshooting)
13. [Related reading](#related-reading)

---

## Two kinds of concurrency (do not confuse them)

`rusty-cat` has **two independent** concurrency knobs. Mixing them up is the single
most common beginner mistake, so start here.

| Knob | Scope | Default | What it caps |
|---|---|---:|---|
| `UploadPounceBuilder::with_max_parts_in_flight(n)` / `DownloadPounceBuilder::with_max_parts_in_flight(n)` | Parts **within one file** | `1` | How many chunks of a **single** file transfer at once |
| `MeowConfig::builder().max_upload_concurrency(k)` | Whole client | `2` | How many **upload files (groups)** run at once |
| `MeowConfig::builder().max_download_concurrency(k)` | Whole client | `2` | How many **download files (groups)** run at once |

- **`with_max_parts_in_flight`** is a *per-task* setting on the task builder. It
  controls parallelism **inside one file**.
- **`max_upload_concurrency` / `max_download_concurrency`** are *client-wide* settings
  on `MeowConfig`. They control how many *separate files* run at the same time, and
  they are counted **independently per direction** (uploads and downloads do not share
  the budget). Each must be `>= 1`; the builder rejects `0`.

The two **multiply**. If you run `max_download_concurrency(3)` and each of those three
downloads uses `with_max_parts_in_flight(4)`, the client can have up to `3 × 4 = 12`
range requests in flight at once. There is **no global cap** that limits the total
number of in-flight parts across all files — you are responsible for keeping the
product reasonable.

> **Monitoring note.** `client.snapshot()` reports **groups** (whole files) queued and
> active — it does **not** expose intra-file parts. A file downloading with 4 parallel
> parts still counts as one active group.

---

## The three preconditions (the concurrency gate)

Setting `with_max_parts_in_flight(8)` does **not** guarantee concurrency. The executor
runs the parallel path **only when all three of these hold at once**:

1. **`max_parts_in_flight() > 1`** — strictly greater than one. (`0` is normalized to
   `1`; there is no upper clamp, so a huge `n` really does scale memory — see below.)
2. **The total size is known up front** (`known_total > 0`). For **downloads** this is
   the condition that actually matters: supply it explicitly with `with_total_size(...)`,
   or let a `HEAD` probe discover it (the server must return a positive `Content-Length`).
   For **uploads** the size comes from the file/`bytes` source automatically.
3. **The transfer protocol is parallel-safe** — its `supports_parallel_parts()` returns
   `true` (next section).

If condition 1 (`max_parts_in_flight` is `1`) or condition 3 (the protocol is not
parallel-safe) is missing, the transfer silently falls back to the **byte-for-byte
identical serial loop**: **no error, no warning** — it still succeeds, just one chunk at
a time. Condition 2 behaves differently for a download using the default range
downloader: if the total cannot be resolved (you passed no `with_total_size` and the
`HEAD` returns no positive `Content-Length`), the task **fails** during preparation
rather than running serial. So if you enabled concurrency and it "did nothing", check
these three conditions first.

---

## Which protocols are parallel-safe

Concurrency requires an **out-of-order-safe** protocol: one where chunk *N* can be
transferred without depending on chunk *N-1* having finished, and where the final
"commit" step orders parts correctly regardless of the order they arrived in. A
protocol advertises this by returning `true` from `supports_parallel_parts()`. The
default for both the upload and download traits is `false`, so a protocol is serial
unless it explicitly opts in.

### Upload protocols

| Upload protocol | Feature flag | Parallel-safe? | Why |
|---|---|:---:|---|
| `AliOssDirectUpload` | `aliyun-oss-direct` | ✅ Yes | OSS Multipart Upload; part number is derived from the byte offset, re-uploading a part is idempotent, and `Complete` merges parts in ascending order. |
| `AliOssPresignedMultipartUpload` (= `PresignedMultipartUpload`) | `aliyun-oss-presigned` | ✅ Yes | Presigned multipart; parts are accounted by offset and the completion manifest is re-sorted by part number. |
| `AzureBlobDirectUpload` | `azure-blob-direct` | ✅ Yes | Block Blob; block id is a pure function of the chunk index, and the final `Put Block List` fixes commit order — never arrival order. |
| `AzureBlobSasMultipartUpload` (= `PresignedMultipartUpload`) | `azure-blob-sas` | ✅ Yes | Same Block Blob model over SAS URLs. |
| Built-in default HTTP upload (`DefaultStyleUpload`) | *(none)* | ❌ No | Trusts the server's single `nextByte` cursor, so parts must be strictly sequential. |
| Your own `BreakpointUpload` | *(custom)* | ❌ No (default) | Inherits `false` unless you override it — see the contract below. |

### Download protocols

| Download protocol | Feature flag | Parallel-safe? | Why |
|---|---|:---:|---|
| `StandardRangeDownload` (the built-in default) | *(none)* | ✅ Yes | Plain RFC 7233 `Range` requests; each part is fully independent and written at its absolute offset. |
| `AliOssDirectDownload` | `aliyun-oss-direct` | ❌ No | Inherits the serial default. |
| `AzureBlobDirectDownload` | `azure-blob-direct` | ❌ No | Inherits the serial default. |
| `AliOssPresignedRangeDownload` / `AzureBlobSasRangeDownload` (= `PresignedRangeDownload`) | `aliyun-oss-presigned` / `azure-blob-sas` | ❌ No | Inherits the serial default. |
| Your own `BreakpointDownload` | *(custom)* | ❌ No (default) | Inherits `false` unless you override it. |

### The asymmetry you must remember

> **Uploads and downloads are not symmetric.**
>
> - For **uploads**, every OSS/Azure provider protocol is parallel-safe. Attach the
>   provider upload protocol and set `n > 1`.
> - For **downloads**, the provider-specific protocols (`AliOssDirectDownload`,
>   `AzureBlobSasRangeDownload`, …) are **serial-only**. The **only** parallel-safe
>   download protocol is the built-in **`StandardRangeDownload`**. To download an
>   OSS/Azure object concurrently you feed a **signed GET URL** (an Aliyun presigned
>   URL, an Azure SAS URL, or a public URL) straight to the default range downloader —
>   see [the download recipe](#concurrent-download-recipe). Attaching a provider
>   download protocol silently forces the serial path.

### Writing your own parallel-safe protocol (advanced)

If you implement a custom `BreakpointUpload`/`BreakpointDownload`, it stays serial
until you override `supports_parallel_parts()` to return `true` — and you should only
do that when your protocol truly satisfies the contract:

- Each part's identity (part number / block id / byte range) is **derived from the
  offset**, not from a server cursor or the completion order of earlier parts.
- Re-transferring the same part is **idempotent**.
- The final commit orders parts **explicitly** (not by arrival order).
- `&self` is shared across all concurrent part calls, so any per-part bookkeeping must
  be **interior-mutable and thread-safe** (e.g. `Mutex`/`BTreeSet`).

Returning `true` without these properties can corrupt the object. When in doubt, leave
it `false`.

---

## Enable the feature

Concurrency itself needs no special flag — `with_max_parts_in_flight` is always
available. You only need a feature flag to pull in the **provider protocol** you attach.

```toml
[dependencies]
# Pick the one(s) you use:
rusty-cat = { version = "0.2.4", features = ["aliyun-oss-direct"] }
# rusty-cat = { version = "0.2.4", features = ["aliyun-oss-presigned"] }
# rusty-cat = { version = "0.2.4", features = ["azure-blob-direct"] }
# rusty-cat = { version = "0.2.4", features = ["azure-blob-sas"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

The **concurrent download** path uses the built-in `StandardRangeDownload`, so it needs
**no provider feature flag** — the base crate is enough for a signed or public GET URL.

---

## Concurrent upload recipes

### The shared lifecycle (write this once)

Every recipe below builds a `PounceTask` and submits it. The simplest way to submit a
task and wait for it to finish is `client.enqueue_and_wait(task, progress_cb)`: it
enqueues the task, calls your progress callback on every update, and resolves when the
task reaches a terminal state — returning a `TaskOutcome` on success, or a `MeowError`
if the task fails or is canceled (so `?` handles the error paths for you). The provider
recipes only change how the **task** is built.

```rust,no_run
use rusty_cat::api::{FileTransferRecord, MeowClient, MeowError, PounceTask, TaskOutcome};

/// Enqueue one task and await its terminal state.
async fn run_to_completion(
    client: &MeowClient,
    task: PounceTask,
) -> Result<TaskOutcome, MeowError> {
    client
        .enqueue_and_wait(task, |record: FileTransferRecord| {
            // Progress is the contiguous-prefix watermark, a ratio in 0.0..=1.0.
            println!("{:.1}% {:?}", record.progress() * 100.0, record.status());
        })
        .await
}
```

> `enqueue_and_wait` is `async` and awaits internally, so it never blocks a runtime
> thread. If you need a hard deadline, wrap the call in
> `tokio::time::timeout(duration, client.enqueue_and_wait(...))`. If you would rather
> drive the lower-level `try_enqueue(task, progress_cb, complete_cb)` yourself (for
> example to fan progress out to many listeners), see the complete example in the
> [crate README](../README.md).

> Put the protocol object in an `Arc` (`Arc::new(protocol)`) before
> `with_breakpoint_upload(...)`: the executor may move part work across async tasks, and
> the same `&self` is shared by every concurrent part.

### Aliyun OSS — direct (AccessKey)

Use this when the client process is trusted to hold OSS AccessKey credentials.

```rust,no_run
use std::sync::Arc;

use rusty_cat::aliyun_oss_direct::AliOssDirectUpload;
use rusty_cat::api::{MeowClient, MeowConfig, UploadPounceBuilder};

// `run_to_completion` is the helper defined above.
async fn upload_oss_direct() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = MeowClient::new(MeowConfig::default());

    let protocol = AliOssDirectUpload::new("my-bucket", "AK_ID", "AK_SECRET", "cn-beijing");

    let task = UploadPounceBuilder::new(
        "big-object.bin",
        "./big-object.bin",
        1024 * 1024, // 1 MiB chunk
    )
    .with_url("https://my-bucket.oss-cn-beijing.aliyuncs.com/test/big-object.bin")
    .with_breakpoint_upload(Arc::new(protocol))
    .with_max_parts_in_flight(4) // up to 4 Upload Part requests at once
    .build()?;

    run_to_completion(&client, task).await?;
    client.close().await?;
    Ok(())
}
```

**How it runs internally:** the SDK calls `InitiateMultipartUpload` once, then fires up
to four `UploadPart` requests concurrently (each part number is `offset / chunk + 1`),
and finally issues one `CompleteMultipartUpload` with `x-oss-complete-all: yes` so OSS
merges every uploaded part in order. OSS allows at most **10,000 parts**, validated up
front. See [Aliyun OSS direct guide](aliyun-oss-direct.md).

### Aliyun OSS — presigned

Use this when your **backend** holds the credentials and hands the client short-lived
presigned part URLs. Build one `PresignedUploadPart` per chunk offset.

```rust,no_run
use std::sync::Arc;

use rusty_cat::aliyun_oss_presigned as aliyun;
use rusty_cat::api::{
    MeowClient, MeowConfig, PresignedMultipartUpload, PresignedMultipartUploadPlan,
    UploadPounceBuilder,
};

// `part_urls` are the presigned PUT URLs your backend hands the client (one per part).
async fn upload_oss_presigned(
    part_urls: [&str; 5],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let one_mb: u64 = 1024 * 1024;
    let total: u64 = 5 * one_mb;

    // One presigned PUT URL per part offset.
    let parts = part_urls
        .iter()
        .enumerate()
        .map(|(i, url)| aliyun::upload_part((i + 1) as u64, i as u64 * one_mb, one_mb, *url))
        .collect::<Vec<_>>();

    let plan = PresignedMultipartUploadPlan::new(total, one_mb, parts);
    let protocol = PresignedMultipartUpload::new(plan);

    let client = MeowClient::new(MeowConfig::default());
    let task = UploadPounceBuilder::new("presigned.bin", "./presigned.bin", one_mb)
        .with_url(part_urls[0]) // any of the part URLs; the plan carries them all
        .with_breakpoint_upload(Arc::new(protocol))
        .with_max_parts_in_flight(4)
        .build()?;

    run_to_completion(&client, task).await?;
    client.close().await?;
    Ok(())
}
```

The plan's `total` and `chunk` **must equal** the task's total and chunk size; the
protocol checks this in `prepare()` before any byte is sent. Separately,
`PresignedMultipartUploadPlan::validate()` rejects zero-length parts, duplicate offsets,
and parts that extend past the total. Parts do **not** all have to equal the chunk
length — the last part is shorter whenever the file size is not an exact multiple of the
chunk. Completion is commonly left to your backend (merge the parts out of band); read
`protocol.uploaded_parts()` afterward if you persist them. See
[Aliyun OSS presigned guide](aliyun-oss-presigned.md).

### Azure Blob — direct (Shared Key)

```rust,no_run
use std::sync::Arc;

use rusty_cat::api::{MeowClient, MeowConfig, UploadPounceBuilder};
use rusty_cat::azure_blob_direct::AzureBlobDirectUpload;

async fn upload_azure_direct() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = MeowClient::new(MeowConfig::default());

    let protocol = AzureBlobDirectUpload::new("myaccount", "BASE64_ACCOUNT_KEY");

    let task = UploadPounceBuilder::new("blob.bin", "./blob.bin", 1024 * 1024)
        .with_url("https://myaccount.blob.core.windows.net/mycontainer/blob.bin")
        .with_breakpoint_upload(Arc::new(protocol))
        .with_max_parts_in_flight(4)
        .build()?;

    run_to_completion(&client, task).await?;
    client.close().await?;
    Ok(())
}
```

**How it runs internally:** the SDK issues up to four `Put Block` requests concurrently
(each block id is the base64 of the chunk index), then one `Put Block List` in strict
index order to commit. Azure allows at most **50,000 blocks** and **4,000 MiB per block**.
Azure `Put Block` returns no `ETag`, so commit order comes from the block-id list, not
per-part ETags. See [Azure Blob direct guide](azure-blob-direct.md).

### Azure Blob — SAS

Use this when your backend hands the client a short-lived blob **SAS URL**. Unlike OSS
presigned, the Azure SAS upload **requires** a completion request so the SDK can issue
`Put Block List` once at the end.

```rust,no_run
use std::sync::Arc;

use rusty_cat::api::{
    MeowClient, MeowConfig, PresignedMultipartUpload, PresignedMultipartUploadPlan,
    UploadPounceBuilder,
};
use rusty_cat::azure_blob_sas as azure;

// `blob_sas_url` is the short-lived blob SAS URL from your backend.
async fn upload_azure_sas(
    blob_sas_url: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let one_mb: u64 = 1024 * 1024;
    let total: u64 = 5 * one_mb;

    // One Put Block per chunk index, plus the ordered block-id list for the commit.
    let parts = (0..5)
        .map(|i| azure::put_block_from_blob_url(blob_sas_url, i, i as u64 * one_mb, one_mb))
        .collect::<Result<Vec<_>, _>>()?;
    let block_ids = (0..5).map(azure::block_id_by_index).collect::<Vec<_>>();
    let block_id_refs = block_ids.iter().map(String::as_str).collect::<Vec<_>>();
    let complete_request = azure::put_block_list_request(blob_sas_url, block_id_refs)?;

    let plan = PresignedMultipartUploadPlan::new(total, one_mb, parts)
        .with_complete_request(complete_request); // required for Azure SAS
    let protocol = PresignedMultipartUpload::new(plan);

    let client = MeowClient::new(MeowConfig::default());
    let task = UploadPounceBuilder::new("azure-sas.bin", "./azure-sas.bin", one_mb)
        .with_url(blob_sas_url)
        .with_breakpoint_upload(Arc::new(protocol))
        .with_max_parts_in_flight(4)
        .build()?;

    run_to_completion(&client, task).await?;
    client.close().await?;
    Ok(())
}
```

See [Azure Blob SAS guide](azure-blob-sas.md).

### What the SDK guarantees on the parallel upload path

On the concurrent path, **no individual part finalizes the upload**. Each part just
uploads its bytes; the `Complete` / `Put Block List` step is **hoisted to the scheduler
and runs exactly once**, only after every part has landed and forms a contiguous prefix.
That is what makes out-of-order parts safe: a part that finishes first can never trigger
a premature completion.

---

## Concurrent download recipe

Because the provider download protocols are serial-only, a concurrent download uses the
**default `StandardRangeDownload`** — you simply **do not** call
`with_breakpoint_download(...)`, and you give the builder a **signed GET URL** whose
authentication rides in the query string (an Aliyun presigned URL, an Azure SAS URL, or
a public URL).

```rust,no_run
use rusty_cat::api::{DownloadPounceBuilder, MeowClient, MeowConfig};

// `signed_get_url` is an Aliyun presigned / Azure SAS / public GET URL.
async fn download_concurrent(
    signed_get_url: &str,
    total_size: u64,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = MeowClient::new(MeowConfig::default());

    let task = DownloadPounceBuilder::new(
        "movie.mp4",
        "./downloads/movie.mp4",
        2 * 1024 * 1024, // 2 MiB range chunks
        signed_get_url,
    )
    .with_total_size(total_size) // condition 2: makes the total known, skips HEAD
    .with_max_parts_in_flight(4) // condition 1: up to 4 Range GETs at once
    // Do NOT call .with_breakpoint_download(...) — keep the default StandardRangeDownload.
    .build();

    run_to_completion(&client, task).await?;
    client.close().await?;
    Ok(())
}
```

Key rules for concurrent download:

- **Give it the total size.** `with_total_size(total)` satisfies precondition 2 and
  skips the `HEAD` probe (ideal for presigned/SAS URLs that may not answer `HEAD`). If
  you omit it, the SDK sends a `HEAD`, which must return a **positive `Content-Length`**.
  If the `HEAD` errors or returns no positive `Content-Length`, the download **fails**
  during preparation (`MissingOrInvalidContentLengthFromHead`) — it does not silently
  fall back to serial. Get the size from your metadata/check endpoint or a prior
  `Content-Range` probe.
- **The server must honor `Range`.** Every part is a `Range` `GET` that **must** answer
  `206 Partial Content`. A `200 OK` (server ignored the range and sent the whole body)
  is a hard `InvalidRange` error, as is a `Content-Range` whose start or total does not
  match what was requested.
- **Add provider headers via `with_headers`, never by editing the URL.** Adding a
  `Range` header does not break query-embedded auth (RFC 7233 — the signature covers the
  URL and query, not arbitrary request headers). If a strict Azure gateway needs an
  explicit `x-ms-version`, add it with `with_headers(...)`; it is applied to both the
  `HEAD` and the `Range` `GET`. Never re-sign or rewrite the signed URL per request.

> **Two different `with_total_size` methods — do not confuse them.**
>
> - `DownloadPounceBuilder::with_total_size(total)` — the **task-builder** knob shown
>   above. This is the one that enables concurrent download.
> - `PresignedRangeDownloadPlan::with_total_size(total)` — a **plan** method used by the
>   **serial** `PresignedRangeDownload` protocol (see the shipped presigned/SAS
>   examples). It does *not* turn on concurrency, because that protocol is serial-only.
>
> For concurrent download, use the **builder** method and the default range downloader.

---

## Peak memory and choosing `n`

Each in-flight part needs a buffer, so:

> **Peak transfer memory for one file ≈ `n × chunk_size`.**

This bound is documented on both `with_max_parts_in_flight` setters. Across the whole
client, memory sums over the files running at once:

```
client peak  ≈  max_upload_concurrency   × up_n   × up_chunk
             +  max_download_concurrency × down_n × down_chunk
```

There is **no SDK-side global memory cap** — an unbounded `n` really does scale memory,
so keep it bounded. Practical guidance:

- Keep `chunk_size` in the **1–8 MiB** range for object storage. Smaller chunks add
  request overhead; with larger chunks each retry re-sends more data, and they raise the
  memory bound.
- Start with `n` around **4** for large files on a fast link and measure. Going much
  higher rarely helps once the link is saturated, and it multiplies memory.
- Remember to multiply by the client-wide file concurrency: `4` downloads ×
  `n = 8` × `4 MiB` = up to `128 MiB` of buffers.

*(Downloads stream each range body straight into the pre-sized file, so real resident
memory is usually **below** the `n × chunk_size` upper bound; treat the formula as a
ceiling, not an exact figure.)*

---

## Progress, pause, and cancel under concurrency

Concurrency changes performance, **not** the lifecycle. Progress, pause, resume, and
cancel behave exactly as on the serial path — because they all observe a **single
contiguous prefix**.

**The watermark model.** Even though parts finish out of order, the SDK only ever
reports and persists the **longest gap-free prefix** of the file (the "watermark").
`record.progress()` is `watermark / total_size` (and `0.0` when the total is `0`).
There is no raw "bytes transferred" getter — if you need a byte count, derive it as
`(f64::from(record.progress()) * record.total_size() as f64) as u64` (going through
`f64` avoids the precision loss `f32` would cause on large files).

A practical consequence: if the last part of a file finishes before an earlier part,
**reported progress does not jump** — it waits until the gap fills. **Progress is
monotonic**, so a flat stretch is normal, not a hang. You get exactly **one coalesced
progress event per watermark advance** (not one per part), and exactly **one terminal
event**, with priority `Failed > Canceled > Complete`.

**Failure and panic semantics.** If one part fails after exhausting its retries, the
SDK stops launching *new* parts but lets the in-flight siblings drain, then fails the
file. A part that **panics** immediately cancels the in-flight siblings and then fails
the file (terminal status `Failed`, not `Canceled`). Either way the file ends in a
single terminal state, identical to serial.

**Completion.** The `Complete`/`Put Block List`/final-write step runs once, after the
join barrier, only when the watermark reaches the total **and** zero parts remain in
flight.

---

## Resuming a concurrent download (the `.rcdl` sidecar)

Serial and concurrent downloads resume differently:

- **Serial download** resume is **file-length based**: the SDK sees the partial file is
  `X` bytes long and asks for `bytes=X-`. No sidecar. (See
  [Resuming after a restart](resume-after-restart.md).)
- **Concurrent download** writes parts out of order into a **pre-sized** file, so the
  file length tells you nothing about which parts are done. Instead it keeps a
  **`<target>.rcdl`** ("resumable download log") sidecar next to the target: a bitmap of
  which fixed-size, chunk-aligned parts are durably on disk. A set bit always means those
  bytes were written at their correct offset and flushed to disk **before** the bit was
  set.

**When the sidecar is reused vs. discarded.** On the next run the sidecar is trusted
**only if every one of these matches**:

| Bound to | Must match |
|---|---|
| Range URL identity | Same URL (an FNV-1a hash of it, which includes host/port/path/query) |
| `total` | Same total size |
| `chunk` | Same chunk size (normalized `.max(1)`) |
| `max_parts` | Same `with_max_parts_in_flight` value |
| Target file | Its on-disk length already equals `total` (pre-sized, not truncated/deleted) |

If **any** of these differs — a different URL, a resized object, a different chunk size,
a different `max_parts`, or a target that was deleted/truncated — the stale sidecar is
**ignored** and a **fresh full download** runs. A leftover `.rcdl` can therefore only
ever cause a **safe re-download**, never silent corruption.

**Lifecycle.** The sidecar **survives cancel and failure on purpose**, so the next run
resumes and re-fetches **only the missing parts** (already-done parts short-circuit with
no network I/O). It is deleted **only on successful completion** (after the SDK verifies
the file length equals `total` and every part is present). A one-shot caller that will
never resume can delete it manually.

> **Cross-mode guard.** If a `.rcdl` sidecar exists next to the target, the **serial**
> download path refuses to run and returns `InvalidTaskState`. An interrupted concurrent
> download cannot be finished serially — resume it with `with_max_parts_in_flight(n)`
> (`n > 1`) again, or delete both the sidecar and the partial file to start over.

---

## Resuming a concurrent upload

Upload resume also honors the contiguous-prefix rule: on resume the SDK continues from
the longest verified gap-free prefix and **re-sends any parts that were ahead of a
hole** (they were never durably committed). How the prefix is discovered differs per
protocol:

- **Presigned multipart** — re-inject the parts your app persisted with
  `PresignedMultipartUpload::with_resumed_parts(...)`; `prepare` resumes past the longest
  verified contiguous prefix (offset + size must match) and re-sends the rest.
- **Aliyun OSS direct** — on resume the SDK adopts the existing multipart session via
  `ListMultipartUploads` and validates every expected part is present remotely. Persist
  the provider `UploadId` (`AliOssDirectUpload::current_upload_id()` /
  `UploadResumeInfo::provider_upload_id`) so you can also abort an orphaned session out
  of band. Set an `AbortMultipartUpload` lifecycle rule on the bucket so abandoned
  sessions stop accruing storage cost.
- **Azure direct / SAS** — there is no provider upload id; resume recovers the
  service-side **uncommitted block list**. Uncommitted blocks are billed until Azure
  garbage-collects them (about 7 days), so set a lifecycle rule.

For the full restart-recovery walkthrough (what to persist, how to rebuild the task),
see [Resuming uploads and downloads after a restart](resume-after-restart.md).

---

## Retry, backoff, and the stable-URL rule

**Per-part retry.** `with_max_chunk_retries(n)` (default `3`; `0` disables retry) applies
**per part** on both the serial and the concurrent paths. Within that budget the SDK
retries **only transient failures** — connection-layer errors and HTTP `408`, `429`, and
`5xx` — and waits between attempts with **exponential backoff (base 200 ms, cap 5,000 ms)
plus ±20% jitter**. Hard client errors (`4xx` other than 408/429, and `3xx`) **fail fast**
without consuming the budget, because retrying them would not help. An `InvalidRange`
error — for example from a server that answers a `Range` request with `200 OK` instead
of `206` — is one of those fast-fail cases.

**The stable-URL rule (important for concurrent download).** `StandardRangeDownload`
reuses **one fixed URL** (`task.url()`) for every concurrent part and has **no URL
refresher**. So a presigned/SAS GET URL you use for a concurrent download **must stay
valid for the entire transfer** — size the URL's expiry to cover the whole (possibly
resumed) download. The presigned URL-refresh machinery
(`PresignedDownloadUrlRefresher`) only runs on the **serial** `PresignedRangeDownload`
path, which is not the path a concurrent download takes.

---

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| Set `n > 1` but it still runs serial | One of the three gate conditions is missing | Ensure `n > 1`, a parallel-safe protocol, and a **known total** (`with_total_size` for downloads). |
| OSS/Azure download won't parallelize | You attached a provider download protocol (serial-only) | Remove `with_breakpoint_download(...)`; use the default `StandardRangeDownload` with a signed GET URL. |
| Download part fails with `InvalidRange` | Server answered `200 OK` (ignored `Range`) or a mismatched `Content-Range` | Point at an endpoint that returns `206 Partial Content` for `Range` requests. |
| Progress sits flat then jumps | Out-of-order parts finished ahead of a gap | Expected — progress tracks the contiguous prefix and is monotonic; it is not a hang. |
| Serial re-download errors `InvalidTaskState` | A leftover `<target>.rcdl` from an interrupted concurrent run | Resume with `with_max_parts_in_flight(n)` again, or delete the sidecar **and** the partial file. |
| Resume re-downloads from scratch | URL, total, chunk, `max_parts`, or target length changed since last run | Keep all five stable across runs, or accept the full re-fetch. |
| Signed URL expires mid-transfer | Concurrent download reuses one URL with no refresher | Issue a GET URL whose expiry covers the whole transfer (including resumes). |
| `CommandSendFailed` when enqueuing many tasks | The command queue is full (`try_enqueue` is fail-fast) | Raise `command_queue_capacity`, or retry with your own backoff. |

---

## Related reading

- [Resuming uploads and downloads after a restart](resume-after-restart.md) — serial,
  file-length-based resume, and what to persist to rebuild a task.
- [Provider feature flags: direct vs presigned/SAS](provider-feature-flags.md) — which
  provider feature to enable first.
- Provider guides:
  [Aliyun OSS direct](aliyun-oss-direct.md) ·
  [Aliyun OSS presigned](aliyun-oss-presigned.md) ·
  [Azure Blob direct](azure-blob-direct.md) ·
  [Azure Blob SAS](azure-blob-sas.md).
- [Crate README](../README.md) — the capability matrix, `MeowConfig`/builder reference,
  and error codes.
