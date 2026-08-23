# Error codes reference

Most `rusty-cat` APIs return `Result<_, MeowError>`. A `MeowError` carries:

- `code() -> i32` — a **stable numeric code** taken from `InnerErrorCode`. Because
  it is a plain integer, it is convenient for FFI boundaries, structured logging,
  and metrics.
- `msg() -> &str` — a human-readable message (may include context such as the URL
  or offset). Do not parse it programmatically; branch on `code()` instead.
- `http_status() -> Option<u16>` — the captured HTTP response status when the
  SDK could associate one with the error.

```rust,no_run
use rusty_cat::api::{InnerErrorCode, MeowError};

fn is_retryable_backpressure(err: &MeowError) -> bool {
    err.code() == InnerErrorCode::CommandSendFailed as i32
}
```

## Sentinels

| Code | `InnerErrorCode` | Meaning |
|---:|---|---|
| `-1` | `Unknown` | Unclassified error. Treat as a generic failure and inspect `msg()`. |
| `0` | `Success` | Non-error sentinel. Not returned inside an `Err`. |

## Error codes

| Code | `InnerErrorCode` | Meaning | Typical trigger | Suggested handling |
|---:|---|---|---|---|
| `101` | `RuntimeCreationFailedError` | The internal scheduler runtime could not be created. | Process is out of threads/handles, or the host is in a degraded state. | Surface as fatal; the client cannot operate. |
| `102` | `ParameterEmpty` | A required parameter is empty or invalid. | Empty URL or file name at enqueue, or a zero-byte upload. | Validate task inputs before enqueue; fix the offending field. |
| `103` | `DuplicateTaskError` | The same file/task is already queued, running, or paused. | Enqueuing a task whose dedupe key (upload = whole-file MD5; download = your `client_file_sign`/derived key) matches a live task. | Reuse the existing `TaskId`, or `cancel(...)` the old one first. |
| `104` | `EnqueueError` | The task could not be enqueued. | Internal enqueue path failed after validation. | Retry; if it persists, capture logs and report. |
| `105` | `IoError` | A local I/O operation failed. | Reading the upload source or writing the download target failed. | Check the path, permissions, and free space; see also `121`/`122`. |
| `106` | `HttpError` | An HTTP request/response operation failed at the transport layer. | DNS failure, connection reset, TLS error, timeout. | Often transient — the SDK retries these within the retry budget; persistent failures indicate a network/endpoint problem. |
| `107` | `ClientClosed` | The client has been closed and can no longer accept operations. | Calling any API after `close()`. | Create a new `MeowClient`; a closed client cannot be reopened. |
| `108` | `TaskNotFound` | An unknown `TaskId` was used in a control API. | `pause`/`resume`/`cancel` on a task that never existed or is already terminal (completed/canceled). | Verify the `TaskId`; treat terminal tasks as gone. |
| `109` | `ResponseStatusError` | The HTTP response status was not the expected success status. | Server returned `4xx`/`5xx` (for example `403` on an expired presigned URL). | Inspect `http_status()` and `msg()`. Chunk requests retry 408, 429, and 5xx; other known statuses fail immediately. |
| `110` | `MissingOrInvalidContentLengthFromHead` | `Content-Length` from the download `HEAD` is missing or invalid. | The server does not return a usable size and no size hint was provided. | Provide a size hint (for example presigned `with_total_size(...)`) or fix the server. |
| `111` | `CommandSendFailed` | A command could not be sent to the scheduler (the command queue is full). | Fail-fast back-pressure: too many concurrent enqueue/control calls. | Retry with your own backoff, or raise `command_queue_capacity`. |
| `112` | `CommandResponseFailed` | The command response channel closed unexpectedly. | The scheduler was torn down while a command was in flight. | Usually happens during shutdown; re-check `is_closed()`. |
| `113` | `ResponseParseError` | A response payload could not be parsed. | Malformed JSON in an upload prepare/completion response. | Fix the server contract; see the upload protocol docs. |
| `114` | `InvalidRange` | Invalid HTTP range semantics or headers. | Local file larger than the remote size, or a presigned part offset/size that does not match the plan. | Verify chunk plan, offsets, and that the partial file is not corrupt. |
| `115` | `FileNotFound` | A required local file does not exist. | The upload source path is missing at enqueue. | Confirm the path before rebuilding the task. |
| `116` | `ChecksumMismatch` | A file checksum/signature did not match the expected value. | Integrity verification failed. | Re-fetch/re-upload the affected data. |
| `117` | `InvalidTaskState` | The current task state does not allow the requested operation. | Resuming a task that is not paused, or double-resuming. | Check task state first; treat as a no-op or surface to the user. |
| `118` | `LockPoisoned` | An internal lock was poisoned (a thread panicked while holding it). | A previous panic corrupted shared state. | Treat as fatal for that client; recreate it. |
| `119` | `HttpClientBuildFailed` | The internal `reqwest::Client` could not be built. | Invalid timeout/keepalive values, or a TLS backend problem. | Fix the config values, or inject a custom `http_client(...)`. |
| `120` | `TaskCanceled` | The task was canceled before reaching `Complete`. | `cancel(...)` was called (or cancellation propagated). | Treat as terminal; create a new task if you want to retry. |
| `121` | `DiskFull` | The local disk ran out of space (`ENOSPC` / `ERROR_DISK_FULL`). | Writing a download chunk failed because the volume is full. | Free space and resume; the partial file is preserved for resume. |
| `122` | `LocalFileRemoved` | The local source/target file was removed or replaced during a transfer. | The user deleted the file mid-download, or the upload source vanished. | Stop the task; re-create the file or re-select the source before resuming. |
| `123` | `BinaryTaskQueueFull` | The bounded binary-task capacity is exhausted. | 1024 binary tasks are queued, active, or waiting for callbacks. | Wait for accepted callbacks to drain, then retry admission. |
| `124` | `BinaryBodyTooLarge` | A binary response exceeded its in-memory limit. | `Content-Length` or streamed bytes exceed the task/client cap. | Use a regular file download or raise the bounded limit, never above 64 MiB. |

## Notes

- Codes are stable identifiers; their **numeric values do not change** across patch
  releases, so they are safe to hard-code at integration boundaries.
- File-transfer chunk retry covers `106` (`HttpError`) and response statuses
  408, 429, and 500–599 within `max_chunk_retries`. The prepare-stage outer
  retry only covers connection-layer `HttpError`; it does not retry prepare
  response statuses.
- Binary tasks use their own retry schedule and retry request-transport/body-read
  failures, not non-success HTTP statuses. See [Binary download](binary-download.md).
- For restart/crash recovery semantics per error (for example resuming after
  `121`/`122`), see [Resume after a process restart](resume-after-restart.md).
