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
- more than 512 MiB in the effective chunk window.

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

Every range request must return all of the following:

- `206 Partial Content`, never `200 OK`;
- a valid `Content-Range` matching the requested start, end, and total;
- a body with exactly the declared range length;
- identity or absent `Content-Encoding`;
- a strong ETag, not a weak `W/` validator.

All part responses must carry the same strong ETag. When HEAD supplied an ETag,
the SDK sends it as `If-Match` and compares every response. If HEAD was skipped,
the first 206 latches its strong ETag for the current run and every other part
must match it. A missing, weak, or changing ETag fails closed with
`InvalidRange` so ranges from different object generations are never assembled.

If a server ignores Range and returns 200, retry the logical download in serial
mode only after removing the parallel checkpoint and partial target. Do not
silently reinterpret a full response as a part.

## The `.rcdl` sidecar

Concurrent downloads pre-size the target and write parts at absolute offsets.
File length therefore cannot represent progress. The SDK writes
`<target>.rcdl`, containing a part bitmap and SHA-256 digest for each durable
completed part.

On restart, a sidecar is reused only when all relevant invariants match:

- target length equals the remote total;
- total size and chunk size match;
- the semantic resource URL plus newly observed strong ETag match;
- each remembered completed part still hashes to its stored digest.

Changing `max_parts_in_flight` does not invalidate compatible durable parts.
Changing chunk size does, because it changes the part grid. Corrupt or
mismatched sidecars start with an all-missing bitmap rather than trusting stale
bits. Successful completion validates the full grid and removes the sidecar.

The sidecar supports at most 1,000,000 parts. Choose a chunk size that keeps the
part count below that limit.

## Signed URL identity

The persisted identity removes recognized authentication-only query parameters
from AWS, Google, OSS, legacy presigned, and Azure SAS signatures, while
preserving semantic parameters such as object version, snapshot, or response
representation selectors. Therefore a refreshed signed URL can reuse a sidecar
when it names the same semantic resource and HEAD observes the same strong ETag.

Use one coherent URL/plan for the requests in a single run. Never persist or log
signed query strings as credentials or task identities.

## HEAD, known sizes, and restart recovery

`with_total_size(...)` and a protocol `total_size_hint()` skip HEAD. They enable
parallel downloads from a GET-only URL, but prepare then has no remote validator
with which to authenticate an old sidecar. Existing cross-process part bits are
deliberately not reused; the first 206 only establishes consistency for the
current run.

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

Do not switch a target containing `.rcdl` to serial mode. Serial prepare rejects
that situation because the pre-sized target length would otherwise look
complete. To abandon the parallel transfer, delete both the sidecar and partial
file before starting serially. Ensure no other process is using the target; the
SDK also acquires a target lease to prevent concurrent writers.

## Retry behavior

Chunk retry applies to connection errors and HTTP 408, 429, and 500–599. Other
HTTP status failures are terminal. Prepare-stage outer retry is limited to
connection-layer `HttpError`. Range validation errors are terminal because
retrying an incompatible representation does not make the assembled file safe.

See [Error codes](error-codes.md), [Task lifecycle](task-lifecycle.md), and
[Resume after restart](resume-after-restart.md) for integration-level handling.
