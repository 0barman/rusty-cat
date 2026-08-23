# Progress, status, and observability

Use per-task callbacks for product state, global listeners for cross-task
telemetry, snapshots for scheduler gauges, and the debug log listener for
diagnostics.

## Progress records

Each `FileTransferRecord` exposes:

| Field | Meaning |
|---|---|
| `task_id()` | Current-process scheduler ID |
| `file_sign()` | Upload file signature when available |
| `file_name()` | Logical file name |
| `total_size()` | Declared or prepared byte size |
| `progress()` | Fraction from `0.0` through `1.0` |
| `status()` | Current `TransferStatus` |
| `direction()` | Upload or download |

The stable status representation is:

| Integer | Status | Terminal |
|---:|---|---:|
| -1 | `None` | No |
| 0 | `Pending` | No |
| 1 | `Transmission` | No |
| 2 | `Paused` | No |
| 3 | `Complete` | Yes |
| 4 | `Failed(MeowError)` | Yes |
| 5 | `Canceled` | Yes |

The `complete_cb` passed to `try_enqueue` only reports `Complete`. Persist
`Failed` and `Canceled` from the progress callback, or use `enqueue_and_wait` to
receive one result for every terminal outcome.

## Global listeners

Register one listener for all file-transfer records and unregister it by ID:

```rust,no_run
use rusty_cat::api::{MeowClient, MeowConfig};

let client = MeowClient::new(MeowConfig::default());
let listener_id = client.register_global_progress_listener(|record| {
    println!("task={} status={:?}", record.task_id(), record.status());
})?;
let removed = client.unregister_global_progress_listener(listener_id)?;
assert!(removed);
# Ok::<(), rusty_cat::api::MeowError>(())
```

Callbacks run in runtime worker context and may overlap. Keep them fast,
thread-safe, and non-blocking. Forward records to a bounded application channel
for database, UI, or network work; define what happens when that channel is full.

## Scheduler snapshots

`client.snapshot().await?` returns `queued_groups`, `active_groups`, and
`active_keys` (`Direction` plus scheduling key). It is a point-in-time diagnostic
view, not a persistence format. It excludes binary tasks and does not expose
individual in-flight parts.

## Debug logs and errors

Install a process-wide debug log listener through
`client.set_debug_log_listener(...)` or the free log functions. Logs include a
level, tag, message, and optional structured context. URLs and known secrets are
sanitized by SDK logging paths, but application callbacks must apply their own
redaction policy.

Use `MeowError::code()` for stable branching, `msg()` for diagnostics, and
`http_status()` when a response status was captured. See
[Error codes](error-codes.md).

Recommended metrics are active/queued groups, terminal outcomes by code,
transferred bytes or progress deltas, operation latency, retry counts from logs,
and command admission failures. Avoid high-cardinality labels such as raw URL,
file name, signed query string, or `TaskId`.
