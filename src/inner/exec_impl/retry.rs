use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::chunk_outcome::ChunkOutcome;
use crate::error::{InnerErrorCode, MeowError};
use crate::inner::UniqueId;
use crate::transfer_executor_trait::TransferTrait;
use crate::transfer_task::TransferTask;

/// 分片重试：基础退避时长（毫秒）。
const CHUNK_RETRY_BASE_DELAY_MS: u64 = 200;
/// 分片重试：最大退避时长上限（毫秒）。
const CHUNK_RETRY_MAX_DELAY_MS: u64 = 5_000;
/// 分片重试：抖动比例（20%），用于避免同频重试雪崩。
const CHUNK_RETRY_JITTER_PERCENT: u64 = 20;

/// 可重试错误码边界（常量）：
/// - 目前先固定为网络/服务端瞬时失败相关码；
/// - 后续可平滑迁移到配置项，不影响调用方。
const RETRYABLE_CHUNK_ERROR_CODES: &[i32] = &[
    InnerErrorCode::HttpError as i32,
    InnerErrorCode::ResponseStatusError as i32,
];

/// 分片重试执行结果：
/// - `Done(outcome)`：本分片成功完成；
/// - `Cancelled`：在重试等待期间收到取消信号，应交由上层走取消分支；
/// - `Failed(err)`：重试耗尽或不可重试错误，交由上层按失败处理。
pub(crate) enum ChunkRetryResult {
    Done(ChunkOutcome),
    Cancelled,
    Failed(MeowError),
}

/// 对单个分片执行“带退避的重试”。
///
/// 设计目标：
/// 1) 将重试策略封装在独立模块，降低对 `exec.rs` 的耦合；
/// 2) 对外只暴露“成功/取消/失败”三态，简化上层流程控制；
/// 3) 在每次重试决策都打详细日志，便于线上排障。
pub(crate) async fn transfer_chunk_with_retry(
    executor: &Arc<dyn TransferTrait>,
    task: &TransferTask,
    key: &UniqueId,
    cancel: &CancellationToken,
    offset: u64,
    chunk_size: u64,
    known_total: u64,
    max_chunk_retries: u32,
) -> ChunkRetryResult {
    // 尝试次数约定：
    // - attempt=0 表示首次执行（非重试）；
    // - attempt>0 表示第 N 次重试。
    let mut attempt: u32 = 0;
    loop {
        // 在每次请求前检查取消，避免 pause/cancel 后仍继续发请求。
        if cancel.is_cancelled() {
            crate::meow_flow_log!(
                "chunk_retry",
                "cancel before transfer: key={:?} offset={} attempt={}",
                key,
                offset,
                attempt
            );
            return ChunkRetryResult::Cancelled;
        }

        // 真正执行一次分片传输。
        match executor
            .transfer_chunk(task, offset, chunk_size, known_total)
            .await
        {
            Ok(outcome) => {
                // 若经过重试后成功，额外记录一次“重试恢复成功”日志。
                if attempt > 0 {
                    crate::meow_flow_log!(
                        "chunk_retry",
                        "retry recovered: key={:?} offset={} attempts_used={} next_offset={}",
                        key,
                        offset,
                        attempt,
                        outcome.next_offset
                    );
                }
                return ChunkRetryResult::Done(outcome);
            }
            Err(err) => {
                // 决策是否可重试：由常量边界 + 剩余次数共同决定。
                let retryable = is_chunk_retryable(&err);
                let reached_limit = attempt >= max_chunk_retries;
                if !retryable || reached_limit {
                    crate::meow_flow_log!(
                        "chunk_retry",
                        "give up: key={:?} offset={} attempt={} max_retries={} retryable={} err={}",
                        key,
                        offset,
                        attempt,
                        max_chunk_retries,
                        retryable,
                        err
                    );
                    return ChunkRetryResult::Failed(err);
                }

                // 计算下一次退避时长（指数退避 + 抖动）。
                let delay_ms = calc_backoff_with_jitter_ms(attempt);
                crate::meow_flow_log!(
                    "chunk_retry",
                    "retry scheduled: key={:?} offset={} attempt={} next_delay_ms={} err={}",
                    key,
                    offset,
                    attempt + 1,
                    delay_ms,
                    err
                );

                // 在退避等待期间支持取消，保证控制语义响应及时。
                tokio::select! {
                    _ = cancel.cancelled() => {
                        crate::meow_flow_log!(
                            "chunk_retry",
                            "cancel during backoff wait: key={:?} offset={} attempt={}",
                            key,
                            offset,
                            attempt
                        );
                        return ChunkRetryResult::Cancelled;
                    }
                    _ = sleep(Duration::from_millis(delay_ms)) => {}
                }

                // 进入下一轮重试。
                attempt += 1;
            }
        }
    }
}

/// 判断分片错误是否可重试。
///
/// 目前按“错误码边界常量”判定；后续如需动态配置可在此处平滑接入配置表。
fn is_chunk_retryable(err: &MeowError) -> bool {
    RETRYABLE_CHUNK_ERROR_CODES
        .iter()
        .any(|code| *code == err.code())
}

/// 计算退避时长（指数退避 + 抖动）：
/// - base: 200ms
/// - max: 5000ms
/// - jitter: ±20%
fn calc_backoff_with_jitter_ms(attempt: u32) -> u64 {
    // 指数退避：base * 2^attempt，并限制上限。
    let exp = CHUNK_RETRY_BASE_DELAY_MS.saturating_mul(1u64 << attempt.min(20));
    let capped = exp.min(CHUNK_RETRY_MAX_DELAY_MS);

    // 抖动采用时间戳纳秒作为轻量随机源，避免额外依赖。
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    // ratio_percent 取值区间 [80, 120]（即 ±20%）。
    let jitter_span = CHUNK_RETRY_JITTER_PERCENT * 2;
    let ratio_percent = 100 - CHUNK_RETRY_JITTER_PERCENT + (nanos % (jitter_span + 1));

    capped.saturating_mul(ratio_percent) / 100
}
