use rusty_cat::down_pounce_builder::DownloadPounceBuilder;
use rusty_cat::error::InnerErrorCode;
use rusty_cat::file_transfer_record::FileTransferRecord;
use rusty_cat::meow_config::MeowConfig;
use rusty_cat::MeowClient;

#[tokio::test]
async fn cancel_unknown_task_returns_task_not_found() {
    // 先创建一个真实 task_id，避免依赖内部构造函数。
    let client = MeowClient::new(MeowConfig::new(1, 1));
    let task = DownloadPounceBuilder::new(
        "task-not-found.bin",
        std::env::temp_dir().join("task-not-found.bin"),
        1024,
        "http://127.0.0.1:9/not-used",
    )
    .build();
    let task_id = client
        .try_enqueue(task, |_record: FileTransferRecord| {}, |_, _| {})
        .await
        .expect("enqueue");

    // 第一次 cancel 应该命中真实任务并成功。
    client
        .cancel(task_id)
        .await
        .expect("first cancel should succeed");

    // 第二次 cancel 针对同一 id 时，映射已删除，应返回 TaskNotFound。
    let err = client
        .cancel(task_id)
        .await
        .expect_err("second cancel should fail with task not found");
    assert_eq!(err.code(), InnerErrorCode::TaskNotFound as i32);

    client.close().await.expect("close client");
}

#[tokio::test]
async fn pause_unknown_task_returns_task_not_found() {
    // 先创建一个真实 task_id，后续通过 cancel 使其变成未知任务。
    let client = MeowClient::new(MeowConfig::new(1, 1));
    let task = DownloadPounceBuilder::new(
        "task-not-found-pause.bin",
        std::env::temp_dir().join("task-not-found-pause.bin"),
        1024,
        "http://127.0.0.1:9/not-used",
    )
    .build();
    let task_id = client
        .try_enqueue(task, |_record: FileTransferRecord| {}, |_, _| {})
        .await
        .expect("enqueue");

    // 先取消，使该 id 不再可控。
    client.cancel(task_id).await.expect("cancel should succeed");

    // 再 pause 同一个 id，应该返回明确的 TaskNotFound。
    let err = client
        .pause(task_id)
        .await
        .expect_err("pause unknown task should fail");
    assert_eq!(err.code(), InnerErrorCode::TaskNotFound as i32);

    client.close().await.expect("close client");
}

#[tokio::test]
async fn resume_unknown_task_returns_task_not_found() {
    // 先创建一个真实 task_id，再通过 cancel 让它进入“未知”状态。
    let client = MeowClient::new(MeowConfig::new(1, 1));
    let task = DownloadPounceBuilder::new(
        "task-not-found-resume.bin",
        std::env::temp_dir().join("task-not-found-resume.bin"),
        1024,
        "http://127.0.0.1:9/not-used",
    )
    .build();
    let task_id = client
        .try_enqueue(task, |_record: FileTransferRecord| {}, |_, _| {})
        .await
        .expect("enqueue");

    // 先取消后再恢复，恢复应返回 TaskNotFound。
    client.cancel(task_id).await.expect("cancel should succeed");
    let err = client
        .resume(task_id)
        .await
        .expect_err("resume unknown task should fail");
    assert_eq!(err.code(), InnerErrorCode::TaskNotFound as i32);

    client.close().await.expect("close client");
}
