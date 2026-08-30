//! 故意异常用例 — 单文件内多分片并发（optimization ④，commit 2776a94）的失败/边界路径。
//!
//! 既有 `intra_file_parallel_test.rs` 已覆盖：乱序成功、窗口上限、不支持协议串行、
//! `max_parts_in_flight==1` 串行、取消中途、分片 panic。本文件补齐尚未覆盖的**异常**分支，
//! 全部经真实执行器（`MeowClient::try_enqueue` -> scheduler -> `run_group_parallel`）驱动，
//! 通过一个可注入异常的自定义 `BreakpointUpload`（无网络）验证：
//!   - 某个分片**返回 Err**（非 panic）：整文件 Failed，`complete` 绝不被调用，不卡死；
//!   - **最低位**分片失败而高位分片已成功：仍 Failed，completion 被 watermark 门控住；
//!   - 全部分片成功但 **`complete` 收尾失败**：整文件 Failed（不能误报 Complete）；
//!   - 分片**瞬时可重试错误**在并发模式下重试后恢复，最终 Complete 且 `complete` 恰好一次；
//!   - **resume 时已到 total**（`prepare` 直接给出 `next_byte==total`）：发 Complete 且
//!     **不调用** `complete`（与串行早退一致——见用例内说明）。

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use rusty_cat::error::InnerErrorCode;
use rusty_cat::http_breakpoint::UploadResumeInfo;
use rusty_cat::upload_trait::{UploadChunkCtx, UploadPrepareCtx};
use rusty_cat::{
    BreakpointUpload, MeowClient, MeowConfig, MeowError, TransferStatus, UploadPounceBuilder,
};

fn temp_upload_path(case: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    p.push(format!("rusty_cat_parallel_fail_{case}_{ts}.bin"));
    p
}

#[derive(Default, Clone)]
struct Recorder {
    /// offset -> received chunk length (only successfully uploaded parts).
    received: BTreeMap<u64, usize>,
    in_flight: usize,
    peak_in_flight: usize,
    /// Number of `complete_upload` invocations (attempts, success or not).
    complete_calls: usize,
    abort_calls: usize,
}

/// Injectable failure config for the mock upload protocol.
#[derive(Clone)]
struct FailPlan {
    total: u64,
    chunk: u64,
    /// `prepare` returns a non-retryable Err (resume/initiate failure).
    fail_prepare: bool,
    /// `prepare` reports this as `next_byte` (resume cursor). `0` = fresh upload.
    prepare_next_byte: u64,
    /// `upload_chunk` for this start offset always returns Err (hard part error).
    fail_at_offset: Option<u64>,
    /// `upload_chunk` for this offset returns a retryable Err on the FIRST
    /// attempt only, then succeeds — exercises retry inside the parallel path.
    transient_fail_once_at: Option<u64>,
    /// `upload_chunk` for this offset returns `completed_file_id: Some(..)`. In the
    /// parallel path a part MUST NOT short-circuit finalize off this (unlike serial).
    completed_file_id_at: Option<u64>,
    /// `upload_chunk` lies about progress, reporting `next_byte` two chunks ahead.
    /// The parallel window keys accounting on the dispatched offset, so the lie
    /// must not create holes or skip a part.
    lie_next_byte: bool,
    /// `complete_upload` returns Err (finalize-step failure).
    fail_complete: bool,
    /// Per-chunk base delay (ms).
    delay_ms: u64,
    /// Higher offsets finish first when true (stresses the watermark on failure).
    scramble: bool,
}

struct InjectableUpload {
    rec: Arc<Mutex<Recorder>>,
    plan: FailPlan,
    /// Per-offset attempt counter, for `transient_fail_once_at`.
    attempts: Arc<Mutex<HashMap<u64, usize>>>,
}

#[async_trait]
impl BreakpointUpload for InjectableUpload {
    async fn prepare(&self, _ctx: UploadPrepareCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        if self.plan.fail_prepare {
            // Non-retryable code (not HttpError) so the prepare layer fast-fails
            // instead of retrying, and we never enter the parallel driver.
            return Err(MeowError::from_code_str(
                InnerErrorCode::Unknown,
                "injected prepare failure",
            ));
        }
        Ok(UploadResumeInfo {
            completed_file_id: None,
            next_byte: Some(self.plan.prepare_next_byte),
            provider_upload_id: None,
        })
    }

