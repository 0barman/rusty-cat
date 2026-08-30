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

/// 选择本次重试驱动的底层分片操作：
/// - `Whole`：串行路径，调 [`TransferTrait::transfer_chunk`]（到达末尾时内联收尾）；
/// - `Part`：单文件并发路径，调 [`TransferTrait::transfer_chunk_part`]（**绝不**内联收尾，
///   收尾由调度器在全部分片汇合后恰好执行一次）。
///
/// 仅决定调用哪个 executor 方法；退避、取消协作、错误分类等重试语义两种模式完全一致。
#[derive(Clone, Copy)]
pub(crate) enum ChunkTransferMode {
    Whole,
    Part,
}

/// 对单个分片执行“带退避的重试”。
///
/// 设计目标：
/// 1) 将重试策略封装在独立模块，降低对 `exec.rs` 的耦合；
/// 2) 对外只暴露“成功/取消/失败”三态，简化上层流程控制；
/// 3) 在每次重试决策都打详细日志，便于线上排障。
///
/// # 参数说明
///
/// - **`executor`**：当前任务使用的传输后端（实现 [`TransferTrait`]），负责实际 HTTP/IO。
/// - **`task`**：只读任务快照（路径、URL、分片大小、协议实现等），单次分片传输的输入上下文。
/// - **`key`**：调度器中的任务键（方向 + 去重标识），用于日志关联与排障。
/// - **`cancel`**：工作协程的取消令牌；在每次发起分片前以及退避等待期间会监听，
///   取值上为有效令牌即可；取消后返回 [`ChunkRetryResult::Cancelled`]。
/// - **`offset`**：本分片在文件中的起始字节偏移；有效范围为 **`0..task.total_size()`**
///   （含已传完边界：若 `offset >= total` 应由上层结束循环，此处仍可能收到合法边界值）。
/// - **`chunk_size`**：本分片计划读取/上传的字节数；通常来自任务的 `chunk_size`，
///   有效范围为 **`>= 1`**（任务构建阶段已将 `0` 归一化）；与 `known_total` 共同决定末片长度。
/// - **`known_total`**：当前已知的文件总字节数；来自 prepare 或最近成功的分片结果，
///   有效范围为 **`0`**（未知时由上层回退）或 **`> 0`** 且应与远端/本地总大小一致，
///   用于末片裁剪与完成判定。
/// - **`max_chunk_retries`**：在**首次尝试失败之后**允许的最大重试次数；语义与退避逻辑绑定：
///   - **`0`**：不重试，任意失败立即返回 [`ChunkRetryResult::Failed`]
///   - **`N > 0`**：最多再重试 `N` 次（总尝试次数上界为 **`1 + N`**）
#[allow(clippy::too_many_arguments)]
pub(crate) async fn transfer_chunk_with_retry(
    executor: &Arc<dyn TransferTrait>,
    task: &TransferTask,
    key: &UniqueId,
    cancel: &CancellationToken,
    offset: u64,
    chunk_size: u64,
    known_total: u64,
    max_chunk_retries: u32,
    mode: ChunkTransferMode,
) -> ChunkRetryResult {
    // 尝试次数约定：
    // - attempt=0 表示首次执行（非重试）；
    // - attempt>0 表示第 N 次重试。
    let mut attempt: u32 = 0;
    loop {
        // 在每次请求前检查取消，避免 pause/cancel 后仍继续发请求。
        if cancel.is_cancelled() {
            crate::meow_trace_log!(
                "chunk_retry",
                "cancel before transfer: key={} offset={} attempt={}",
                crate::inner::safe_key(key),
                offset,
                attempt
            );
            return ChunkRetryResult::Cancelled;
        }

        // 真正执行一次分片传输。两种模式仅调用的 executor 方法不同，重试语义一致。
        let transfer = match mode {
            ChunkTransferMode::Whole => {
                executor
                    .transfer_chunk(task, offset, chunk_size, known_total)
                    .await
            }
            ChunkTransferMode::Part => {
                executor
                    .transfer_chunk_part(task, offset, chunk_size, known_total)
                    .await
            }
        };
        match transfer {
            Ok(outcome) => {
                // 若经过重试后成功，额外记录一次“重试恢复成功”日志。
                if attempt > 0 {
                    crate::meow_warn_log!(
                        "chunk_retry",
                        "retry recovered: key={} offset={} attempts_used={} next_offset={}",
                        crate::inner::safe_key(key),
                        offset,
                        attempt,
                        outcome.next_offset
                    );
                }
                return ChunkRetryResult::Done(outcome);
            }
            Err(err) => {
                // pause/cancel 可能在 in-flight 请求返回后才落到令牌上；此时错误常为 HttpError
                //（底层 Canceled/reset），应优先走取消分支而不是按网络错误重试或失败。
                let cancellation_related = cancellation_can_mask(&err);
                if cancel.is_cancelled() && cancellation_related {
                    crate::meow_trace_log!(
                        "chunk_retry",
                        "cancel after chunk error: key={} offset={} attempt={} err={}",
                        crate::inner::safe_key(key),
                        offset,
                        attempt,
                        crate::log::redact_secrets(&err.to_string())
                    );
                    return ChunkRetryResult::Cancelled;
                }
                // 决策是否可重试：由常量边界 + 剩余次数共同决定。
                let retryable = is_transport_retryable(&err);
                let reached_limit = attempt >= max_chunk_retries;
                if !retryable || reached_limit {
                    crate::log::emit_lazy(|| {
                        let mut log = crate::log::Log::error(
                            "chunk_retry",
                            format!(
                                "give up: key={} offset={} attempt={} max_retries={} retryable={} err={}",
                                crate::inner::safe_key(key),
                                offset,
                                attempt,
                                max_chunk_retries,
                                retryable,
                                crate::log::redact_secrets(&err.to_string())
                            ),
                        )
                        .with_offset(offset)
                        .with_attempt(attempt)
                        .with_max_retries(max_chunk_retries)
                        .with_error_code(err.code());
                        if let Some(status) = err.http_status() {
                            log = log.with_http_status(status);
                        }
                        log
                    });
                    return ChunkRetryResult::Failed(err);
                }

                // 计算下一次退避时长（指数退避 + 抖动）。
                let delay_ms = calc_backoff_with_jitter_ms(attempt);
                crate::meow_trace_log!(
                    "chunk_retry",
                    "retry scheduled: key={} offset={} attempt={} next_delay_ms={} err={}",
                    crate::inner::safe_key(key),
                    offset,
                    attempt + 1,
                    delay_ms,
                    crate::log::redact_secrets(&err.to_string())
                );

                // 在退避等待期间支持取消，保证控制语义响应及时。
                tokio::select! {
                    _ = cancel.cancelled() => {
                        crate::meow_trace_log!(
                            "chunk_retry",
                            "cancel during backoff wait: key={} offset={} attempt={}",
                            crate::inner::safe_key(key),
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

/// Only transport errors that can be caused by aborting an in-flight request
/// may be translated into the scheduler's `Cancelled` outcome. Local source
/// integrity failures (for example `ChecksumMismatch`) must remain failures even
/// when a pause/cancel races their return, so upload cleanup still aborts the
/// provider session instead of publishing a resumable pause.
pub(crate) fn cancellation_can_mask(error: &MeowError) -> bool {
    matches!(
        error.code(),
        code if code == InnerErrorCode::HttpError as i32
            || code == InnerErrorCode::TaskCanceled as i32
    )
}

/// 判断网络/HTTP 层瞬时错误是否可按与分片相同的策略重试（prepare / chunk 共用）。
///
/// 目前按“错误码边界常量”判定；后续如需动态配置可在此处平滑接入配置表。
pub(crate) fn is_transport_retryable(err: &MeowError) -> bool {
    let code = err.code();
    if !RETRYABLE_CHUNK_ERROR_CODES.contains(&code) {
        return false;
    }
    // 当错误来自非 2xx 响应时会带上 HTTP 状态码：硬客户端错误（多数 4xx）重发同一请求也
    // 无法恢复，故快速失败；瞬时状态（5xx 及 408/429）与“无状态码”的错误保持可重试，
    // 不改变既有行为。
    if code == InnerErrorCode::ResponseStatusError as i32 {
        if let Some(status) = err.http_status() {
            return is_retryable_http_status(status);
        }
    }
    true
}

/// 判断某个 HTTP 响应状态码是否值得重试。
///
/// 可重试：服务端瞬时失败（5xx），以及两个“稍后重试可能成功”的状态
/// `408 Request Timeout` 与 `429 Too Many Requests`。其余客户端错误（4xx）视为
/// 不可恢复并快速失败；非 2xx 的 3xx 同样按不可重试处理。
pub(crate) fn is_retryable_http_status(status: u16) -> bool {
    matches!(status, 408 | 429 | 500..=599)
}

/// 仅连接/请求栈层面的错误（[`InnerErrorCode::HttpError`]），用于 `prepare` 外层的有限重试：
/// 避免把业务态的 [`InnerErrorCode::ResponseStatusError`]（如上传 prepare 返回 5xx）当成可重试，
/// 否则在单响应脚本/服务端已关连接时会退化成下一次尝试的 `HttpError`，掩盖真实错误码。
pub(crate) fn is_connection_layer_retryable(err: &MeowError) -> bool {
    err.code() == InnerErrorCode::HttpError as i32
}

/// 计算退避时长（指数退避 + 抖动）：
/// - base: 200ms
/// - max: 5000ms
/// - jitter: ±20%
///
/// `attempt` 为从 0 起的重试轮次索引，与分片重试模块内语义一致。
pub(crate) fn calc_backoff_with_jitter_ms(attempt: u32) -> u64 {
    // 指数退避：base * 2^attempt，并限制上限。
    let exp = CHUNK_RETRY_BASE_DELAY_MS.saturating_mul(1u64 << attempt.min(20));
    let capped = exp.min(CHUNK_RETRY_MAX_DELAY_MS);

    // 抖动采用时间戳纳秒作为轻量随机源，避免额外依赖。
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or_else(|_| {
            crate::meow_warn_log!(
                "chunk_retry",
                "system clock before unix epoch: backoff jitter source defaulted to 0"
            );
            0
        });
    // ratio_percent 取值区间 [80, 120]（即 ±20%）。
    let jitter_span = CHUNK_RETRY_JITTER_PERCENT * 2;
    let ratio_percent = 100 - CHUNK_RETRY_JITTER_PERCENT + (nanos % (jitter_span + 1));

    capped.saturating_mul(ratio_percent) / 100
}

#[cfg(test)]
mod tests {
    use super::{cancellation_can_mask, is_retryable_http_status, is_transport_retryable};
    use crate::error::{InnerErrorCode, MeowError};

    fn response_status_err(status: Option<u16>) -> MeowError {
        let err = MeowError::from_code(InnerErrorCode::ResponseStatusError, "status".to_string());
        match status {
            Some(s) => err.with_http_status(s),
            None => err,
        }
    }

    #[test]
    fn retryable_status_covers_5xx_and_throttle_codes() {
        for s in [500u16, 502, 503, 504, 408, 429] {
            assert!(is_retryable_http_status(s), "{s} should be retryable");
        }
    }

    #[test]
    fn non_retryable_status_covers_hard_4xx_and_3xx() {
        for s in [400u16, 401, 403, 404, 409, 410, 422, 301, 302] {
            assert!(!is_retryable_http_status(s), "{s} should not be retryable");
        }
    }

    #[test]
    fn transport_retryable_fast_fails_hard_4xx_response_status() {
        assert!(!is_transport_retryable(&response_status_err(Some(403))));
        assert!(!is_transport_retryable(&response_status_err(Some(404))));
    }

    #[test]
    fn transport_retryable_keeps_5xx_and_throttle_response_status() {
        assert!(is_transport_retryable(&response_status_err(Some(500))));
        assert!(is_transport_retryable(&response_status_err(Some(503))));
        assert!(is_transport_retryable(&response_status_err(Some(429))));
    }

    #[test]
    fn transport_retryable_keeps_status_less_response_status() {
        // Backward compatible: a ResponseStatusError without an attached status
        // (e.g. produced by a non-HTTP code path) stays retryable as before.
        assert!(is_transport_retryable(&response_status_err(None)));
    }

    #[test]
    fn transport_retryable_keeps_http_error_and_rejects_others() {
        // Connection-layer HttpError stays retryable regardless of status.
        assert!(is_transport_retryable(&MeowError::from_code1(
            InnerErrorCode::HttpError
        )));
        // A code outside the retryable set is never retried.
        assert!(!is_transport_retryable(&MeowError::from_code1(
            InnerErrorCode::InvalidRange
        )));
    }

    #[test]
    fn cancellation_never_masks_local_integrity_failure() {
        assert!(!cancellation_can_mask(&MeowError::from_code1(
            InnerErrorCode::ChecksumMismatch
        )));
        assert!(!cancellation_can_mask(&MeowError::from_code1(
            InnerErrorCode::LocalFileRemoved
        )));
        assert!(cancellation_can_mask(&MeowError::from_code1(
            InnerErrorCode::HttpError
        )));
        assert!(cancellation_can_mask(&MeowError::from_code1(
            InnerErrorCode::TaskCanceled
        )));
    }
}
