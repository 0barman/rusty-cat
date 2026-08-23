# Binary download

Use `BinaryTask` for a small HTTP `GET` whose entire response must be retained
in memory, such as configuration, a thumbnail, or a manifest. Use a regular
download task for large bodies, resumability, progress, pause/resume, or file
output.

## Complete example

```rust,no_run
use rusty_cat::api::{
    BinaryDownloadConfig, BinaryTask, MeowClient, MeowConfig,
};

#[tokio::main]
async fn main() -> Result<(), rusty_cat::api::MeowError> {
    let binary = BinaryDownloadConfig::builder()
        .max_body_bytes(2 * 1024 * 1024)
        .redirect_limit(3)
        .build()?;
    let config = MeowConfig::builder()
        .binary_download_config(binary)
        .build()?;
    let client = MeowClient::new(config);

    let task = BinaryTask::new("https://example.com/manifest.json")
        .with_max_body_bytes(512 * 1024);
    let (tx, rx) = tokio::sync::oneshot::channel();
    let task_id = client.try_enqueue_binary_task(task, move |_task_id, result| {
        let _ = tx.send(result);
    })?;

    let output = match rx.await {
        Ok(result) => result?,
        Err(_) => {
            client.close().await?;
            return Err(rusty_cat::api::MeowError::from_code_str(
                rusty_cat::api::InnerErrorCode::CommandResponseFailed,
                "binary callback channel closed",
            ));
        }
    };
    println!("task={task_id}, bytes={}", output.bytes().len());
    if let Some(content_type) = output.content_type() {
        println!("content-type={content_type:?}");
    }
    client.close().await?;
    Ok(())
}
```

The callback is `FnOnce` and can run before `try_enqueue_binary_task` returns.
It must finish in bounded time and must not synchronously wait for `close()` on
the same client, because close drains callbacks.

## Limits and defaults

| Setting | Default | Constraint |
|---|---:|---:|
| Response body | 5 MiB | Client maximum is 64 MiB |
| Per-task body limit | Client limit | May only tighten the client limit |
| Concurrent requests | 2 | Fixed |
| Accepted tasks | 1024 | Includes queued, active, and callback-in-progress tasks |
| Redirects | 5 | Configurable from 0 through 10 |
| Retry delays | 300 ms, 800 ms | At most 8 non-zero delays |
| Request timeout / TCP keepalive | Inherit `MeowConfig` | May be overridden in binary config |

A full binary queue returns `BinaryTaskQueueFull` (123). A declared or streamed
body over the limit returns `BinaryBodyTooLarge` (124).

## HTTP and lifecycle behavior

- Only `http` and `https` URLs without userinfo are accepted.
- Redirect targets containing userinfo are rejected, as are HTTPS-to-HTTP
  downgrades.
- Request transport failures and response-body read failures use the configured
  retry delays. A non-success HTTP status is terminal and is not status-retried.
- Binary tasks have no progress records, persisted checkpoints, file output, or
  scheduler snapshot entries.
- `cancel(task_id)` is supported. `pause` and `resume` are not.
- `close()` cancels remaining binary work and waits for accepted callbacks to
  drain.
