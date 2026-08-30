# Concurrent chunk transfer

`with_max_parts_in_flight(n)` controls how many chunks of one file may run at
the same time. The default is `1`, which keeps a strictly serial transfer. Use a
higher value only for large files and only after measuring the real network,
storage, CPU, and memory behavior of your workload.

This setting is different from `MeowConfig::max_upload_concurrency` and
`max_download_concurrency`: those limit active file groups; the parts setting
limits requests inside one active file.

## Quick start

```rust,no_run
use rusty_cat::api::{DownloadPounceBuilder, MeowClient, MeowConfig};

#[tokio::main]
async fn main() -> Result<(), rusty_cat::api::MeowError> {
    let client = MeowClient::new(MeowConfig::default());
    let task = DownloadPounceBuilder::new(
        "archive.bin",
        "./archive.bin",
        8 * 1024 * 1024,
        "https://example.com/archive.bin",
    )
    // Leave total size unknown so prepare performs HEAD and can bind any
    // existing .rcdl checkpoint to a strong ETag.
    .with_max_parts_in_flight(4)
    .build();

    let result = client.enqueue_and_wait(task, |_| {}).await;
    client.close().await?;
    result?;
    Ok(())
}
```

The protocol must opt into out-of-order parts. The bundled
`StandardRangeDownload` and multipart provider protocols do this where safe.
A custom protocol remains serial until its `supports_parallel_parts()` returns
`true`; read [Custom protocols](custom-protocols.md) before opting in.

## Hard safety limits

Before a parallel run, the executor caps the effective window by the remaining
part grid and rejects either of these conditions with a controlled I/O error:

- more than 256 effective in-flight part tasks;
- more than the client-wide buffered-part budget (512 MiB on 64-bit targets,
  64 MiB on 32-bit targets).

The byte budget is shared by every active parallel file in one client. Uploads
are charged for the body plus verification scratch; downloads are charged for
both the response `Bytes` frame and the destination part buffer that coexist
during the copy. Waiting for a permit is cancellation-aware, and permits are
released on success, failure, cancel, or panic.

The approximate upload buffer budget is `effective_parts * chunk_size`, bounded
by the bytes remaining. Downloads stream response bodies into positioned file
writes but still consume per-request buffers, connection state, and sidecar
metadata. Start with 2–4 parts; increasing `n` can reduce performance when disk,
server throttling, or checkpoint cost is the bottleneck.

Passing `0` normalizes to `1`. A tiny file can run with a large configured
maximum because validation uses the effective part count, not the raw number.

## Upload contract

Parallel upload is safe only when:

- each part number or block ID is a pure function of byte offset;
- the server does not require one sequential next-byte cursor;
- uploading the same offset again overwrites or is otherwise idempotent;
- the final commit happens once, after every in-flight part has completed.

Bundled OSS multipart, Azure Put Block, and corresponding presigned helpers meet
that contract. The executor waits for the full part window to join before
calling `complete_upload` once.

Retries can repeat a part. Completion and abort endpoints should therefore be
idempotent. If a direct or presigned provider session is canceled, cleanup still
depends on that protocol's abort implementation or plan; concurrency does not
create a universal cleanup guarantee.

## Concurrent download server contract

Every range request must return all of the following protocol structure:

- `206 Partial Content`, never `200 OK`;
- a valid `Content-Range` matching the requested start, end, and total;
- a body with exactly the declared range length;
- identity or absent `Content-Encoding`.

Validator requirements depend on the number of responses. When HEAD supplied a
strong ETag, the SDK sends it as `If-Match`, and every 206 must return that same
strong ETag. If HEAD was skipped or returned no strong validator, an old
checkpoint is not reused. A run needing multiple ranges must then obtain a
strong, non-`W/` ETag from the first 206 and require that same value on every
other part. A missing, weak, or changing validator fails closed with
`InvalidRange` so ranges from different object generations are never assembled.
A one-range download with no prepared validator may complete without an ETag
because it cannot combine bytes from different responses.

If a server ignores Range and returns 200, retry the logical download in serial
mode only after removing the parallel checkpoint and partial target. Do not
silently reinterpret a full response as a part.

## The private `.rusty-cat/<hash>.rcdl` sidecar

Concurrent downloads pre-size the target and write parts at absolute offsets.
File length therefore cannot represent progress. The SDK writes
`.rusty-cat/<sha256-of-lossless-file-name>.rcdl` in a private directory adjacent
to the target, containing a part bitmap and SHA-256 digest for each durable
completed part. The former `<target>.rcdl` path is ignored and left untouched;
it may be an ordinary file or the target of another transfer.

The adjacent `.rusty-cat` directory carries an SDK ownership marker. A
file/symlink at the namespace path, or a non-empty unmarked directory, fails
closed without modifying existing entries. An empty directory may be
initialized. This dedicated namespace plus the strong path hash prevents names
such as `foo` and `foo.rcdl` from sharing a target/sidecar path. The target lease
also covers the generated sidecar path, so a task that explicitly targets that
hashed path cannot run concurrently with its owner.

