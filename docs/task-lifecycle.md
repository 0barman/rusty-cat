# Task lifecycle

`MeowClient` schedules file transfers on background executors. Submitting a task
and finishing a task are separate events, so shutdown order is part of the API
contract.

## The safest one-task pattern

Use `enqueue_and_wait` when the current operation should not continue until the
transfer reaches `Complete`, `Failed`, or `Canceled`:

```rust,no_run
use rusty_cat::api::{DownloadPounceBuilder, MeowClient, MeowConfig};

#[tokio::main]
async fn main() -> Result<(), rusty_cat::api::MeowError> {
    let client = MeowClient::new(MeowConfig::default());
    let task = DownloadPounceBuilder::new(
        "artifact.bin",
        "./artifact.bin",
        1024 * 1024,
        "https://example.com/artifact.bin",
    )
    .build();

    let result = client
        .enqueue_and_wait(task, |record| {
            println!("{:?} {:.1}%", record.status(), record.progress() * 100.0);
        })
        .await;

    // close is terminal: call it after all work you intend to await.
    client.close().await?;
    result?;
    Ok(())
}
```

Do not call `close()` immediately after `try_enqueue()`. `try_enqueue()` only
admits the task; it does not wait for network I/O. `close()` cancels in-flight
work and reports unfinished file transfers as `Paused` during shutdown.

## Choosing an enqueue API

| API | Starts immediately | Waits for terminal state | Intended use |
|---|---:|---:|---|
| `try_enqueue` | Yes | No | Multiple tasks, UI task lists, external lifecycle management |
| `enqueue_and_wait` | Yes | Yes | CLI jobs, services awaiting one transfer, simple integrations |
| `try_enqueue_paused` | No | No | Restore persisted tasks without network or file I/O |
| `try_enqueue_binary_task` | Yes | Callback only | Small bounded responses; see [Binary download](binary-download.md) |

Both `try_enqueue` variants fail fast with `CommandSendFailed` when the command
queue is full. Size `MeowConfig::command_queue_capacity`, rate-limit producers,
or retry admission with application-level backoff. Control calls such as
`pause`, `resume`, `cancel`, and `snapshot` wait for command-queue capacity.

## Status and callback contract

The progress callback receives `Pending`, `Transmission`, `Paused`, `Complete`,
`Failed`, and `Canceled` records. The completion callback passed to
`try_enqueue` runs only on `Complete`; observe `Failed` and `Canceled` in the
progress callback. `enqueue_and_wait` converts all three terminal outcomes into
one awaitable result.

Callbacks may run on runtime worker threads. Keep them bounded and non-blocking;
send work to your own channel if persistence or UI work can block. See
[Progress, status, and observability](progress-status-observability.md).

## Pause, resume, cancel

- `pause(task_id)` stops scheduling the file task and preserves usable local or
  remote checkpoint state. An active request is canceled cooperatively.
- `resume(task_id)` continues the same in-process task ID.
- `cancel(task_id)` is terminal. Upload cleanup is protocol-specific: direct OSS
  aborts its multipart session, Azure direct deletes the target blob, and a
  presigned upload only sends cleanup when its plan has an `abort_request`.
- Binary tasks support cancellation only; pause and resume return
  `InvalidTaskState`.

`TaskId` identifies a scheduler entry in the current process. Persist the data
needed to rebuild a logical task, not the ID as a cross-process identity.

## Restore tasks without starting them

Rebuild the task, call `try_enqueue_paused`, persist the newly returned
`TaskId`, and later call `resume`. Import emits one `Paused` record with progress
`0.0` because prepare has not yet inspected the local or remote checkpoint.
Display your persisted progress until the first resumed progress update.

Recovery support depends on the protocol; consult
[Resume after restart](resume-after-restart.md) before promising resumability.

## Sharing and shutdown

`MeowClient` is intentionally not `Clone`. Share one scheduler with
`Arc<MeowClient>`:

```rust,no_run
use std::sync::Arc;
use rusty_cat::api::{MeowClient, MeowConfig};

let client = Arc::new(MeowClient::new(MeowConfig::default()));
let worker_client = Arc::clone(&client);
tokio::spawn(async move {
    let _ = worker_client;
});
```

Call `close().await` once no more tasks will be submitted and after awaited work
has reached a terminal state. It cancels unfinished work, drains submitted
callbacks, joins executor threads, and permanently closes the client. A second
close returns `ClientClosed`; create a new client for later work.
