# Resume after a process restart

`rusty-cat` does not persist a task database. Your application must persist the
information needed to rebuild a logical transfer, then construct and enqueue a
new task after restart. `TaskId` is process-local and is not a durable identity.

The SDK does write one internal checkpoint: serial and concurrent downloads use
a losslessly path-derived `.rusty-cat/<sha256-of-file-name>.rcdl` sidecar in a
private namespace adjacent to the target. That file is part of download
correctness and must stay with the partial target. A checkpoint is reusable across processes only
when its semantic resource, total, chunk grid, freshly observed strong ETag,
and stored local part digests all validate. It is a generation-bound checkpoint,
not proof supplied by path or file length alone.

## Capability matrix

| Transfer mode | In-process pause/resume | Cross-process resume source | Restart guarantee |
|---|---:|---|---|
| Serial download | Yes | `.rcdl` bitmap/digests plus fresh strong ETag from HEAD | Supported with validator requirements below |
| Concurrent download | Yes | `.rcdl` bitmap/digests plus fresh strong ETag from HEAD | Supported with validator requirements below |
| Default HTTP upload | Yes | Server-reported `nextByte` during prepare | Supported if the server persists and authenticates the cursor |
| Presigned multipart upload | Yes | Backend session plus persisted/reconciled completed parts | Supported when the plan/helper is rebuilt with valid session state and fresh URLs |
| Aliyun OSS direct upload | Yes | Current protocol instance | No public checkpoint injection after restart; abort orphan and start a new session |
| Azure Blob direct upload | Yes | Current protocol instance | No public checkpoint injection after restart; reconcile/clean remote blocks and restart |
| Direct/provider download | Yes | Digest-backed `.rcdl`, subject to the protocol's validator support | Same download rules as above |
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
its real offset from a validated sidecar or remote protocol.

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

Prepare obtains the remote total from a protocol hint, `with_total_size`, or
HEAD, then resumes only the longest contiguous prefix whose generation-bound
sidecar identity and SHA-256 part digests validate. A legacy local file, missing
sidecar, or sidecar that cannot be bound to the freshly observed generation is
not trusted by length, even if its length equals the remote total; it is
truncated to zero and fetched safely. A local length greater than the remote
total fails with `InvalidRange`.

Cross-process resume requires a fresh strong ETag from HEAD. If HEAD is skipped
or has no strong validator, the old sidecar is deliberately not reused. If HEAD
did provide one, every 206 must return the same value. Otherwise the first 206
can latch a strong ETag for the current run, and every later range must match it.
A serial run that needs multiple ranges fails closed on a missing, weak, or
changing ETag; a one-range run may complete without one because it cannot join
bytes from different responses.

## Concurrent download

The target is pre-sized, so length is not progress. The `.rcdl` sidecar stores
durable completed parts and their SHA-256 digests. Cross-process reuse requires a
fresh HEAD response with a strong ETag and the same semantic resource identity,
stable range-request context, total size, and chunk size. The binding material
is persisted only as a SHA-256 digest, and completed part digests are rechecked
from disk. Custom download protocols that keep the default
`resume_identity() -> None`, and tasks using an externally injected HTTP client,
validate the current run but do not reuse an earlier process's bits.

Do not set `with_total_size` when cross-process reuse is required: that skips
HEAD, so an old sidecar cannot be authenticated. The first 206 can latch an ETag
for one new run but cannot retroactively prove an earlier process's checkpoint.
Every 206 in a multi-range run must carry the same strong ETag. A parallel
configuration whose entire object is one range may complete without an ETag;
there is no cross-response generation-mixing risk. Read the complete
[concurrent download contract](concurrent-chunk-transfer.md).

Changing `max_parts_in_flight` is allowed. Changing chunk size starts a new part
grid. Do not edit the `.rusty-cat` namespace independently of its partial
targets. The SDK leaves its zero-length ownership marker in that directory
after completed sidecars are removed.

Every `.rusty-cat` path component is reserved even before an ownership marker
exists. A path inside it cannot itself be used as a visible download target,
including through a symlink alias. The task fails before target creation so it
cannot race namespace ownership or overwrite checkpoint/atomic temp files.

The pre-0.3.6 `<target>.rcdl` location is not migrated automatically. It may be
an ordinary file (including the real target of a separate transfer), so 0.3.6
never reads, overwrites, or deletes it and starts with fresh checkpoint state.
A file/symlink at the reserved `.rusty-cat` path, or a non-empty directory there
without the SDK ownership marker, fails closed and remains unchanged. An empty
directory may be initialized as the checkpoint namespace.

## Active download target path

The SDK holds a normalized path lease and one locked target handle throughout an
active download. On Windows that handle deliberately does not grant delete
sharing, so Windows rejects deleting, renaming, or replacing the target path
until the task reaches a terminal state and releases the handle. This includes
an identical-content replacement and is a safety boundary, not a claim that the
physical file is part of checkpoint identity. After terminal release, the path
can be replaced normally. On platforms that permit an active rename, final
content validation still rejects a different-content replacement.

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

The deterministic restart scenarios live in
[`test-app/src/scenarios/local.rs`](../../test-app/src/scenarios/local.rs). Run
the checkpoint-backed recovery scenario from the repository root. Its first
client leaves a validated checkpoint after an injected range failure; a new
client then fetches only the missing part:

```text
cargo run --manifest-path test-app/Cargo.toml -- resume-restart
```

For paused import followed by selective resume, run:

```text
cargo run --manifest-path test-app/Cargo.toml -- restore-paused
```
