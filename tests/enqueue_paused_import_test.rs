//! Integration tests for `MeowClient::try_enqueue_paused`: paused import +
//! selective start.
//!
//! Design doc: `product_doc/rusty-cat_restore_import_design_2026-06-13.md` (phase 1).
//!
//! Coverage:
//! - Happy path: paused import does **zero I/O**, `snapshot` is idle, `resume`
//!   drives a download/upload to completion, selective start of a subset, and
//!   resume continuing from an on-disk partial file.
//! - **Exceptions / improper usage** (the focus): import after close, invalid
//!   params (empty URL/name, zero-byte upload), duplicate imports, paused import
//!   vs active enqueue collisions, cancel-then-resume on a paused import, double
//!   resume, re-pausing a paused import, command-queue-full `CommandSendFailed`,
//!   and closing with unresumed paused imports.

#[path = "dev_server/mod.rs"]
mod dev_server;

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusty_cat::down_pounce_builder::DownloadPounceBuilder;
use rusty_cat::error::InnerErrorCode;
use rusty_cat::file_transfer_record::FileTransferRecord;
use rusty_cat::meow_config::MeowConfig;
use rusty_cat::transfer_status::TransferStatus;
use rusty_cat::up_pounce_builder::UploadPounceBuilder;
use rusty_cat::MeowClient;

// ----------------------------- helpers -----------------------------

fn temp_path(case: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    p.push(format!("rusty_cat_enqueue_paused_{case}_{ts}.bin"));
    p
}

fn single_dl_client() -> MeowClient {
    MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(1)
            .build()
            .expect("valid config"),
    )
}

/// Polls until `pred` holds; returns false on timeout (caller asserts, so the
/// test fails loudly instead of hanging silently).
async fn wait_until<F: FnMut() -> bool>(timeout_ms: u64, mut pred: F) -> bool {
    let mut waited = 0u64;
    while waited < timeout_ms {
        if pred() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        waited += 20;
    }
    false
}

fn has_status(statuses: &Arc<Mutex<Vec<TransferStatus>>>, want: &TransferStatus) -> bool {
    statuses
        .lock()
        .expect("lock statuses")
        .iter()
        .any(|s| std::mem::discriminant(s) == std::mem::discriminant(want))
}

fn count_status(statuses: &Arc<Mutex<Vec<TransferStatus>>>, want: &TransferStatus) -> usize {
    statuses
        .lock()
        .expect("lock statuses")
        .iter()
        .filter(|s| std::mem::discriminant(*s) == std::mem::discriminant(want))
        .count()
}