    async fn upload_chunk(&self, ctx: UploadChunkCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
        let offset = ctx.offset;
        let len = ctx.chunk.len();

        // Hard, non-recoverable part error.
        if self.plan.fail_at_offset == Some(offset) {
            return Err(MeowError::from_code_str(
                InnerErrorCode::ResponseStatusError,
                "injected hard part error",
            ));
        }
        // Transient error on the first attempt only.
        if self.plan.transient_fail_once_at == Some(offset) {
            let first = {
                let mut a = self.attempts.lock().expect("attempts lock");
                let n = a.entry(offset).or_insert(0);
                *n += 1;
                *n == 1
            };
            if first {
                // Retryable code (ResponseStatusError without an HTTP status stays
                // retryable), so the retry layer reattempts and then succeeds.
                return Err(MeowError::from_code_str(
                    InnerErrorCode::ResponseStatusError,
                    "injected transient part error",
                ));
            }
        }

        {
            let mut r = self.rec.lock().expect("rec lock");
            r.in_flight += 1;
            r.peak_in_flight = r.peak_in_flight.max(r.in_flight);
        }
        let delay = if self.plan.scramble {
            (self.plan.total.saturating_sub(offset) / self.plan.chunk.max(1)) * self.plan.delay_ms
        } else {
            self.plan.delay_ms
        };
        tokio::time::sleep(Duration::from_millis(delay)).await;
        {
            let mut r = self.rec.lock().expect("rec lock");
            r.received.insert(offset, len);
            r.in_flight -= 1;
        }
        let next_byte = if self.plan.lie_next_byte {
            // Claim two chunks of progress: the parallel window must ignore this
            // for accounting (it keys on the dispatched offset).
            Some((offset + 2 * self.plan.chunk).min(self.plan.total))
        } else {
            Some(offset + len as u64)
        };
        Ok(UploadResumeInfo {
            // A server-side "already complete" id reported by ONE part must not
            // finalize the parallel upload; the scheduler owns completion.
            completed_file_id: if self.plan.completed_file_id_at == Some(offset) {
                Some("server-finalized".to_string())
            } else {
                None
            },
            next_byte,
            provider_upload_id: None,
        })
    }

    async fn complete_upload(
        &self,
        _client: &reqwest::Client,
        _task: &rusty_cat::TransferTask,
    ) -> Result<Option<String>, MeowError> {
        self.rec.lock().expect("rec lock").complete_calls += 1;
        if self.plan.fail_complete {
            return Err(MeowError::from_code_str(
                InnerErrorCode::ResponseStatusError,
                "injected complete failure",
            ));
        }
        Ok(Some("done".to_string()))
    }

    async fn abort_upload(
        &self,
        _client: &reqwest::Client,
        _task: &rusty_cat::TransferTask,
    ) -> Result<(), MeowError> {
        self.rec.lock().expect("rec lock").abort_calls += 1;
        Ok(())
    }

    fn supports_parallel_parts(&self) -> bool {
        true
    }
}

struct RunOutcome {
    terminal: Option<TransferStatus>,
    rec: Recorder,
}

