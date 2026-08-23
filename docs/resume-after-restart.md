# Resume after a process restart

`rusty-cat` does not persist a task database. Your application must persist the
information needed to rebuild a logical transfer, then construct and enqueue a
new task after restart. `TaskId` is process-local and is not a durable identity.

The SDK does write one internal checkpoint: concurrent downloads use a
`<target>.rcdl` sidecar. That file is part of download correctness and must stay
next to the partial target.

## Capability matrix

| Transfer mode | In-process pause/resume | Cross-process resume source | Restart guarantee |
|---|---:|---|---|
| Serial download | Yes | Partial file length | Supported when remote total/representation is still compatible |
| Concurrent download | Yes | `.rcdl` bitmap/digests plus fresh strong ETag from HEAD | Supported with validator requirements below |
| Default HTTP upload | Yes | Server-reported `nextByte` during prepare | Supported if the server persists and authenticates the cursor |
| Presigned multipart upload | Yes | Backend session plus persisted/reconciled completed parts | Supported when the plan/helper is rebuilt with valid session state and fresh URLs |
| Aliyun OSS direct upload | Yes | Current protocol instance | No public checkpoint injection after restart; abort orphan and start a new session |
| Azure Blob direct upload | Yes | Current protocol instance | No public checkpoint injection after restart; reconcile/clean remote blocks and restart |
| Direct/provider download | Yes | Serial file length or concurrent `.rcdl`, depending on parts setting | Same download rules as above |
| Binary download | Cancel only | None | Not resumable |

Do not describe all upload modes as restart-resumable. The protocol must be able
to recover or reconstruct remote session state, not merely recreate the same URL.

## What to persist

Persist application-owned data, not signed secrets:

- stable logical transfer ID;
- direction and protocol/provider kind;
- local path and logical file name;
- upload source identity, total size, and chunk size;
- stable remote object/session identity;
- builder headers or metadata that are safe and still valid;
- desired `max_parts_in_flight`;
- latest status/progress for UI only;
- for presigned multipart, upload ID and completed part metadata needed by the
  provider helper/backend.

Do not persist raw signed URLs longer than necessary. Store a backend request ID
or object key and request fresh URLs. Protect local paths, headers, tokens, and
provider upload IDs according to your threat model.

Write persistence from a bounded application queue rather than blocking an SDK
callback. A progress fraction is not a trusted byte checkpoint; recovery derives
its real offset from the local file, sidecar, or remote protocol.

## Restore workflow

1. Load incomplete application records.
2. Verify the local upload source or download target still belongs to the record.
3. Reconcile remote session state and refresh credentials/URLs.
4. Rebuild the same logical task and protocol.
5. Use `try_enqueue_paused` if the user should decide what restarts, otherwise
   enqueue and await it.
6. Replace the persisted old `TaskId` with the newly returned process-local ID.
7. Call `close().await` only after the intended work reaches a terminal state.

```rust,no_run
use rusty_cat::api::{DownloadPounceBuilder, MeowClient, MeowConfig};

# async fn restore() -> Result<(), rusty_cat::api::MeowError> {
let client = MeowClient::new(MeowConfig::default());
let task = DownloadPounceBuilder::new(
    "archive.bin",
    "./downloads/archive.bin",
    4 * 1024 * 1024,
    "https://example.com/archive.bin",
)
.build();

let task_id = client
    .try_enqueue_paused(
        task,
        |record| println!("{:?} {:.1}%", record.status(), record.progress() * 100.0),
        |_id, _payload| {},
    )
    .await?;
client.resume(task_id).await?;
// In a long-lived task manager, wait for a terminal callback/status before close.
# Ok(())
# }
```

The initial imported `Paused` record has progress `0.0` because no prepare stage
has run. Continue showing your persisted UI progress until resume produces a new
record.

## Serial download

Prepare reads the current target length and resumes from that byte. It obtains
the remote total from a protocol hint, `with_total_size`, or HEAD.

- Local length less than total: resume from local length.
- Local length equal to total: report complete without another range body.
- Local length greater than total: fail with `InvalidRange`; do not truncate or
  silently report success.

The server must honor Range consistently. If the remote object can change under
the same URL, use versioned URLs or application validation; serial length alone
does not prove that the partial prefix and current remote object are one
generation.

## Concurrent download

The target is pre-sized, so length is not progress. The `.rcdl` sidecar stores
durable completed parts and their SHA-256 digests. Cross-process reuse requires a
fresh HEAD response with a strong ETag and the same semantic resource identity,
total size, and chunk size. Completed part digests are rechecked from disk.

Do not set `with_total_size` when cross-process reuse is required: that skips
HEAD, so an old sidecar cannot be authenticated. The first 206 can latch an ETag
for one new run but cannot retroactively prove an earlier process's checkpoint.
Every 206 must carry the same strong ETag. Read the complete
[concurrent download contract](concurrent-chunk-transfer.md).

Changing `max_parts_in_flight` is allowed. Changing chunk size starts a new part
grid. Do not delete or move `.rcdl` independently of its partial target.

## Default HTTP upload

The default protocol sends a prepare request before file bytes. Your server must
return a durable, authenticated `nextByte`; the executor combines that value
with its local offset. After a process restart, the new task begins without an
in-memory offset, so remote prepare state is the source of truth.

Make the upload/session identity stable, bind it to the user and file, validate
the local file has not changed, and make repeated chunk writes idempotent. See
[Default HTTP upload protocol](default-http-upload-protocol.md).

## Presigned multipart upload

Persist the upload/session ID and provider part results, including part number,
offset, size, provider part ID, and ETag where applicable. On restart, ask the
backend for a fresh plan, reconcile client records with the remote session, and
seed the provider helper with only confirmed completed parts. Expired signed
URLs are authorization artifacts, not checkpoint identity.

Completion and abort are explicit plan operations. A plan without
`complete_request` can finish locally without committing a provider object; a
plan without `abort_request` cannot clean remote state on cancel. See
[Presigned lifecycle](presigned-lifecycle.md).

## Direct cloud uploads

The Aliyun OSS direct and Azure Blob direct protocols retain multipart/block
session bookkeeping in the live protocol object and can pause/resume within the
same process. The current public builders do not accept a prior upload session or
confirmed offset after a restart. Recreating the task therefore does not safely
continue the old session.

Persist the Aliyun upload ID if you need out-of-band orphan cleanup via the
provider API, then start a fresh multipart session. For Azure, reconcile and
clean uncommitted blocks according to your storage policy before restarting.
Note that canceling Azure direct upload invokes its abort behavior, which deletes
the target blob; make that policy visible to users.

## Crash and shutdown semantics

A process crash cannot run `close`, completion, or abort hooks. Backends should
expire/reap orphaned sessions and make recovery APIs idempotent. During a normal
shutdown, call `close().await`; unfinished file tasks emit `Paused`, but close is
still terminal for that client. Construct a new client when the application
continues later.

The runnable download example is
[`../examples/resume_after_restart.rs`](../examples/resume_after_restart.rs).
