use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::error::{InnerErrorCode, MeowError};

const MEMORY_LIMIT_32_BIT: u64 = 64 * 1024 * 1024;
const MEMORY_LIMIT_64_BIT: u64 = 512 * 1024 * 1024;

/// Returns the aggregate parallel-body budget for a client executor.
///
/// Mobile 32-bit processes have a much smaller address space and are more
/// vulnerable to fragmentation, so they deliberately use one eighth of the
/// 64-bit cap. Keeping this as a pure function lets a native 64-bit test prove
/// the 32-bit policy without requiring a runnable 32-bit target.
pub(crate) const fn memory_limit_for_pointer_width(pointer_width: u32) -> u64 {
    if pointer_width <= 32 {
        MEMORY_LIMIT_32_BIT
    } else {
        MEMORY_LIMIT_64_BIT
    }
}

pub(crate) const fn current_memory_limit_bytes() -> u64 {
    memory_limit_for_pointer_width(usize::BITS)
}

/// Client-scoped byte semaphore shared by every active parallel file.
///
/// Each part obtains all bytes atomically before it reads/buffers its body, so
/// a task never holds a partial allocation while waiting for the rest. Owned
/// permits make success, cancellation and task panic follow the same RAII
/// release path.
#[derive(Clone, Debug)]
pub(crate) struct ParallelMemoryBudget {
    semaphore: Arc<Semaphore>,
    limit_bytes: u64,
}

impl ParallelMemoryBudget {
    pub(crate) fn for_current_target() -> Self {
        Self::with_limit(current_memory_limit_bytes())
    }

    fn with_limit(limit_bytes: u64) -> Self {
        let capped = limit_bytes
            .min(Semaphore::MAX_PERMITS as u64)
            .min(u32::MAX as u64);
        let limit = match usize::try_from(capped) {
            Ok(limit) => limit,
            // `Semaphore::MAX_PERMITS` is itself a usize, so the clamp above
            // makes this branch unreachable on supported targets. Keep it
            // non-panicking for defensive portability.
            Err(_) => Semaphore::MAX_PERMITS,
        };
        Self {
            semaphore: Arc::new(Semaphore::new(limit)),
            limit_bytes: limit as u64,
        }
    }

    #[cfg(test)]
    fn with_limit_for_test(limit_bytes: u64) -> Self {
        Self::with_limit(limit_bytes)
    }

    pub(crate) fn limit_bytes(&self) -> u64 {
        self.limit_bytes
    }

    /// Acquires the bytes for one part, or returns `Ok(None)` when its task was
    /// canceled while waiting. No control path needs to close the semaphore.
    pub(crate) async fn acquire(
        &self,
        bytes: u64,
        cancel: &CancellationToken,
    ) -> Result<Option<OwnedSemaphorePermit>, MeowError> {
        if bytes > self.limit_bytes {
            return Err(MeowError::from_code(
                InnerErrorCode::IoError,
                format!(
                    "parallel part memory exceeds client limit: requested={bytes} limit={}",
                    self.limit_bytes
                ),
            ));
        }
        let permits = u32::try_from(bytes).map_err(|_| {
            MeowError::from_code(
                InnerErrorCode::IoError,
                format!("parallel part memory permit count does not fit u32: {bytes}"),
            )
        })?;
        let semaphore = Arc::clone(&self.semaphore);
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Ok(None),
            acquired = semaphore.acquire_many_owned(permits) => acquired
                .map(Some)
                .map_err(|_| MeowError::from_code_str(
                    InnerErrorCode::IoError,
                    "parallel memory limiter closed unexpectedly",
                )),
        }
    }

    #[cfg(test)]
    fn available_permits_for_test(&self) -> usize {
        self.semaphore.available_permits()
    }
}

#[cfg(test)]
mod tests {
    use super::{memory_limit_for_pointer_width, ParallelMemoryBudget};
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn thirty_two_bit_targets_use_a_lower_memory_cap() {
        let cap_32 = memory_limit_for_pointer_width(32);
        let cap_64 = memory_limit_for_pointer_width(64);

        assert_eq!(cap_32, 64 * 1024 * 1024);
        assert_eq!(cap_64, 512 * 1024 * 1024);
        assert!(cap_32 < cap_64);
    }

    #[test]
    fn oversized_internal_limit_is_clamped_without_panicking() {
        let budget = ParallelMemoryBudget::with_limit_for_test(u64::MAX);
        assert_eq!(
            budget.limit_bytes(),
            (tokio::sync::Semaphore::MAX_PERMITS as u64).min(u32::MAX as u64)
        );
    }

    #[tokio::test]
    async fn oversized_part_is_rejected_instead_of_waiting_forever() {
        let budget = ParallelMemoryBudget::with_limit_for_test(8);
        let error = tokio::time::timeout(
            Duration::from_secs(1),
            budget.acquire(9, &CancellationToken::new()),
        )
        .await
        .expect("oversized acquisition must fail immediately")
        .expect_err("part exceeds client budget");

        assert_eq!(error.code(), crate::InnerErrorCode::IoError as i32);
        assert_eq!(budget.available_permits_for_test(), 8);
    }

    #[tokio::test]
    async fn cloned_budget_limits_multiple_active_files_and_releases_permits() {
        let budget = ParallelMemoryBudget::with_limit_for_test(10);
        let first = budget
            .acquire(7, &CancellationToken::new())
            .await
            .expect("acquire")
            .expect("not canceled");
        let waiter_budget = budget.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let waiter = tokio::spawn(async move {
            let _ = started_tx.send(());
            waiter_budget.acquire(4, &CancellationToken::new()).await
        });

        started_rx.await.expect("waiter reached acquisition");
        assert!(
            !waiter.is_finished(),
            "the shared byte cap must block the second worker"
        );
        drop(first);
        let second = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter must make progress after permit release")
            .expect("waiter task")
            .expect("acquire result")
            .expect("not canceled");
        drop(second);
        assert_eq!(budget.available_permits_for_test(), 10);
    }

    #[tokio::test]
    async fn cancellation_while_waiting_does_not_leak_or_deadlock() {
        let budget = ParallelMemoryBudget::with_limit_for_test(8);
        let holder = budget
            .acquire(8, &CancellationToken::new())
            .await
            .expect("acquire")
            .expect("not canceled");
        let cancel = CancellationToken::new();
        let waiter_budget = budget.clone();
        let waiter_cancel = cancel.clone();
        let waiter = tokio::spawn(async move { waiter_budget.acquire(1, &waiter_cancel).await });

        cancel.cancel();
        assert!(tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("canceled waiter must return")
            .expect("waiter task")
            .expect("acquire result")
            .is_none());
        drop(holder);
        assert_eq!(budget.available_permits_for_test(), 8);
    }

    #[tokio::test]
    async fn task_panic_releases_owned_byte_permit() {
        let budget = ParallelMemoryBudget::with_limit_for_test(8);
        let panic_budget = budget.clone();
        let joined = tokio::spawn(async move {
            let _permit = panic_budget
                .acquire(8, &CancellationToken::new())
                .await
                .expect("acquire")
                .expect("not canceled");
            panic!("fixture panic while holding memory budget");
        })
        .await;

        assert!(joined.is_err());
        assert_eq!(budget.available_permits_for_test(), 8);
    }
}