/// Drives one upload to a terminal status and returns the recorder snapshot.
#[allow(clippy::too_many_arguments)]
async fn run(case: &str, plan: FailPlan, max_parts: usize, max_chunk_retries: u32) -> RunOutcome {
    let payload: Vec<u8> = (0..plan.total as u32).map(|i| (i % 251) as u8).collect();
    let path = temp_upload_path(case);
    fs::write(&path, &payload).expect("write upload fixture");

    let rec = Arc::new(Mutex::new(Recorder::default()));
    let client = MeowClient::new(
        MeowConfig::builder()
            .max_upload_concurrency(1)
            .max_download_concurrency(1)
            .build()
            .expect("valid config"),
    );
    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses_cb = statuses.clone();

    let task = UploadPounceBuilder::new("p.bin", &path, plan.chunk)
        .with_url("https://placeholder/upload")
        .with_breakpoint_upload(Arc::new(InjectableUpload {
            rec: rec.clone(),
            plan: plan.clone(),
            attempts: Arc::new(Mutex::new(HashMap::new())),
        }))
        .with_max_parts_in_flight(max_parts)
        .with_max_chunk_retries(max_chunk_retries)
        .build()
        .expect("build upload task");

    client
        .try_enqueue(
            task,
            move |record| {
                statuses_cb
                    .lock()
                    .expect("lock statuses")
                    .push(record.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("enqueue upload task");

    let mut terminal = None;
    for _ in 0..600 {
        let snapshot = statuses.lock().expect("lock statuses").clone();
        terminal = snapshot
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
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    client.close().await.expect("close client");
    let _ = fs::remove_file(&path);

    let rec = rec.lock().expect("rec lock").clone();
    RunOutcome { terminal, rec }
}

fn base_plan(total: u64, chunk: u64) -> FailPlan {
    FailPlan {
        total,
        chunk,
        fail_prepare: false,
        prepare_next_byte: 0,
        fail_at_offset: None,
        transient_fail_once_at: None,
        completed_file_id_at: None,
        lie_next_byte: false,
        fail_complete: false,
        delay_ms: 8,
        scramble: false,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn part_error_fails_upload_without_completing() {
    // A middle part returns a hard Err while siblings are in flight. The driver
    // must drain to quiescence, emit exactly one Failed, and NEVER finalize — a
    // file with a missing part must not be committed.
    let mut plan = base_plan(6000, 1000);
    plan.fail_at_offset = Some(2000);
    let out = run("part_err", plan, 3, 0).await;

    assert!(
        matches!(out.terminal, Some(TransferStatus::Failed(_))),
        "a part Err must terminate the upload as Failed, got {:?}",
        out.terminal
    );
    assert_eq!(
        out.rec.complete_calls, 0,
        "complete must NOT be called when a part failed"
    );
    assert!(
        !out.rec.received.contains_key(&2000),
        "the failing part's offset must not be recorded as uploaded"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn post_prepare_parallel_window_rejection_aborts_created_session() {
    let plan = base_plan(257, 1);
    let out = run("window_reject_abort", plan, 257, 0).await;

    assert!(matches!(out.terminal, Some(TransferStatus::Failed(_))));
    assert_eq!(
        out.rec.received.len(),
        0,
        "no part may start after rejection"
    );
    assert_eq!(out.rec.complete_calls, 0);
    assert_eq!(
        out.rec.abort_calls, 1,
        "prepare-created provider session must be aborted exactly once"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn low_part_failure_with_high_parts_done_still_fails_without_complete() {
    // Scramble so the HIGH offsets finish first (their parts are recorded), but
    // the LOWEST part (offset 0) hard-fails. Because completion is gated on a
    // contiguous prefix reaching total, the already-finished high parts must NOT
    // trigger `complete`; the whole file fails.
    let mut plan = base_plan(6000, 1000);
    plan.fail_at_offset = Some(0);
    plan.scramble = true;
    let out = run("low_fail", plan, 4, 0).await;

    assert!(
        matches!(out.terminal, Some(TransferStatus::Failed(_))),
        "a failed low part must fail the whole upload, got {:?}",
        out.terminal
    );
    assert_eq!(
        out.rec.complete_calls, 0,
        "completion must stay gated behind the contiguous prefix; complete must not fire"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn complete_step_failure_marks_upload_failed() {
    // Every part uploads fine, but the finalize step (`complete_upload`) fails.
    // The upload must surface as Failed, not a false Complete. The finalize is
    // attempted exactly once, after all parts joined.
    let mut plan = base_plan(6000, 1000);
    plan.fail_complete = true;
    let out = run("complete_fail", plan, 4, 0).await;

    assert!(
        matches!(out.terminal, Some(TransferStatus::Failed(_))),
        "a failing complete step must terminate as Failed, got {:?}",
        out.terminal
    );
    assert_eq!(
        out.rec.received.len(),
        6,
        "all 6 parts must have uploaded before finalize was attempted"
    );
    assert_eq!(
        out.rec.complete_calls, 1,
        "finalize must be attempted exactly once (after the join barrier)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transient_part_error_recovers_and_completes() {
    // One part fails transiently (retryable) on its first attempt, then succeeds.
    // The parallel path shares the same retry layer as serial, so the upload must
    // still reach Complete with every part recorded and `complete` fired once.
    let mut plan = base_plan(6000, 1000);
    plan.transient_fail_once_at = Some(3000);
    let out = run("transient", plan, 4, 2).await;

    assert!(
        matches!(out.terminal, Some(TransferStatus::Complete)),
        "a transient (retryable) part error must recover to Complete, got {:?}",
        out.terminal
    );
    assert_eq!(
        out.rec.received.len(),
        6,
        "every part (including the one that retried) must end up uploaded"
    );
    assert_eq!(
        out.rec.complete_calls, 1,
        "complete must fire exactly once after recovery"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_at_total_completes_without_finalize() {
    // `prepare` reports the file is already fully uploaded (next_byte == total),
    // e.g. a resume after every part was sent. The parallel driver's
    // `start_offset >= total` early return must emit Complete WITHOUT uploading
    // any chunk and WITHOUT calling `complete` — byte-identical to the serial
    // early return. (This documents that resume-at-total skips a fresh finalize;
    // it matches existing serial behavior.)
    let mut plan = base_plan(6000, 1000);
    plan.prepare_next_byte = 6000;
    let out = run("resume_total", plan, 4, 0).await;

    assert!(
        matches!(out.terminal, Some(TransferStatus::Complete)),
        "resume already at total must reach Complete, got {:?}",
        out.terminal
    );
    assert!(
        out.rec.received.is_empty(),
        "no chunk must be uploaded when resuming already at total"
    );
    assert_eq!(
        out.rec.complete_calls, 0,
        "the resume-at-total fast path must not call complete"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prepare_failure_fails_before_dispatching_any_part() {
    // `prepare` fails (non-retryable) on a parallel-eligible task. The failure
    // must surface BEFORE the windowed driver spawns anything: no chunk is
    // uploaded and complete is never called.
    let mut plan = base_plan(6000, 1000);
    plan.fail_prepare = true;
    let out = run("prepare_fail", plan, 4, 0).await;

    assert!(
        matches!(out.terminal, Some(TransferStatus::Failed(_))),
        "a prepare failure must terminate as Failed, got {:?}",
        out.terminal
    );
    assert!(
        out.rec.received.is_empty(),
        "no part may be dispatched when prepare failed, got {:?}",
        out.rec.received.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        out.rec.complete_calls, 0,
        "complete must not run when prepare failed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn part_reporting_completed_file_id_does_not_short_circuit_finalize() {
    // The SERIAL path treats a chunk's `completed_file_id` as terminal; the
    // PARALLEL path must NOT — finalizing off whichever part reaches the server's
    // "done" first, while lower parts are still in flight, would commit a torn
    // object. So an early part reporting a completed_file_id must be ignored: all
    // parts still upload and the scheduler finalizes exactly once.
    let mut plan = base_plan(6000, 1000);
    plan.completed_file_id_at = Some(1000);
    plan.scramble = true;
    let out = run("completed_id_ignored", plan, 4, 0).await;

    assert!(
        matches!(out.terminal, Some(TransferStatus::Complete)),
        "a part's completed_file_id must not short-circuit; expected Complete, got {:?}",
        out.terminal
    );
    assert_eq!(
        out.rec.received.len(),
        6,
        "every part must still upload despite a part reporting completed_file_id"
    );
    assert_eq!(
        out.rec.complete_calls, 1,
        "the scheduler must own finalization (exactly once), not an individual part"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn part_lying_about_next_byte_does_not_skip_or_hole() {
    // A part claims `next_byte` two chunks ahead. The parallel window accounts by
    // dispatched offset, not by the protocol's reported cursor, so the lie must
    // not skip a part or leave a hole — every chunk is still dispatched & uploaded.
    let mut plan = base_plan(6000, 1000);
    plan.lie_next_byte = true;
    let out = run("lie_next_byte", plan, 4, 0).await;

    assert!(
        matches!(out.terminal, Some(TransferStatus::Complete)),
        "a part lying about next_byte must still complete, got {:?}",
        out.terminal
    );
    assert_eq!(
        out.rec.received.keys().copied().collect::<Vec<_>>(),
        vec![0, 1000, 2000, 3000, 4000, 5000],
        "every chunk-aligned offset must be dispatched exactly once despite the jumped cursor"
    );
    assert_eq!(out.rec.complete_calls, 1, "finalize fires exactly once");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn short_last_part_out_of_order_with_retry_completes_correctly() {
    // End-to-end stress of the short-last-part + out-of-order + retry combination:
    // total 6500 (last part is 500 bytes), high offsets finish first, and the last
    // (short) part fails once transiently before succeeding. The watermark must
    // still clamp to total and finalize exactly once.
    let mut plan = base_plan(6500, 1000);
    plan.scramble = true;
    plan.transient_fail_once_at = Some(6000);
    let out = run("short_last", plan, 3, 2).await;

    assert!(
        matches!(out.terminal, Some(TransferStatus::Complete)),
        "short last part with a transient retry must still complete, got {:?}",
        out.terminal
    );
    assert_eq!(
        out.rec.received.keys().copied().collect::<Vec<_>>(),
        vec![0, 1000, 2000, 3000, 4000, 5000, 6000],
        "all seven chunk-aligned offsets must arrive"
    );
    assert_eq!(
        out.rec.received.get(&6000).copied(),
        Some(500),
        "the short last part must carry exactly 500 bytes"
    );
    assert!(
        out.rec
            .received
            .iter()
            .filter(|(&o, _)| o != 6000)
            .all(|(_, &l)| l == 1000),
        "every non-final part must carry a full 1000-byte chunk"
    );
    assert_eq!(out.rec.complete_calls, 1, "finalize fires exactly once");
}