async fn wait_terminal(statuses: &Arc<Mutex<Vec<TransferStatus>>>, timeout_ms: u64) -> Option<TransferStatus> {
    let mut waited = 0u64;
    while waited < timeout_ms {
        let found = statuses
            .lock()
            .expect("lock statuses")
            .iter()
            .rev()
            .find(|s| {
                matches!(
                    s,
                    TransferStatus::Complete | TransferStatus::Failed(_) | TransferStatus::Canceled
                )
            })
            .cloned();
        if found.is_some() {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        waited += 20;
    }
    None
}

// ============================ happy path ============================

#[tokio::test]
async fn enqueue_paused_does_no_io_and_snapshot_is_idle() {
    // Paused import must issue no HTTP / file write, snapshot must show empty
    // queue and zero active groups, and the callback should observe exactly one
    // Paused (no Pending/Transmission/Complete).
    let payload = b"paused-import-no-io".repeat(512);
    let server = dev_server::DevFileServer::spawn(payload);
    let client = single_dl_client();
    let path = temp_path("no_io");

    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();

    let task = DownloadPounceBuilder::new(
        "no_io.bin",
        &path,
        1024,
        format!("{}/download/no_io.bin", server.base_url()),
    )
    .build();

    let _task_id = client
        .try_enqueue_paused(
            task,
            move |r: FileTransferRecord| {
                statuses_cb.lock().expect("lock").push(r.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("enqueue_paused should succeed");

    // Wait for the first Paused event.
    assert!(
        wait_until(2000, || has_status(&statuses, &TransferStatus::Paused)).await,
        "should observe a Paused status after paused import"
    );
    // Wait a bit more to ensure no further scheduling happens.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Key assertions: no Pending / Transmission / Complete.
    assert!(
        !has_status(&statuses, &TransferStatus::Pending),
        "paused import must not emit Pending"
    );
    assert!(
        !has_status(&statuses, &TransferStatus::Transmission),
        "paused import must not start transferring"
    );
    assert!(
        !has_status(&statuses, &TransferStatus::Complete),
        "paused import must not complete on its own"
    );
    // No real download happened, so the target file must not be created.
    assert!(!path.exists(), "no file should be written before resume");

    // Snapshot must be idle (neither queued nor active).
    let snap = client.snapshot().await.expect("snapshot");
    assert_eq!(snap.queued_groups, 0, "paused import must not be queued");
    assert_eq!(snap.active_groups, 0, "paused import must not be active");

    client.close().await.expect("close");
    server.shutdown();
    let _ = fs::remove_file(&path);
}

#[tokio::test]
async fn enqueue_paused_then_resume_completes_download() {
    // After a paused import, resume must drive the download to completion with
    // correct file contents.
    let payload = b"resume-after-paused-import-abcdefghij".repeat(256);
    let server = dev_server::DevFileServer::spawn(payload.clone());
    let client = single_dl_client();
    let path = temp_path("resume_complete");

    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();

    let task = DownloadPounceBuilder::new(
        "resume.bin",
        &path,
        2048,
        format!("{}/download/resume.bin", server.base_url()),
    )
    .build();

    let task_id = client
        .try_enqueue_paused(
            task,
            move |r: FileTransferRecord| {
                statuses_cb.lock().expect("lock").push(r.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("enqueue_paused");

    assert!(
        wait_until(2000, || has_status(&statuses, &TransferStatus::Paused)).await,
        "paused first"
    );

    client.resume(task_id).await.expect("resume paused import");

    let terminal = wait_terminal(&statuses, 8000).await;
    client.close().await.expect("close");
    server.shutdown();

    assert!(
        matches!(terminal, Some(TransferStatus::Complete)),
        "expected Complete after resume, got {terminal:?}"
    );
    let bytes = fs::read(&path).expect("read downloaded file");
    let _ = fs::remove_file(&path);
    assert_eq!(bytes, payload, "downloaded content must match payload");
}

#[tokio::test]
async fn enqueue_paused_upload_no_io_until_resume_then_completes() {
    // Upload direction: while paused-imported, the upload protocol's
    // prepare/chunk call counts must be 0; after resume it must actually run
    // prepare + chunks and finish.
    let src = temp_path("upload_src");
    let body = b"upload-paused-import-payload!".repeat(8); // far larger than one chunk
    fs::write(&src, &body).expect("write upload source");

    let server = dev_server::DevFileServer::spawn(Vec::new());
    let client = single_dl_client();

    let completed = Arc::new(AtomicBool::new(false));
    let completed_cb = completed.clone();

    let task = UploadPounceBuilder::new("up.bin", &src, 16)
        .with_url(format!("{}/upload", server.base_url()))
        .build()
        .expect("build upload task");

    let task_id = client
        .try_enqueue_paused(
            task,
            |_r: FileTransferRecord| {},
            move |_id, _payload| {
                completed_cb.store(true, Ordering::SeqCst);
            },
        )
        .await
        .expect("enqueue_paused upload");

    // While paused: zero protocol calls.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let before = server.upload_inspector();
    assert_eq!(before.prepare_calls, 0, "no prepare before resume");
    assert_eq!(before.chunk_calls, 0, "no chunk upload before resume");

    // After resume: it must actually upload and complete.
    client.resume(task_id).await.expect("resume upload");
    assert!(
        wait_until(8000, || completed.load(Ordering::SeqCst)).await,
        "upload should complete after resume"
    );
    let after = server.upload_inspector();
    assert!(after.prepare_calls >= 1, "prepare must run after resume");
    assert!(after.chunk_calls >= 1, "chunks must upload after resume");
    assert!(after.completed, "server should observe completion");

    client.close().await.expect("close");
    server.shutdown();
    let _ = fs::remove_file(&src);
}

#[tokio::test]
async fn import_many_paused_then_resume_only_one() {
    // Import 3 paused tasks and resume only the middle one: only it downloads to
    // completion; the other two stay paused (no file created, no Transmission).
    let payload = b"selective-start-payload".repeat(256);
    let server = dev_server::DevFileServer::spawn(payload.clone());
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(2)
            .build()
            .expect("valid config"),
    );

    let mut ids = Vec::new();
    let mut paths = Vec::new();
    let mut status_vecs: Vec<Arc<Mutex<Vec<TransferStatus>>>> = Vec::new();

    for i in 0..3usize {
        let path = temp_path(&format!("many_{i}"));
        let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
        let statuses_cb = statuses.clone();
        // Distinct URLs to avoid dedupe collisions.
        let task = DownloadPounceBuilder::new(
            format!("many_{i}.bin"),
            &path,
            1024,
            format!("{}/download/many_{i}.bin", server.base_url()),
        )
        .build();
        let id = client
            .try_enqueue_paused(
                task,
                move |r: FileTransferRecord| {
                    statuses_cb.lock().expect("lock").push(r.status().clone());
                },
                |_, _| {},
            )
            .await
            .expect("enqueue_paused many");
        ids.push(id);
        paths.push(path);
        status_vecs.push(statuses);
    }

    // All should be idle (paused).
    let snap = client.snapshot().await.expect("snapshot");
    assert_eq!(snap.queued_groups, 0);
    assert_eq!(snap.active_groups, 0);

    // Start only the middle one.
    client.resume(ids[1]).await.expect("resume the chosen one");

    let terminal = wait_terminal(&status_vecs[1], 8000).await;
    assert!(
        matches!(terminal, Some(TransferStatus::Complete)),
        "the resumed task must complete, got {terminal:?}"
    );

    // The other two stay paused: no Transmission, no Complete, no file written.
    for i in [0usize, 2usize] {
        assert!(
            !has_status(&status_vecs[i], &TransferStatus::Transmission),
            "unselected task {i} must not transfer"
        );
        assert!(
            !has_status(&status_vecs[i], &TransferStatus::Complete),
            "unselected task {i} must not complete"
        );
        assert!(!paths[i].exists(), "unselected task {i} must not write a file");
    }

    let bytes = fs::read(&paths[1]).expect("read resumed file");
    assert_eq!(bytes, payload);

    client.close().await.expect("close");
    server.shutdown();
    for p in &paths {
        let _ = fs::remove_file(p);
    }
}

#[tokio::test]
async fn resume_paused_import_continues_from_existing_partial_file() {
    // Core restore semantics: with an on-disk partial file already present, a
    // paused import + resume must continue from the local length and finish with
    // bytes equal to the full payload.
    let payload = b"0123456789abcdefghijABCDEFGHIJ".repeat(64); // 1920 bytes
    let prefix_len = 700usize;
    let server = dev_server::DevFileServer::spawn(payload.clone());
    let client = single_dl_client();
    let path = temp_path("partial_resume");

    // Pre-write a partial file matching the payload prefix.
    fs::write(&path, &payload[..prefix_len]).expect("write partial file");

    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();
    let task = DownloadPounceBuilder::new(
        "partial.bin",
        &path,
        256,
        format!("{}/download/partial.bin", server.base_url()),
    )
    .build();

    let task_id = client
        .try_enqueue_paused(
            task,
            move |r: FileTransferRecord| {
                statuses_cb.lock().expect("lock").push(r.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("enqueue_paused");

    assert!(wait_until(2000, || has_status(&statuses, &TransferStatus::Paused)).await);
    client.resume(task_id).await.expect("resume");

    let terminal = wait_terminal(&statuses, 8000).await;
    client.close().await.expect("close");
    server.shutdown();

    assert!(
        matches!(terminal, Some(TransferStatus::Complete)),
        "expected Complete, got {terminal:?}"
    );
    let bytes = fs::read(&path).expect("read final file");
    let _ = fs::remove_file(&path);
    assert_eq!(bytes, payload, "resume must continue from on-disk partial and finish correctly");
}

// ===================== exceptions / improper usage =====================

#[tokio::test]
async fn enqueue_paused_after_close_returns_client_closed() {
    let client = single_dl_client();
    client.close().await.expect("close");

    let task = DownloadPounceBuilder::new(
        "late.bin",
        temp_path("late"),
        1024,
        "http://127.0.0.1:9/late",
    )
    .build();
    let err = client
        .try_enqueue_paused(task, |_r: FileTransferRecord| {}, |_, _| {})
        .await
        .expect_err("enqueue_paused after close");
    assert_eq!(err.code(), InnerErrorCode::ClientClosed as i32);
}

#[tokio::test]
async fn enqueue_paused_empty_url_returns_parameter_empty() {
    let client = single_dl_client();
    let task = DownloadPounceBuilder::new("a.bin", temp_path("empty_url"), 1024, "").build();
    let err = client
        .try_enqueue_paused(task, |_r: FileTransferRecord| {}, |_, _| {})
        .await
        .expect_err("empty url");
    assert_eq!(err.code(), InnerErrorCode::ParameterEmpty as i32);
    client.close().await.expect("close");
}

#[tokio::test]
async fn enqueue_paused_empty_file_name_returns_parameter_empty() {
    let client = single_dl_client();
    let task =
        DownloadPounceBuilder::new("", temp_path("empty_name"), 1024, "http://127.0.0.1:9/x")
            .build();
    let err = client
        .try_enqueue_paused(task, |_r: FileTransferRecord| {}, |_, _| {})
        .await
        .expect_err("empty name");
    assert_eq!(err.code(), InnerErrorCode::ParameterEmpty as i32);
    client.close().await.expect("close");
}

#[tokio::test]
async fn enqueue_paused_upload_zero_byte_source_returns_parameter_empty() {
    let src = temp_path("zero_upload");
    fs::write(&src, []).expect("write empty file");
    let task = UploadPounceBuilder::new("zero.bin", &src, 1024)
        .with_url("http://127.0.0.1:9/up")
        .build()
        .expect("build upload task");
    let client = single_dl_client();
    let err = client
        .try_enqueue_paused(task, |_r: FileTransferRecord| {}, |_, _| {})
        .await
        .expect_err("zero-byte upload");
    assert_eq!(err.code(), InnerErrorCode::ParameterEmpty as i32);
    let _ = fs::remove_file(&src);
    client.close().await.expect("close");
}

#[tokio::test]
async fn duplicate_paused_import_reports_duplicate_in_callback() {
    // Two paused imports with the same URL: the second returns a task_id but its
    // callback must observe Failed(DuplicateTaskError), matching the dedupe
    // behavior of a normal enqueue.
    let payload = b"dup-paused-import".repeat(64);
    let server = dev_server::DevFileServer::spawn(payload);
    let client = single_dl_client();
    let url = format!("{}/download/dup.bin", server.base_url());

    let s2: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let s2_cb = s2.clone();

    let t1 = DownloadPounceBuilder::new("dup.bin", temp_path("dup1"), 1024, url.clone()).build();
    let t2 = DownloadPounceBuilder::new("dup.bin", temp_path("dup2"), 1024, url).build();

    client
        .try_enqueue_paused(t1, |_r: FileTransferRecord| {}, |_, _| {})
        .await
        .expect("first paused import");
    client
        .try_enqueue_paused(
            t2,
            move |r: FileTransferRecord| {
                s2_cb.lock().expect("lock").push(r.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("second paused import still returns id");

    let saw_dup = wait_until(3000, || {
        s2.lock().expect("lock").iter().any(|s| {
            matches!(s, TransferStatus::Failed(e) if e.code() == InnerErrorCode::DuplicateTaskError as i32)
        })
    })
    .await;
    assert!(saw_dup, "duplicate paused import must report DuplicateTaskError");

    client.close().await.expect("close");
    server.shutdown();
}

#[tokio::test]
async fn paused_import_then_active_enqueue_same_url_reports_duplicate() {
    // Paused import first, then a normal try_enqueue with the same URL: the
    // second submission must hit the duplicate branch.
    let payload = b"paused-then-active".repeat(64);
    let server = dev_server::DevFileServer::spawn(payload);
    let client = single_dl_client();
    let url = format!("{}/download/mix1.bin", server.base_url());

    let s2: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let s2_cb = s2.clone();

    let t1 = DownloadPounceBuilder::new("mix.bin", temp_path("mix1a"), 1024, url.clone()).build();
    let t2 = DownloadPounceBuilder::new("mix.bin", temp_path("mix1b"), 1024, url).build();

    client
        .try_enqueue_paused(t1, |_r: FileTransferRecord| {}, |_, _| {})
        .await
        .expect("paused import");
    client
        .try_enqueue(
            t2,
            move |r: FileTransferRecord| {
                s2_cb.lock().expect("lock").push(r.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("active enqueue returns id");

    let saw_dup = wait_until(3000, || {
        s2.lock().expect("lock").iter().any(|s| {
            matches!(s, TransferStatus::Failed(e) if e.code() == InnerErrorCode::DuplicateTaskError as i32)
        })
    })
    .await;
    assert!(saw_dup, "active enqueue over an existing paused import must be a duplicate");

    client.close().await.expect("close");
    server.shutdown();
}

#[tokio::test]
async fn active_enqueue_then_paused_import_same_url_reports_duplicate() {
    // Reverse: active enqueue first, then a paused import with the same URL; the
    // paused import must hit the duplicate branch.
    let payload = b"active-then-paused".repeat(64);
    let server = dev_server::DevFileServer::spawn(payload);
    let client = single_dl_client();
    let url = format!("{}/download/mix2.bin", server.base_url());

    let s2: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let s2_cb = s2.clone();

    let t1 = DownloadPounceBuilder::new("mix.bin", temp_path("mix2a"), 1024, url.clone()).build();
    let t2 = DownloadPounceBuilder::new("mix.bin", temp_path("mix2b"), 1024, url).build();

    client
        .try_enqueue(t1, |_r: FileTransferRecord| {}, |_, _| {})
        .await
        .expect("active enqueue");
    client
        .try_enqueue_paused(
            t2,
            move |r: FileTransferRecord| {
                s2_cb.lock().expect("lock").push(r.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("paused import returns id");

    let saw_dup = wait_until(3000, || {
        s2.lock().expect("lock").iter().any(|s| {
            matches!(s, TransferStatus::Failed(e) if e.code() == InnerErrorCode::DuplicateTaskError as i32)
        })
    })
    .await;
    assert!(saw_dup, "paused import over an existing active task must be a duplicate");

    client.close().await.expect("close");
    server.shutdown();
}

#[tokio::test]
async fn cancel_paused_import_then_resume_returns_task_not_found() {
    // Cancelling a never-started paused import must succeed and clean up; a
    // subsequent resume of the same id must return TaskNotFound (group removed).
    let payload = b"cancel-paused-import".repeat(64);
    let server = dev_server::DevFileServer::spawn(payload);
    let client = single_dl_client();
    let path = temp_path("cancel_paused");

    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();
    let task = DownloadPounceBuilder::new(
        "cancel.bin",
        &path,
        1024,
        format!("{}/download/cancel.bin", server.base_url()),
    )
    .build();
    let task_id = client
        .try_enqueue_paused(
            task,
            move |r: FileTransferRecord| {
                statuses_cb.lock().expect("lock").push(r.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("paused import");

    assert!(wait_until(2000, || has_status(&statuses, &TransferStatus::Paused)).await);

    client.cancel(task_id).await.expect("cancel paused import");
    assert!(
        wait_until(2000, || has_status(&statuses, &TransferStatus::Canceled)).await,
        "cancel should emit Canceled"
    );

    let err = client
        .resume(task_id)
        .await
        .expect_err("resume after cancel must fail");
    assert_eq!(err.code(), InnerErrorCode::TaskNotFound as i32);

    assert!(!path.exists(), "canceled-before-start task must not write a file");
    client.close().await.expect("close");
    server.shutdown();
    let _ = fs::remove_file(&path);
}

#[tokio::test]
async fn double_resume_on_paused_import_is_rejected() {
    // Resuming the same paused import twice is invalid. Because the first resume
    // already cleared the paused flag and entered scheduling, the second must
    // return InvalidTaskState (still running) or TaskNotFound (already finished
    // and removed); both are valid rejections.
    let payload = b"double-resume-payload".repeat(4096); // large, lowers instant-finish odds
    let server = dev_server::DevFileServer::spawn(payload);
    let client = single_dl_client();
    let path = temp_path("double_resume");

    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();
    let task = DownloadPounceBuilder::new(
        "double.bin",
        &path,
        1024,
        format!("{}/download/double.bin", server.base_url()),
    )
    .build();
    let task_id = client
        .try_enqueue_paused(
            task,
            move |r: FileTransferRecord| {
                statuses_cb.lock().expect("lock").push(r.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("paused import");

    assert!(wait_until(2000, || has_status(&statuses, &TransferStatus::Paused)).await);

    client.resume(task_id).await.expect("first resume should succeed");
    let err = client
        .resume(task_id)
        .await
        .expect_err("second resume must be rejected");
    assert!(
        err.code() == InnerErrorCode::InvalidTaskState as i32
            || err.code() == InnerErrorCode::TaskNotFound as i32,
        "second resume should be InvalidTaskState or TaskNotFound, got code {}",
        err.code()
    );

    client.close().await.expect("close");
    server.shutdown();
    let _ = fs::remove_file(&path);
}

#[tokio::test]
async fn pause_on_paused_import_is_idempotent_and_resume_still_works() {
    // Pausing an already-paused import must be idempotent and succeed, and the
    // task must still resume to completion afterwards.
    let payload = b"pause-idempotent-payload".repeat(128);
    let server = dev_server::DevFileServer::spawn(payload.clone());
    let client = single_dl_client();
    let path = temp_path("pause_idem");

    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();
    let task = DownloadPounceBuilder::new(
        "idem.bin",
        &path,
        1024,
        format!("{}/download/idem.bin", server.base_url()),
    )
    .build();
    let task_id = client
        .try_enqueue_paused(
            task,
            move |r: FileTransferRecord| {
                statuses_cb.lock().expect("lock").push(r.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("paused import");

    assert!(wait_until(2000, || has_status(&statuses, &TransferStatus::Paused)).await);

    // Re-pause: the current implementation returns Ok for an already-paused task
    // (it emits Paused again).
    client.pause(task_id).await.expect("re-pause should be ok");
    assert!(
        count_status(&statuses, &TransferStatus::Paused) >= 1,
        "still observably paused"
    );
    // Still no transfer has happened.
    assert!(!has_status(&statuses, &TransferStatus::Transmission));

    // Resume must still drive it to completion.
    let mut resumed = false;
    for _ in 0..150 {
        if client.resume(task_id).await.is_ok() {
            resumed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(resumed, "resume after re-pause should eventually succeed");

    let terminal = wait_terminal(&statuses, 8000).await;
    client.close().await.expect("close");
    server.shutdown();
    assert!(matches!(terminal, Some(TransferStatus::Complete)), "got {terminal:?}");
    let bytes = fs::read(&path).expect("read file");
    let _ = fs::remove_file(&path);
    assert_eq!(bytes, payload);
}

#[tokio::test]
async fn enqueue_paused_returns_command_send_failed_when_queue_is_full() {
    // Same fail-fast back-pressure as try_enqueue: with command queue capacity 1,
    // a burst of paused imports must yield at least one CommandSendFailed.
    let payload = b"paused-back-pressure".repeat(8);
    let server = dev_server::DevFileServer::spawn(payload);
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(1)
            .command_queue_capacity(1)
            .worker_event_queue_capacity(1)
            .build()
            .expect("valid config"),
    );

    let mut results = Vec::new();
    for i in 0..32usize {
        let task = DownloadPounceBuilder::new(
            format!("bp_{i}.bin"),
            temp_path(&format!("bp_{i}")),
            1024,
            format!("{}/download/bp_{i}.bin", server.base_url()),
        )
        .build();
        results.push(
            client
                .try_enqueue_paused(task, |_r: FileTransferRecord| {}, |_, _| {})
                .await,
        );
    }

    let failed: Vec<_> = results.iter().filter_map(|r| r.as_ref().err()).collect();
    assert!(
        !failed.is_empty(),
        "expected at least one CommandSendFailed under a 1-slot command queue"
    );
    for err in &failed {
        assert_eq!(
            err.code(),
            InnerErrorCode::CommandSendFailed as i32,
            "fail-fast error must be CommandSendFailed; got {err:?}"
        );
    }

    client.close().await.expect("close");
    server.shutdown();
}

#[tokio::test]
async fn close_with_unresumed_paused_imports_is_clean() {
    // Importing several paused tasks and never resuming them, then closing, must
    // return cleanly (no hang) and emit a terminal Paused for those groups.
    let payload = b"close-with-paused".repeat(64);
    let server = dev_server::DevFileServer::spawn(payload);
    let client = single_dl_client();

    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    for i in 0..2usize {
        let statuses_cb = statuses.clone();
        let task = DownloadPounceBuilder::new(
            format!("closepaused_{i}.bin"),
            temp_path(&format!("close_paused_{i}")),
            1024,
            format!("{}/download/closepaused_{i}.bin", server.base_url()),
        )
        .build();
        client
            .try_enqueue_paused(
                task,
                move |r: FileTransferRecord| {
                    statuses_cb.lock().expect("lock").push(r.status().clone());
                },
                |_, _| {},
            )
            .await
            .expect("paused import");
    }

    assert!(wait_until(2000, || has_status(&statuses, &TransferStatus::Paused)).await);

    // Key point: closing with unresumed paused imports must not hang.
    client.close().await.expect("close should be clean even with paused imports");
    assert!(client.is_closed());
    server.shutdown();
}
