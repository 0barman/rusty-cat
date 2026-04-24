#[path = "dev_server/mod.rs"]
mod dev_server;

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusty_cat::down_pounce_builder::DownloadPounceBuilder;
use rusty_cat::error::InnerErrorCode;
use rusty_cat::file_transfer_record::FileTransferRecord;
use rusty_cat::meow_config::MeowConfig;
use rusty_cat::transfer_status::TransferStatus;
use rusty_cat::MeowClient;

fn temp_download_path(case: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    p.push(format!("rusty_cat_control_race_{case}_{ts}.bin"));
    p
}

fn assert_code_in(err: &rusty_cat::MeowError, allowed: &[InnerErrorCode]) {
    assert!(
        allowed.iter().any(|c| c.clone() as i32 == err.code()),
        "unexpected error code={} msg={}",
        err.code(),
        err.msg()
    );
}

#[tokio::test]
async fn concurrent_pause_resume_cancel_on_same_task_do_not_break_state_machine() {
    let payload = b"control-race-same-task".repeat(1024 * 128);
    let server = dev_server::DevFileServer::spawn(payload);
    let client = Arc::new(MeowClient::new(MeowConfig::new(1, 1)));
    let path = temp_download_path("same_task");

    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();

    let task = DownloadPounceBuilder::new(
        "race.bin",
        &path,
        1024,
        format!("{}/download/race.bin", server.base_url()),
    )
    .build();

    let task_id = client
        .try_enqueue(
            task,
            move |record: FileTransferRecord| {
                statuses_cb
                    .lock()
                    .expect("lock statuses")
                    .push(record.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("enqueue race task");

    for _ in 0..100 {
        if statuses
            .lock()
            .expect("lock statuses")
            .iter()
            .any(|s| matches!(s, TransferStatus::Transmission))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let mut handles = Vec::new();
    for _ in 0..8 {
        let c = client.clone();
        handles.push(tokio::spawn(async move {
            let _ = c.pause(task_id).await;
        }));
        let c = client.clone();
        handles.push(tokio::spawn(async move {
            let _ = c.resume(task_id).await;
        }));
        let c = client.clone();
        handles.push(tokio::spawn(async move {
            let _ = c.cancel(task_id).await;
        }));
    }
    for h in handles {
        h.await.expect("join control race op");
    }

    tokio::time::sleep(Duration::from_millis(200)).await;

    let observed = statuses.lock().expect("lock statuses final").clone();
    assert!(
        observed.iter().any(|s| {
            matches!(
                s,
                TransferStatus::Canceled | TransferStatus::Complete | TransferStatus::Failed(_)
            )
        }),
        "task should converge to terminal status under control races"
    );

    client.close().await.expect("close client");
    server.shutdown();
    let _ = fs::remove_file(&path);
}

#[tokio::test]
async fn close_racing_with_enqueue_pause_resume_is_semantically_stable() {
    let payload = b"control-race-close".repeat(4096);
    let server = dev_server::DevFileServer::spawn(payload);
    let client = Arc::new(MeowClient::new(MeowConfig::new(0, 0)));
    let path = temp_download_path("close_race_anchor");

    let anchor = DownloadPounceBuilder::new(
        "anchor.bin",
        &path,
        1024,
        format!("{}/download/anchor.bin", server.base_url()),
    )
    .build();

    let task_id = client
        .try_enqueue(anchor, |_r: FileTransferRecord| {}, |_, _| {})
        .await
        .expect("enqueue anchor");

    let c_close = client.clone();
    let h_close = tokio::spawn(async move { c_close.close().await });

    let c_pause = client.clone();
    let h_pause = tokio::spawn(async move { c_pause.pause(task_id).await });

    let c_resume = client.clone();
    let h_resume = tokio::spawn(async move { c_resume.resume(task_id).await });

    let c_enqueue = client.clone();
    let server_url = server.base_url().to_string();
    let enqueue_path = temp_download_path("close_race_enqueue");
    let h_enqueue = tokio::spawn(async move {
        let t = DownloadPounceBuilder::new(
            "late.bin",
            &enqueue_path,
            1024,
            format!("{server_url}/download/late.bin"),
        )
        .build();
        let r = c_enqueue
            .try_enqueue(t, |_r: FileTransferRecord| {}, |_, _| {})
            .await;
        (r, enqueue_path)
    });

    let close_ret = h_close.await.expect("join close");
    match close_ret {
        Ok(()) => {}
        Err(e) => assert_code_in(&e, &[InnerErrorCode::ClientClosed]),
    }

    let pause_ret = h_pause.await.expect("join pause");
    if let Err(e) = pause_ret {
        assert_code_in(
            &e,
            &[
                InnerErrorCode::ClientClosed,
                InnerErrorCode::TaskNotFound,
                InnerErrorCode::InvalidTaskState,
            ],
        );
    }

    let resume_ret = h_resume.await.expect("join resume");
    if let Err(e) = resume_ret {
        assert_code_in(
            &e,
            &[
                InnerErrorCode::ClientClosed,
                InnerErrorCode::TaskNotFound,
                InnerErrorCode::InvalidTaskState,
            ],
        );
    }

    let (enqueue_ret, enqueue_path) = h_enqueue.await.expect("join enqueue");
    if let Err(e) = enqueue_ret {
        assert_code_in(
            &e,
            &[
                InnerErrorCode::ClientClosed,
                InnerErrorCode::CommandSendFailed,
            ],
        );
    }

    assert!(
        client.is_closed().await,
        "client should be closed after race"
    );

    let post_close_err = client
        .snapshot()
        .await
        .expect_err("snapshot after close should fail");
    assert_eq!(post_close_err.code(), InnerErrorCode::ClientClosed as i32);

    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(&enqueue_path);
    server.shutdown();
}

#[tokio::test]
async fn pause_resume_cancel_right_after_completion_have_stable_errors() {
    let payload = b"terminal-then-control".repeat(128);
    let server = dev_server::DevFileServer::spawn(payload.clone());
    let client = MeowClient::new(MeowConfig::new(1, 1));
    let path = temp_download_path("post_terminal");

    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();

    let task = DownloadPounceBuilder::new(
        "done.bin",
        &path,
        payload.len() as u64,
        format!("{}/download/done.bin", server.base_url()),
    )
    .build();
    let task_id = client
        .try_enqueue(
            task,
            move |record: FileTransferRecord| {
                statuses_cb
                    .lock()
                    .expect("lock statuses")
                    .push(record.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("enqueue terminal task");

    let mut terminal: Option<TransferStatus> = None;
    for _ in 0..300 {
        terminal = statuses
            .lock()
            .expect("lock statuses final")
            .iter()
            .rev()
            .find(|s| {
                matches!(
                    s,
                    TransferStatus::Complete | TransferStatus::Failed(_) | TransferStatus::Canceled
                )
            })
            .cloned();
        if terminal.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        matches!(terminal, Some(TransferStatus::Complete)),
        "task should complete before control commands"
    );

    let pause_err = client.pause(task_id).await.expect_err("pause after done");
    assert_code_in(
        &pause_err,
        &[
            InnerErrorCode::TaskNotFound,
            InnerErrorCode::InvalidTaskState,
        ],
    );

    let resume_err = client.resume(task_id).await.expect_err("resume after done");
    assert_code_in(
        &resume_err,
        &[
            InnerErrorCode::TaskNotFound,
            InnerErrorCode::InvalidTaskState,
        ],
    );

    let cancel_err = client.cancel(task_id).await.expect_err("cancel after done");
    assert_code_in(
        &cancel_err,
        &[
            InnerErrorCode::TaskNotFound,
            InnerErrorCode::InvalidTaskState,
        ],
    );

    client.close().await.expect("close client");
    server.shutdown();
    let _ = fs::remove_file(&path);
}