The `.rusty-cat` component is reserved for visible download targets even before
the marker is created. Lexical and resolved/symlink paths through that component
fail closed, removing the marker-claim race in which a visible file could
otherwise be mistaken for a stale checkpoint temp file.

On restart, a sidecar is treated as generation-bound and reused only when all
relevant invariants match:

- target length equals the remote total;
- total size and chunk size match;
- the semantic resource URL, stable effective range-request context, and newly
  observed strong ETag match;
- each remembered completed part still hashes to its stored digest.

Changing `max_parts_in_flight` does not invalidate compatible durable parts.
Changing chunk size does, because it changes the part grid. Corrupt or
mismatched sidecars, or local bytes without a matching sidecar, have no
generation proof and start with an all-missing bitmap rather than trusting stale
bytes or inferring progress from target length. In effect, the object is fetched
again from byte zero. Successful completion validates the full grid and removes
the sidecar.

The sidecar supports at most 1,000,000 parts on 64-bit targets. On 32-bit
targets the limit is 407,779 parts so the resident digest table, checkpoint
clone, encoded snapshot, bitmaps, and pending entries remain within the 64 MiB
checkpoint-memory policy. Choose a chunk size that keeps the part count below
the target architecture's limit; an oversized grid returns a controlled I/O
error before allocation.

## Signed URL and request identity

The SDK removes recognized authentication-only query parameters from AWS,
Google, OSS, legacy presigned, and Azure SAS signatures while preserving
semantic parameters such as object version, snapshot, or response
representation selectors. URL userinfo is reduced to stable principal context.
The canonical URL, ETag, effective invariant headers, and protocol context are
then domain-separated and SHA-256 hashed; the sidecar stores only that fixed-size
digest. Raw URLs, ETags, headers, credentials, and unknown query values are never
persisted in checkpoint identity.

`StandardRangeDownload` and bundled direct-download protocols provide stable
resume context. A custom `BreakpointDownload` must implement
`resume_identity()` completely; its safe default is `None`, which disables old
sidecar reuse while retaining current-run ETag validation. Exclude per-part
`Range`/validator values and short-lived signature timestamps, but include every
tenant, principal, transform, locale, media type, and other selector that can
change returned bytes.

An externally injected `reqwest::Client` may contain default headers that
reqwest cannot expose for canonicalization. Such tasks conservatively disable
cross-process sidecar reuse; make representation headers explicit on the task
and use the built-in client when restart reuse is required.

Use one coherent URL/plan for the requests in a single run. Never persist or log
signed query strings as credentials or task identities.

## HEAD, known sizes, and restart recovery

`with_total_size(...)` and a protocol `total_size_hint()` skip HEAD. They enable
parallel downloads from a GET-only URL, but prepare then has no remote validator
with which to authenticate an old sidecar. Existing cross-process part bits are
deliberately not reused; the first 206 only establishes consistency for the
current run. If the object needs multiple ranges, that first 206 must contain a
strong ETag; if the object fits in one range, no cross-response validator is
required.

For cross-process concurrent resume with the current public API:

1. Leave builder total size at zero.
2. Ensure the protocol HEAD URL works.
3. Return positive `Content-Length`, a strong ETag, and identity content encoding.
4. Return that same strong ETag on every 206 response.

If the signed GET URL cannot be used for HEAD, implement a download protocol
whose `head_url` and HEAD headers use a separate authorized metadata URL. A
size-only hint is not enough to prove object generation.

## Pause, cancel, and mode changes

Pause drains/cancels the active window and keeps durable checkpoint state.
Resume schedules missing parts again. Cancel is terminal but a partial file or
sidecar can still exist on disk; remove them according to your product's data
retention policy.

Switching a compatible target from parallel to serial mode is safe: serial
prepare validates the stored digests, keeps only the longest contiguous prefix,
truncates the pre-sized target to that prefix, and atomically discards higher
sidecar bits. An incompatible or unauthenticated sidecar starts from zero.
Ensure no other process is using the target; the SDK also acquires a path lease
and an actual-target file lock to prevent cooperating concurrent writers.

On Windows, the active target handle intentionally omits delete sharing. The OS
therefore rejects deleting, renaming, or replacing the target path while the
download is active, including replacement with identical content. This is the
safer platform boundary: after a terminal result releases the handle, the path
may be replaced normally. Platforms that permit an active rename still rely on
the final visible-content validation to reject different bytes.

## Retry behavior

Chunk retry applies to connection errors and HTTP 408, 429, and 500–599. Other
HTTP status failures are terminal. Prepare-stage outer retry is limited to
connection-layer `HttpError`. Range validation errors are terminal because
retrying an incompatible representation does not make the assembled file safe.

See [Error codes](error-codes.md), [Task lifecycle](task-lifecycle.md), and
[Resume after restart](resume-after-restart.md) for integration-level handling.
