# rusty-cat

[![Crates.io](https://img.shields.io/crates/v/rusty-cat.svg)](https://crates.io/crates/rusty-cat)
[![Docs.rs](https://docs.rs/rusty-cat/badge.svg)](https://docs.rs/rusty-cat)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/license/mit)

`rusty-cat` is an async Rust SDK for resumable file upload and download. It gives applications a compact public facade for building transfer tasks, running those tasks in a background scheduler, receiving progress callbacks, and plugging in protocol-specific implementations such as plain HTTP, Aliyun OSS, Aliyun OSS presigned URLs, Azure Blob Storage, and Azure Blob SAS URLs.

The crate is designed for applications that need reliable large-file transfer without forcing a specific storage backend or database layer. The SDK handles scheduling, chunk dispatch, retry, pause/resume/cancel commands, and progress fan-out. Your application remains responsible for business records, credential management, user permissions, and provider-specific setup.

The recommended public import is:

```rust
use rusty_cat::api::*;
```

Existing module paths still work, but `rusty_cat::api::*` is the stable, beginner-friendly entry point. Using the facade also makes future refactoring easier because most application code can import SDK types from a single module.

Start with the [developer documentation index](docs/README.md) and read the
[task lifecycle guide](docs/task-lifecycle.md) before integrating shutdown or
restart recovery.

## Package, platform, Rust, and license

| Item | Value |
|---|---|
| Crate | `rusty-cat` |
| Version | `0.3.6` |
| Rust edition | 2021 |
| MSRV | Rust 1.89 stable |
| Runtime | Tokio-based async runtime hosted by an internal scheduler thread |
| HTTP stack | `reqwest` with `rustls-tls` |
| Platforms | Release targets are native Linux, macOS, and Windows; Android and iOS remain compile-gated and experimental at runtime. See the verification boundary below. |
| License | MIT |
| Repository | <https://github.com/0barman/rusty-cat> |

See the [0.3.6 release notes](docs/release-0.3.6.md) for the Windows stable-Rust
compatibility fix and the local-file consistency changes.

### Platform support levels

| Support level | Targets | Required release gate and boundary |
|---|---|---|
| Native release gate | GitHub-hosted Linux, macOS, and Windows x64 MSVC | Before publication, core library tests must run on Rust 1.89 and current stable, and applicable native file-locking, rename, hardlink, and process-exit cases must pass. |
| Compile gate; runtime experimental | `aarch64-linux-android`, `aarch64-apple-ios` | Before publication, the library must cross-compile. Simulator/device filesystem behavior, including rename, sync, locking, and crash recovery, is not yet a release-tested guarantee. |
| Best effort | Other Rust target triples, filesystems, and architectures | No release gate. Support depends on Rust, Tokio, reqwest, `fs2`, and the target filesystem; validate in the deployment environment before relying on resumability. |

These rows define publication gates, not evidence that a particular checkout
has already passed them. Native Windows and mobile-target results must be linked
from the release CI before `0.3.6` is published; local cross-compilation is not a
substitute for those results.

`rust-version = "1.89"` is part of the package manifest. Consumers do not need
nightly Rust on Windows.

### Local-file identity, locking, and checkpoint boundaries

`rusty-cat` treats byte content, not a platform-specific inode or Windows file
ID, as the correctness identity of a transfer. If the platform and filesystem
permit a path replacement while a transfer is active, identical replacement
bytes can pass content validation; a same-length replacement or rewrite with
different bytes is rejected before successful completion. File metadata can be
used as a fast change signal, but is never sufficient proof that content is
unchanged.

Windows deliberately opens an active download target without delete sharing.
Consequently, deleting, renaming, or replacing that target path is rejected by
Windows while the transfer owns the handle, even when the proposed replacement
has identical content. This is a safety constraint, not physical-file identity
checking. Once the task reaches a terminal state and releases the handle, the
path can be replaced normally. Other platforms may permit an active rename; the
final visible-path digest checks still reject different content.

For file-backed uploads, the protocol MD5 and SHA-256 content snapshot are
computed in one initial scan. Actual part reads are checked against the
snapshot's SHA-256 blocks, and the source is content-validated again before
completion. For downloads, serial and parallel modes record SHA-256 part
digests in an adjacent private `.rusty-cat/<sha256-of-file-name>.rcdl`
namespace; completed ranges are revalidated through the visible path before
`Complete` and before the sidecar is removed. The former `<target>.rcdl`
location is never read, overwritten, migrated, or deleted because it may be an
ordinary user file. A checkpoint is
reusable across processes only when it is bound to the same semantic resource,
total and chunk grid, and a freshly observed strong ETag, and its stored local
part digests still match. Without that generation-bound sidecar proof, existing
local bytes are not inferred to be valid from length and are downloaded again
from byte zero.

Downloads acquire both a normalized path lease and an exclusive lock on the
actual target file. Every transfer write uses that same locked handle, so
cooperating processes and hardlink aliases contend on the underlying file. The
lock is advisory on platforms/filesystems with advisory locking: it is not a
security boundary against software that ignores locks, and filesystems that do
not implement the required locking semantics are not guaranteed.

Checkpoint publication follows this order: sync target data, write and sync a
new exclusively-created sidecar temporary file, close it, then rename it over
the sidecar (with a parent-directory sync on Unix). Recovery trusts only a
complete, valid old or new snapshot; an uncommitted part may be downloaded
again. This is a logical integrity guarantee, not a promise equivalent to
Windows write-through rename or universal power-loss durability across every
filesystem, storage device, or network mount.

### Badge Markdown

The Crates.io, Docs.rs, and License badge Markdown is shown below. These badges are safe to paste into downstream README files or generated documentation pages:

```markdown
[![Crates.io](https://img.shields.io/crates/v/rusty-cat.svg)](https://crates.io/crates/rusty-cat)
[![Docs.rs](https://docs.rs/rusty-cat/badge.svg)](https://docs.rs/rusty-cat)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/license/mit)
```

## Feature highlights

### Capability matrix

| Capability | Supported | Notes |
|---|:---:|---|
| HTTP resumable upload | Yes | Upload tasks are split into chunks and delegated to a `BreakpointUpload` implementation. The default style supports multipart/form-data chunk requests, and provider plugins can replace the request logic. |
| HTTP resumable download | Yes | Download tasks use `HEAD` during preparation and `GET` with `Range` headers for chunk transfer through `StandardRangeDownload`. |
| In-memory upload source | Yes | `UploadPounceBuilder::from_bytes(file_name, bytes, chunk_size)` uploads from an in-memory buffer instead of a file path; chunking and sizing behave the same as file-backed uploads. |
| Bounded in-memory binary GET | Yes | `MeowClient::try_enqueue_binary_task(...)` returns `Bytes` plus optional `Content-Type`. It has an isolated runtime/client/callback path, fixed concurrency 2, capacity 1024, cancel-only control, and is excluded from `snapshot()`. See [Binary download](docs/binary-download.md). |
| Aliyun OSS direct upload/download | Yes | Enable `aliyun-oss-direct`; use `AliOssDirectUpload` and `AliOssDirectDownload` when the client process is trusted to hold AccessKey credentials. |
| Aliyun OSS presigned upload/download | Yes | Enable `aliyun-oss-presigned`; use short-lived presigned part and range URLs generated by your backend. |
| Azure Blob direct upload/download | Yes | Enable `azure-blob-direct`; use Shared Key-authenticated block upload and range download when the client process is trusted to hold the storage account key. |
| Azure Blob SAS upload/download | Yes | Enable `azure-blob-sas`; use short-lived SAS URLs generated by your backend. |
| Provider-neutral presigned primitives | Yes | Enable `presigned` to use `PresignedMultipartUpload`/`PresignedRangeDownload` and their plans against any S3/OSS-style backend your server can presign, without a provider-specific feature. |
| Presigned plan validation | Yes | `PresignedMultipartUploadPlan::validate()` rejects zero-size parts, duplicate offsets, and parts outside the declared object size before any byte is sent. |
| Presigned completion/abort callbacks | Optional | A plan can carry `complete_request`/`abort_request` plus an optional `PresignedCompletionBodyBuilder`. Without those requests, the SDK cannot commit or clean up remote provider state. See [Presigned lifecycle](docs/presigned-lifecycle.md). |
| Presigned URL refresh on expiry | Yes | `PresignedUploadUrlRefresher`/`PresignedDownloadUrlRefresher` with `refresh_before_secs` refresh part/range URLs before they expire during long transfers. |
| Upload concurrency setting | Yes | `MeowConfig::builder().max_upload_concurrency(n)` limits the number of upload groups running at the same time. |
| Download concurrency setting | Yes | `MeowConfig::builder().max_download_concurrency(n)` limits the number of download groups running at the same time. |
| Upload progress | Yes | Per-task progress callbacks receive `FileTransferRecord` snapshots. See [Progress, status, and observability](docs/progress-status-observability.md). |
| Download progress | Yes | The same callback model is used for downloads, so upload and download UI code can share one progress-record handler. |
| Global progress listener | Yes | `register_global_progress_listener(...)` observes all tasks created by the client, which is useful for dashboards and persistence workers. |
| Global SDK debug logs | Yes | `set_debug_log_listener(...)` installs a process-global SDK log listener for diagnostics and integration tests. |
| Application-managed persistence | Yes | The SDK intentionally does not persist transfer state in an embedded database, so it can fit server, desktop, mobile, and CLI applications. |
| Custom database adaptation | Yes | Persist records from callbacks/listeners in your own database and rebuild tasks after restart. |
| Callback panic isolation | Yes | User callbacks are isolated from scheduler execution; callbacks should still be fast, non-blocking, and panic-free. |
| Chunk failure retry | Yes | `with_max_chunk_retries(...)` on upload and download builders controls additional retries after the first failed chunk transfer. |
| Upload prepare retry | Yes | `UploadPounceBuilder::with_max_upload_prepare_retries(...)` controls additional retries after the first failed upload preparation attempt. |
| Intra-file parallel parts | Opt-in | `UploadPounceBuilder::with_max_parts_in_flight(n)` uploads up to `n` chunks of one file concurrently. Default `1` (serial). Honored only for out-of-order-safe upload protocols — all four provider protocols (Aliyun OSS direct & presigned multipart, Azure Blob direct & SAS block blob); the built-in default HTTP upload stays serial. Progress, resume, pause, and cancel still observe a single contiguous prefix. See [Concurrent chunked transfer](docs/concurrent-chunk-transfer.md). |
| Intra-file parallel download | Opt-in | `DownloadPounceBuilder::with_max_parts_in_flight(n)` fetches range chunks concurrently and writes at absolute offsets. The effective window is limited to 256 parts and a client-wide 512 MiB budget on 64-bit targets (64 MiB on 32-bit). Cross-process `.rcdl` reuse requires a fresh strong ETag from HEAD; `with_total_size` skips HEAD and therefore cannot reuse old sidecar bits. See [Concurrent chunk transfer](docs/concurrent-chunk-transfer.md). |
| Transport-aware retry & backoff | Yes | Chunk retry covers transport failures plus HTTP 408, 429, and 5xx. Prepare outer retry covers connection-layer errors only. |
| Disk-full & local-content-change detection | Yes | Local I/O and identity failures are classified as `DiskFull`, `LocalFileRemoved`, or `ChecksumMismatch`, so callers can distinguish capacity, path, and byte-content failures. |
| Pause/resume/cancel | Yes | Use `pause(...)`, `resume(...)`, and `cancel(...)` with the returned `TaskId`. |
| Paused import / selective restore | Yes | `try_enqueue_paused(...)` imports a task in the paused state with no network/file I/O; resume only the user-selected subset on restart. |
| Resume after process restart / crash | Protocol-dependent | Serial and concurrent downloads use digest-backed `.rcdl` state; an untrusted legacy partial file without a matching sidecar restarts at byte zero. Cross-process part reuse requires a freshly observed strong ETag. Default uploads use server `nextByte`, and presigned uploads use reconciled re-injected parts. Direct OSS/Azure upload sessions cannot currently be injected into a new task. See [Resume after restart](docs/resume-after-restart.md). |
| Presigned multipart resume across restart | Yes | `PresignedMultipartUpload::with_resumed_parts(...)` re-injects parts persisted by a previous run; `prepare` resumes past the longest verified contiguous prefix and re-sends the rest. |
| Provider multipart session id surfacing | Yes | `UploadResumeInfo::provider_upload_id` and `AliOssDirectUpload::current_upload_id()` expose the provider `UploadId` (not a secret) so orphaned multipart sessions can be aborted out of band. |
| Upload abort on cancel | Protocol-dependent | Cancel invokes `abort_upload`, but its effect varies: some protocols clean remote state, Azure direct deletes the target blob, and presigned plans without `abort_request` are a no-op. |
| Snapshot diagnostics | Yes | `snapshot()` returns queued and active scheduler state for monitoring and troubleshooting. |
| Custom HTTP client | Yes | Inject a preconfigured `reqwest::Client` with `MeowConfigBuilder::http_client(...)` for proxy, TLS, default headers, or observability integration. |
| Custom upload protocol | Yes | Implement `BreakpointUpload` to integrate business-specific upload APIs. See [Custom protocols](docs/custom-protocols.md). |
| Custom download protocol | Yes | Implement `BreakpointDownload` to integrate custom range-download authentication or headers. See [Custom protocols](docs/custom-protocols.md). |
| Custom upload request method/headers | Yes | `with_method(...)` and `with_headers(...)` on `UploadPounceBuilder` customize the default upload request line and headers. |
| Per-task download HTTP override | Yes | `DownloadPounceBuilder::with_breakpoint_download_http(...)` overrides the range `Accept` header for a single task without changing global config. |

### Architecture overview

| Layer | Main types | Responsibility |
|---|---|---|
| Public facade | `rusty_cat::api::*` | One import point for client, config, task builders, callbacks, errors, status, logs, and optional providers. |
| Client | `MeowClient` | Owns immutable config, lazily starts independent Pounce/Binary executors, submits tasks, controls lifecycle, and manages listeners. |
| Config | `MeowConfig`, `MeowConfigBuilder` | Defines concurrency, queue capacities, HTTP timeout/keepalive, range-download behavior, and optional custom HTTP client. |
| Task builders | `UploadPounceBuilder`, `DownloadPounceBuilder` | Convert simple parameters into executable `PounceTask` values. |
| Scheduler | Internal executor | Runs background workers, queues tasks, dispatches chunks, retries failures, and emits events. |
| Protocol plugins | `BreakpointUpload`, `BreakpointDownload` | Implement provider-specific signing, presigned URLs, chunk requests, and completion behavior. |
| Observability | `FileTransferRecord`, `TransferSnapshot`, `Log` | Per-task progress, global progress events, queue snapshots, and debug logs. |

## Quick start

Add the crate:

```toml
[dependencies]
rusty-cat = "0.3.6"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

For OSS providers, enable only what you need. Keeping the feature list small reduces optional dependencies and makes it clearer which cloud integrations your application actually uses:

```toml
[dependencies]
rusty-cat = { version = "0.3.6", features = ["aliyun-oss-direct"] }
```

| Feature | Purpose |
|---|---|
| `aliyun-oss-direct` | Aliyun OSS direct upload/download with AccessKey credentials and OSS Signature Version 4 signing. |
| `aliyun-oss-presigned` | Aliyun OSS presigned multipart upload and range download helpers. |
| `azure-blob-direct` | Azure Blob upload/download with Shared Key authentication. |
| `azure-blob-sas` | Azure Blob SAS upload/download helpers. |
| `presigned` | Provider-neutral presigned multipart/range primitives. |
| `aliyun-oss` | Convenience umbrella that currently enables `aliyun-oss-presigned` only. It does **not** pull in `aliyun-oss-direct`. |
| `azure-blob` | Convenience umbrella that currently enables `azure-blob-sas` only. It does **not** pull in `azure-blob-direct`. |
| `oss-providers` | Both umbrellas together: `aliyun-oss` + `azure-blob` (Aliyun OSS presigned + Azure Blob SAS). It does not enable the direct/AccessKey/Shared Key features. |
| `all` | Enables all four provider features (`aliyun-oss-direct`, `aliyun-oss-presigned`, `azure-blob-direct`, `azure-blob-sas`). Use it for broad integration testing, not minimal production builds. |

The `aliyun-oss`, `azure-blob`, and `oss-providers` umbrellas intentionally select the **presigned/SAS** flows, because those are the recommended model for untrusted clients (your backend holds the credentials). When you need the direct AccessKey/Shared Key flows, enable `aliyun-oss-direct` and/or `azure-blob-direct` explicitly.

For a focused comparison of direct credentials versus presigned/SAS URLs, see [Provider feature flags: direct vs presigned/SAS](docs/provider-feature-flags.md).

### Complete end-to-end example

This example starts from `MeowConfig`, creates a `MeowClient`, registers listeners, builds a task, submits it, waits for the completion/failure signal, inspects a snapshot, and closes the client. It uses an HTTP range download task because that path works without cloud credentials; the same client lifecycle applies to upload tasks and OSS/Azure provider tasks.

```rust,no_run
use std::sync::Arc;
use std::time::Duration;

use rusty_cat::api::{DownloadPounceBuilder, Log, MeowClient, MeowConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let config = MeowConfig::builder()
        .max_upload_concurrency(2)
        .max_download_concurrency(2)
        .http_timeout(Duration::from_secs(30))
        .tcp_keepalive(Duration::from_secs(60))
        .command_queue_capacity(256)
        .worker_event_queue_capacity(1024)
        .build()?;
    let client = MeowClient::new(config);

    client.set_debug_log_listener(Some(Arc::new(|log: Log| {
        println!("[rusty-cat] {log}");
    })))?;
    let listener_id = client.register_global_progress_listener(|record| {
        println!(
            "global: task={} file={} progress={:.2}% status={:?}",
            record.task_id(),
            record.file_name(),
            record.progress() * 100.0,
            record.status(),
        );
    })?;

    let task = DownloadPounceBuilder::new(
        "example.bin",
        "./downloads/example.bin",
        1024 * 1024,
        "https://example.com/example.bin",
    )
    .with_client_file_sign("business-file-id-001")
    .with_max_chunk_retries(3)
    .build();

    // Save the result so cleanup and close still run after a transfer failure.
    let result = client
        .enqueue_and_wait(task, |record| {
            println!(
                "task={} progress={:.2}% status={:?}",
                record.task_id(),
                record.progress() * 100.0,
                record.status(),
            );
        })
        .await;

    let snapshot = client.snapshot().await?;
    println!(
        "snapshot: queued={}, active={}",
        snapshot.queued_groups, snapshot.active_groups
    );
    client.unregister_global_progress_listener(listener_id)?;
    client.set_debug_log_listener(None)?;
    client.close().await?;

    let outcome = result?;
    println!("task {} complete; payload={:?}", outcome.task_id, outcome.payload);
    Ok(())
}
```

### `MeowClient` API guide

| Function | Use it when | Important notes |
|---|---|---|
| `MeowClient::new(config)` | Create the SDK entry point. | The executor starts lazily on the first task operation. `MeowClient` is not `Clone` because it owns scheduler state; wrap it in `Arc<MeowClient>` when multiple async tasks or threads need shared access. |
| `http_client()` | Need a `reqwest::Client` aligned with SDK config. | Returns the injected custom client when one was configured; otherwise builds a client from `http_timeout` and `tcp_keepalive`. This is useful when protocol code outside the executor must make compatible HTTP calls. |
| `register_global_progress_listener(listener)` | Observe all task progress records. | Returns a `GlobalProgressListenerId`. Use this for UI-wide progress aggregation, persistence queues, or monitoring. Keep callback work fast. |
| `unregister_global_progress_listener(id)` | Remove one global listener. | Returns `Ok(false)` when the ID does not exist, so cleanup code can call it safely. |
| `clear_global_listener()` | Remove every global progress listener. | Useful during shutdown, integration-test cleanup, or application logout flows. |
| `set_debug_log_listener(Some(listener))` | Receive SDK debug logs. | The listener is process-global rather than client-local. Pass `None` to clear it before shutdown or when tests need isolation. |
| `try_enqueue(task, progress_cb, complete_cb).await` | Submit an upload/download task. | This performs asynchronous submission, not synchronous transfer completion. It fails fast when the command queue is full. Store the returned `TaskId` for pause/resume/cancel operations. |
| `enqueue_and_wait(task, progress_cb).await` | Await one file task through `Complete`, `Failed`, or `Canceled`. | Returns `TaskOutcome` on success and a `MeowError` for failure/cancel. Await it before `close()`. |
| `try_enqueue_paused(task, progress_cb, complete_cb).await` | Import a task in the paused state without scheduling it. | Performs no network or file I/O until you call `resume(...)`. Use it for restart/restore: import many tasks, then resume only the user-selected subset. Same fail-fast back-pressure as `try_enqueue`. See the persistence section below. |
| `try_enqueue_binary_task(task, complete_cb)` | Download a small bounded response into memory. | Synchronous fail-fast admission; HTTP runs on isolated resources. See [Binary download](docs/binary-download.md). |
| `pause(task_id).await` | Pause a queued or running task. | Sends a command to the scheduler. A paused task can be resumed later with the same `TaskId`. |
| `resume(task_id).await` | Continue a paused task. | Keeps the same `TaskId` and asks the scheduler to continue from available local/remote progress. |
| `cancel(task_id).await` | Stop a task. | Cancellation is best-effort and may run provider cleanup such as aborting a multipart session. Treat canceled tasks as terminal unless your application deliberately creates a new task. |
| `snapshot().await` | Inspect queued and active groups. | Useful for dashboards, health checks, and debugging scheduler behavior under concurrency. |
| `close().await` | Shut down. | Mandatory for clean shutdown: cancels in-flight work, flushes `Paused` events, drains callbacks, and joins the scheduler thread. Calling it synchronously from a transfer callback fails with `InvalidTaskState`; return and schedule close elsewhere. Await intended work first; see [Task lifecycle](docs/task-lifecycle.md). |
| `is_closed()` | Check whether the client is closed. | A successfully closed client cannot be reopened; create a new `MeowClient` if you need to submit more work. |

There is no public `enqueue(...)` method in the current API. Use `try_enqueue(...)`; the name is intentional because enqueue uses fail-fast backpressure. If your application submits many tasks at once, increase `command_queue_capacity` or retry `CommandSendFailed` with your own backoff policy.

## Configuration parameters

### `MeowConfig` and `MeowConfigBuilder`

Start with `MeowConfig::default()` for a safe baseline or use `MeowConfig::builder()` for validated customization. The configuration is immutable after the client is created, which prevents accidental runtime changes from affecting tasks already in the scheduler.

| Parameter | Default | Constraint | Description |
|---|---:|---|---|
| `max_upload_concurrency` | `2` | `>= 1` | Maximum upload groups processed concurrently. |
| `max_download_concurrency` | `2` | `>= 1` | Maximum download groups processed concurrently. |
| `breakpoint_download_http.range_accept` | `application/octet-stream` | Valid header value | Default `Accept` header for range download chunks. |
| `http_client` | `None` | Reusable `reqwest::Client` | Optional custom HTTP client for proxy, TLS, default headers, or observability. |
| `http_timeout` | `5s` | Positive duration | Per-request timeout for internally built HTTP clients. |
| `tcp_keepalive` | `30s` | Positive duration | TCP keepalive for internally built HTTP clients. |
| `command_queue_capacity` | `128` | `>= 1` | Queue for enqueue, pause, resume, cancel, snapshot, and close commands. |
| `worker_event_queue_capacity` | `256` | `>= 1` | Queue for progress/state events. |
| `binary_download_config` | `None` (safe defaults on first use) | Valid `BinaryDownloadConfig` | Optional binary GET body limit, timeout/keepalive override, redirect limit, and retry delays. It does not reuse the Pounce HTTP client. |

| Builder/accessor | Description |
|---|---|
| `MeowConfig::builder()` | Creates a builder initialized with defaults. |
| `max_upload_concurrency(n)` / `max_upload_concurrency()` | Sets/reads upload concurrency. Recommended range: `1..=64`. |
| `max_download_concurrency(n)` / `max_download_concurrency()` | Sets/reads download concurrency. Recommended range: `1..=64`. |
| `http_client(client)` | Injects a custom `reqwest::Client` for proxy, TLS, headers, or observability. |
| `http_timeout(duration)` / `http_timeout()` | Sets/reads HTTP timeout. Typical range: `3s..=60s`. |
| `tcp_keepalive(duration)` / `tcp_keepalive()` | Sets/reads TCP keepalive. Typical range: `15s..=120s`. |
| `command_queue_capacity(n)` / `command_queue_capacity()` | Sets/reads control queue capacity. |
| `worker_event_queue_capacity(n)` / `worker_event_queue_capacity()` | Sets/reads worker event queue capacity. |
| `breakpoint_download_http(config)` / `breakpoint_download_http()` | Sets/reads range-download HTTP behavior. |
| `binary_download_config(config)` / `binary_download_config()` | Sets/reads optional bounded binary GET behavior. Default body limit is 5 MiB and hard maximum is 64 MiB. |
| `build()` | Validates constraints and returns `MeowConfig`. |

### `UploadPounceBuilder`

| Method | Required? | Description |
|---|:---:|---|
| `UploadPounceBuilder::new(file_name, file_path, chunk_size)` | Yes | Creates a file-backed upload task. `chunk_size == 0` is normalized to the SDK default. |
| `UploadPounceBuilder::from_bytes(file_name, bytes, chunk_size)` | Alternative | Creates an in-memory upload task. The `Vec<u8>` is moved into `bytes::Bytes`. |
| `with_url(url)` | Usually yes | Sets target upload URL. For direct OSS/Azure, this is the final object/blob URL. For presigned flows, it is commonly the first part URL or logical target URL. |
| `with_file_path(path)` | Optional | Replaces the local file source. |
| `with_bytes(bytes)` | Optional | Replaces the source with in-memory bytes. |
| `with_method(method)` | Optional | Sets HTTP method for default/custom upload requests. Default is `POST`. |
| `with_headers(headers)` | Optional | Replaces base request headers. |
| `with_breakpoint_upload(upload)` | Optional | Sets a per-task custom `BreakpointUpload`, such as Aliyun/Azure direct or presigned upload. See [Custom protocols](docs/custom-protocols.md). |
| `with_max_chunk_retries(retries)` | Optional | Sets additional retries after the first failed chunk attempt. `0` disables chunk retry. Default is `3`. |
| `with_max_upload_prepare_retries(retries)` | Optional | Sets additional retries after the first failed upload prepare attempt. Default is `3`. |
| `with_max_parts_in_flight(n)` | Optional | Maximum chunks of this file uploaded concurrently. Default `1`; `0` normalizes to `1`. The protocol must opt into out-of-order safety. Effective runs are limited to 256 part tasks and the shared 512 MiB/64 MiB (64-bit/32-bit) client budget, including upload verification scratch. See [Concurrent chunk transfer](docs/concurrent-chunk-transfer.md). |
| `build()` | Yes | Reads file metadata for file-backed uploads and returns `PounceTask`; may return `std::io::Error`. |

Beginner tips:

- Use a `chunk_size` between `1 MiB` and `8 MiB` for common object storage workloads unless your provider requires a different size. Very small chunks increase request overhead; very large chunks reduce retry granularity.
- Put provider protocol objects in `Arc` and pass them to `with_breakpoint_upload(...)` because the executor can move transfer work across async tasks.
- For restart recovery, persist enough business metadata in your own database to rebuild the same logical task later, including local path, remote URL/object key, direction, chunk size, and provider type.

When you do not attach a provider plugin, uploads use the built-in default protocol. Its exact request/response format — and how the `fileMd5` signature is derived — is documented in [Default HTTP upload protocol contract](docs/default-http-upload-protocol.md).

### `DownloadPounceBuilder`

| Method | Required? | Description |
|---|:---:|---|
| `DownloadPounceBuilder::new(file_name, file_path, chunk_size, url)` | Yes | Creates a range-download task. The SDK uses `HEAD` for prepare and `GET` with `Range` for chunks. |
| `with_url(url)` | Optional | Replaces the remote download URL. |
| `with_file_path(path)` | Optional | Replaces the local output path. |
| `with_headers(headers)` | Optional | Replaces base request headers for `HEAD` and range `GET`. |
| `with_client_file_sign(sign)` | Optional | Sets a client-defined file signature shown in progress records. Useful for database keys. |
| `with_breakpoint_download(download)` | Optional | Sets a per-task custom `BreakpointDownload`, such as Aliyun/Azure direct or presigned range download. See [Custom protocols](docs/custom-protocols.md). |
| `with_breakpoint_download_http(config)` | Optional | Overrides per-task range download HTTP behavior. |
| `with_max_chunk_retries(retries)` | Optional | Sets additional retries after the first failed range chunk attempt. `0` disables chunk retry. Default is `3`. |
| `with_max_parts_in_flight(n)` | Optional | Maximum range chunks fetched concurrently. Default `1`; `0` normalizes to `1`. The protocol must be range-safe and total size must resolve. Effective runs are limited to 256 part tasks and the shared 512 MiB/64 MiB (64-bit/32-bit) client budget. |
| `with_total_size(size)` | Optional | Supplies a known total and skips HEAD. Useful for GET-only URLs, but an old concurrent-download `.rcdl` cannot be authenticated/reused without a fresh strong ETag from HEAD. |
| `build()` | Yes | Returns `PounceTask`. Validation happens during enqueue/runtime. |

Download HTTP methods are intentionally not configurable. Resumable HTTP download depends on standard `HEAD` and `GET` range behavior. If a gateway or provider needs a non-standard method, implement `BreakpointDownload` and inject it with `with_breakpoint_download(...)`.

#### Range headers and query-embedded auth (presigned / SAS)

A presigned/SAS range URL must authorize `Range` requests and any required
provider headers. Whether an extra header is allowed depends on the signature's
signed-header policy, so verify this in the backend signer rather than assuming
all presigned formats behave alike. The range `Accept` header is overridable per
task; provider headers can be added through task base headers, which apply to
HEAD and GET. Do not edit or log the signed query string.

#### Download resume and content checkpoints (`.rusty-cat/<hash>.rcdl`)

Serial and concurrent downloads write a losslessly path-derived, SHA-256-named
sidecar under an adjacent `.rusty-cat` directory. It records durable part bits
and SHA-256 digests, survives an
interruption, and is deleted only after successful final content validation.
Concurrent mode pre-sizes its positioned-write target; serial mode keeps the
visible length equal to the verified contiguous prefix. A legacy partial file
without a matching sidecar has no remote-generation proof and is restarted at
byte zero.

Cross-process reuse in either mode requires matching total, chunk grid,
semantic resource URL, stable effective range-request context, a freshly
observed strong ETag, and matching local part digests. Authentication-only
signed query parameters are ignored while semantic selectors remain; all
binding material is persisted only as one domain-separated SHA-256 digest, not
as raw URL/header/credential text. Changing `max_parts_in_flight` does not
invalidate compatible parts. `with_total_size` skips HEAD, so old sidecar bits
are deliberately not reused; the first 206 may latch a strong ETag for the
current run but cannot authenticate a checkpoint from an earlier process.

The `.rusty-cat` directory is a reserved SDK checkpoint namespace and contains
an ownership marker. A file/symlink at that path, or a non-empty unmarked
directory, fails closed and is left untouched. An empty directory may be
initialized. Legacy `<file>.rcdl` files are deliberately ignored and left
unchanged; 0.3.6 starts fresh instead of trying to infer whether such a file is
old SDK state or user data. Every `.rusty-cat` path component is reserved even
before an ownership marker exists; a visible target inside it, including
through a symlink alias, is rejected before it can race a namespace claim or
overwrite a final/temporary checkpoint.

The same validator rule applies to serial and parallel downloads. If HEAD
prepared a strong ETag, every 206 must return that same strong ETag. Without a
prepared validator, a download that needs multiple ranges requires the first
206 to provide a strong ETag and every later response to match it; missing,
weak, or changing validators fail closed. A one-range download may complete
without an ETag because no bytes from different responses can be combined.

For a step-by-step, beginner-friendly walkthrough of concurrent chunked upload **and** download — the two concurrency knobs, the parallel-safe protocol matrix, per-provider recipes, memory sizing, and `.rcdl` resume — see [Concurrent chunked transfer (single-file parallel parts)](docs/concurrent-chunk-transfer.md).

#### Custom-protocol migration

Existing custom `BreakpointDownload` implementations stay serial because
`supports_parallel_parts` defaults to `false`. Opt in only when each range
request is independent and the server meets the 206, content-range, identity
encoding, and strong-ETag contract in the
[custom protocol](docs/custom-protocols.md) and
[concurrency](docs/concurrent-chunk-transfer.md) guides.

`resume_identity()` independently defaults to `None`. This is fail-closed:
custom protocols still validate the current run, but cannot reuse an earlier
process's sidecar until they return complete, stable representation/principal
context. Externally injected HTTP clients likewise disable cross-process reuse
because their hidden default headers cannot be canonicalized.

## Error handling and retries

Most SDK calls return `Result<_, MeowError>`. `MeowError::code()` is a stable numeric code (an `i32`, suitable for FFI or structured logging), `msg()` is human-readable context, and `http_status()` returns a captured response status when available. Branch on the code/status, never on message text.

The codes you will most often branch on:

| Code | `InnerErrorCode` | When it happens |
|---:|---|---|
| `102` | `ParameterEmpty` | A required value (URL, file name, non-zero size) was empty at enqueue. |
| `103` | `DuplicateTaskError` | The same file/task is already queued, running, or paused. |
| `107` | `ClientClosed` | The client was closed; create a new `MeowClient` to submit more work. |
| `108` | `TaskNotFound` | `pause`/`resume`/`cancel` referenced an unknown or already-terminal `TaskId`. |
| `111` | `CommandSendFailed` | The command queue is full (fail-fast back-pressure). Retry with backoff or raise `command_queue_capacity`. |
| `116` | `ChecksumMismatch` | Local bytes no longer match the upload content snapshot or a committed download-part digest. |
| `117` | `InvalidTaskState` | The operation is invalid in the task's current state (for example resuming a task that is not paused). |
| `120` | `TaskCanceled` | The task was canceled before reaching `Complete`. |
| `121` | `DiskFull` | The local disk ran out of space while writing a download. |
| `122` | `LocalFileRemoved` | The local path disappeared or changed length/type while required by the transfer. A same-length byte-content mismatch is reported as `ChecksumMismatch`; an identical-content replacement can pass where the OS permits it, while Windows rejects active download-target replacement through its sharing mode. |
| `123` | `BinaryTaskQueueFull` | The bounded binary executor has 1024 accepted tasks. |
| `124` | `BinaryBodyTooLarge` | A binary response exceeded its configured in-memory limit. |

See the [Error codes reference](docs/error-codes.md) for the complete list (codes `101`–`124`), with suggested handling.

### Retry and transient errors

Two builder knobs control how many times a failed step is retried before the task fails:

- `with_max_chunk_retries(n)` — extra attempts after the first failed chunk transfer (default `3`; `0` disables chunk retry).
- `UploadPounceBuilder::with_max_upload_prepare_retries(n)` — extra attempts after the first failed upload prepare (default `3`).

Chunk retry covers transport failures and HTTP 408, 429, and 500–599, using
exponential backoff with jitter. Other known HTTP statuses, malformed payloads,
and invalid ranges fail fast. The upload prepare outer loop retries only
connection-layer `HttpError`, not prepare response statuses. Binary GETs have a
separate delay schedule and retry transport/body-read failures, not non-success
statuses.

To continue a task that ultimately failed (or that you paused), use `resume(...)`
for the same live task or rebuild it only when its protocol supports the required
checkpoint. See [Resume after a process restart](docs/resume-after-restart.md).

## SDK debug logs: levels and what to persist

`set_debug_log_listener(Some(listener))` installs one process-global callback that receives every `Log` the SDK emits. Each entry carries a `LogLevel` that tells you how to treat it. The levels are ordered by severity and are designed so you can split a high-volume diagnostic stream from a small, durable troubleshooting stream:

| Level | What it is | Frequency | Persist? |
|-------|------------|-----------|----------|
| `Trace` | per-chunk / per-poll / per-retry hot-loop detail | very high | **No** — drop or sample only; never store |
| `Debug` | low-volume internal diagnostics | a few per task | optional / short-term ring buffer only |
| `Info` | normal operational notes | low | optional |
| `Key` | task & executor **lifecycle checkpoints** (created, enqueued, started, prepared, resumed, completed, paused, cancelled, closed, listener changes) | a few per task | **Yes** — keep and forward when reporting a bug |
| `Warn` | recoverable anomaly, caller misuse, backpressure, failed cleanup | per anomaly | **Yes** |
| `Error` | a chunk/part upload or download **failed**, a task failed, an HTTP error, a signing failure, or a caught panic — with full structured context | per failure | **Yes** |

**Rule of thumb:** persist everything at `>= LogLevel::Key` (`Key | Warn | Error`) and never persist the high-frequency `Trace` stream. The library exposes this policy so you do not have to hard-code it:

```rust
use std::sync::Arc;
use rusty_cat::api::{Log, LogLevel, MeowClient, MeowConfig};

# fn ship_to_log_server(_: &Log) {}
# fn local_ring_buffer(_: &Log) {}
let client = MeowClient::new(MeowConfig::default());
client.set_debug_log_listener(Some(Arc::new(|log: Log| {
    // Never store the high-frequency Trace tier — it fires per chunk/poll/retry.
    if log.level() == LogLevel::Trace {
        return;
    }
    if log.level().persist_recommended() {
        // Key | Warn | Error → keep these and ship them to your log server.
        ship_to_log_server(&log);
    } else {
        // Debug | Info → optional; a short-term in-memory ring buffer at most.
        local_ring_buffer(&log);
    }
}))).unwrap();
// where `ship_to_log_server` / `local_ring_buffer` are your own sinks.
```

When you hit a problem, **collect the persisted `Key + Warn + Error` entries and send them to the library author** — the `Key` checkpoints reconstruct what the task was doing and the `Error` entries pinpoint the failure.

### Structured fields for triage

Besides `log.level()`, `log.tag()` and `log.message()`, `Error` and `Key` entries may carry structured context you can index or filter on: `task_id()`, `object_key()`, `part_index()`, `offset()`, `byte_len()`, `http_status()`, `attempt()`, `max_retries()`, `error_code()`, and `url()`. `Log`'s `Display` also appends every present field as ` key=value`, so `format!("{log}")` is a self-describing line.

### Secret safety

The SDK never puts a raw SAS/presigned URL, signature, credential, response body, or request header into a log: URLs go through `sanitize_url()` (signature/credential query params are redacted) and error chains/response bodies go through `redact_secrets()` before they reach a `Log`. Both helpers are public (`rusty_cat::api::{sanitize_url, redact_secrets}`) so you can apply the same redaction to anything **you** add in a progress callback or debug listener — do not re-log a raw task URL or header map yourself.

## OSS upload/download developer guides

OSS and Blob workflows are provider-specific, so detailed beginner guides live in separate documents. The SDK does not persist your keys, secrets, account keys, tokens, presigned URLs, or SAS URLs in a built-in database or credential store. Some values are held in memory while executing tasks. You must provide them from your application or trusted backend, and you should avoid logging them in progress callbacks or debug listeners.

If you are deciding which provider feature to enable first, start with [Provider feature flags: direct vs presigned/SAS](docs/provider-feature-flags.md).

| Guide | Feature flag | Runnable test-app coverage |
|---|---|---|
| [Aliyun OSS direct upload/download](docs/aliyun-oss-direct.md) | `aliyun-oss-direct` | [`aliyun-direct`](../test-app/README.md#provider-direct-场景) uses the official SDK provider with dedicated live-test configuration. |
| [Aliyun OSS presigned upload/download](docs/aliyun-oss-presigned.md) | `aliyun-oss-presigned` | [`aliyun-presigned`](../test-app/src/download/aliyun_presigned.rs) currently covers signed range download. |
| [Azure Blob direct upload/download](docs/azure-blob-direct.md) | `azure-blob-direct` | [`azure-direct`](../test-app/README.md#provider-direct-场景) uses the official SDK provider with dedicated live-test configuration. |
| [Azure Blob SAS upload/download](docs/azure-blob-sas.md) | `azure-blob-sas` | [`loonadm`](../test-app/src/main.rs) exercises backend-issued presigned/SAS upload and download paths. |

To override the dedicated Aliyun live-test configuration, set `RC_ALIYUN_BUCKET`,
`RC_ALIYUN_ACCESS_KEY_ID`, and `RC_ALIYUN_ACCESS_KEY_SECRET`. Optional settings
are `RC_ALIYUN_REGION`, `RC_ALIYUN_OBJECT_PREFIX`, `RC_DIRECT_UPLOAD_SIZE`,
`RC_DIRECT_PART_SIZE`, and `RC_OUT_DIR`:

```text
cargo run --manifest-path test-app/Cargo.toml -- aliyun-direct
```

To override the dedicated Azure live-test configuration, set `RC_AZURE_ACCOUNT_NAME`,
`RC_AZURE_ACCOUNT_KEY`, and `RC_AZURE_CONTAINER`. Optional settings are
`RC_AZURE_BLOB_PREFIX`, `RC_DIRECT_UPLOAD_SIZE`, `RC_DIRECT_PART_SIZE`, and
`RC_OUT_DIR`:

```text
cargo run --manifest-path test-app/Cargo.toml -- azure-direct
```

Both scenarios use the official SDK provider implementations and support
environment overrides for isolated live-test configuration. The former
hand-written signers were not migrated into test-app. Do not reuse test-app
configuration or artifacts in a production client.

## Persistence and custom database integration

`rusty-cat` intentionally has no built-in database. This keeps the SDK small and lets you choose SQLite, PostgreSQL, Redis, a mobile database, or an existing business persistence layer. The SDK emits progress records and terminal states; your application decides how those records map to durable business state.

Recommended pattern:

1. Create your own transfer table with fields such as business file ID, local path, remote URL/object key, direction, chunk size, provider, status, progress, and credential reference.
2. Register per-task and/or global progress callbacks.
3. In callbacks, persist `FileTransferRecord` values or forward them to a persistence worker. Do not perform slow database writes directly on the callback path; prefer batching or sending records to your own worker queue.
4. On process restart, query unfinished rows and rebuild equivalent `PounceTask` values.
5. Reconcile the checkpoint and enqueue only when the protocol's recovery
   contract supports it. Direct OSS/Azure uploads currently cannot inject an old
   multipart/block session into a new task.

Never persist raw cloud secrets unless your security model explicitly allows it. Prefer storing a reference to a backend-owned credential or generating fresh short-lived presigned/SAS URLs.

> **New to restart recovery?** Start with the protocol-by-protocol
> [restart capability matrix](docs/resume-after-restart.md).

### Importing tasks in the paused state (selective restore)

`try_enqueue_paused(task, progress_cb, complete_cb)` imports a task in the `Paused` state **without scheduling it**. Unlike `try_enqueue`, it performs no network or file I/O: the task is registered into the scheduler and a single `Paused` progress record is emitted, but no `HEAD`/`GET`/upload request is sent and no file is opened until you start it.

This is the entry point for "restore on restart, then let the user choose what to download now":

1. On restart, rebuild a `PounceTask` for each unfinished row in your database.
2. Import each one with `try_enqueue_paused(...)` and keep the returned `TaskId`.
3. Render your task list from your own persisted progress. The `Paused` record reports `0.0` progress because no `prepare` has run yet, so the SDK does not know the real offset until the task is resumed.
4. When the user selects transfers, call `resume(task_id)`; the rest stay paused.
   The real checkpoint is protocol-specific (digest-validated download `.rcdl`,
   server `nextByte`, or reconciled presigned parts).

```rust,no_run
use rusty_cat::api::{DownloadPounceBuilder, MeowClient, MeowConfig};

async fn restore_and_start(client: &MeowClient) -> Result<(), rusty_cat::api::MeowError> {
    // Rebuilt from your own database after a restart.
    let task = DownloadPounceBuilder::new(
        "report.bin",
        "./downloads/report.bin",
        1024 * 1024,
        "https://example.com/report.bin",
    )
    .build();

    // Imported paused: no HTTP request is sent and no file is opened here.
    let task_id = client
        .try_enqueue_paused(task, |_record| {}, |_id, _payload| {})
        .await?;

    // Later, when the user chooses to start this transfer:
    client.resume(task_id).await?;
    Ok(())
}
```

Run the [`restore-paused`](../test-app/src/scenarios/local.rs) test-app scenario for a complete demonstration that imports several tasks paused and resumes only a selected subset:

```text
cargo run --manifest-path test-app/Cargo.toml -- restore-paused
```

## Runnable test-app scenarios

All runnable transfer demonstrations live in [`test-app`](../test-app/README.md). Run these commands from the repository root.

For the complete evidence-based inventory and dated results, see the [test scenario and verification matrix](docs/test-scenarios.md) ([简体中文](docs/test-scenarios.zh-CN.md)).

| Scenario | What it demonstrates |
|---|---|
| [`loonadm`](../test-app/src/main.rs) | Backend login, multipart upload/complete, provider downloads, metrics, and consistency checks. |
| [`direct-download`](../test-app/src/download/direct.rs) | Range-download a supplied URL with configurable concurrency and optional size/MD5 checks. |
| [`otacdn-x86-64`](../test-app/src/scenarios/otacdn_x86_64.rs) | Live CDN pause/resume with fixed size and release-digest verification. |
| [`azure-download`](../test-app/src/download/oss_azure.rs) | Read-authorized Azure Range download with four in-flight parts and size verification. |
| [`azure-sas-roundtrip`](../test-app/src/scenarios/azure_sas.rs) | Put Block/Block List upload followed by parallel Range readback and SHA-256 verification. |
| [`aliyun-presigned`](../test-app/src/download/aliyun_presigned.rs) | Aliyun OSS V4 presigned range download with expiry and size validation. |
| [`aliyun-direct`](../test-app/README.md#provider-direct-场景) | Official Aliyun OSS direct-provider upload/download using dedicated live-test configuration. |
| [`aliyun-prepare-files`](../test-app/src/scenarios/aliyun_upload_matrix.rs) | Generate deterministic zero-byte and chunk-boundary fixtures without accessing a cloud service. |
| [`aliyun-upload-matrix`](../test-app/src/scenarios/aliyun_upload_matrix.rs) | Run direct and presigned upload/readback across boundary-sized fixtures. |
| [`azure-direct`](../test-app/README.md#provider-direct-场景) | Official Azure Blob direct-provider upload/download using dedicated live-test configuration. |
| [`local-http`](../test-app/src/scenarios/local.rs) | Local HTTP upload/download pause-resume and byte-exact verification. |
| [`resume-restart`](../test-app/src/scenarios/local.rs) | Restart after an injected range failure, reuse the validated checkpoint, and fetch only the missing part. |
| [`restore-paused`](../test-app/src/scenarios/local.rs) | Paused import with zero initial I/O and selective resume. |
| [`local-all`](../test-app/src/scenarios/local.rs) | Run all three deterministic local scenarios in sequence. |

For example:

```text
cargo run --manifest-path test-app/Cargo.toml -- local-all
```

## Shutdown checklist

- Keep callbacks short and non-blocking.
- Store every returned `TaskId` if you plan to pause, resume, cancel, or inspect a task.
- Use `snapshot()` for runtime diagnostics.
- Always call `close().await` during shutdown.
- Recreate a new `MeowClient` after a successful `close()` if you need to submit more work.
