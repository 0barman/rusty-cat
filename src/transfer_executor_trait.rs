use async_trait::async_trait;

use crate::chunk_outcome::ChunkOutcome;
use crate::error::MeowError;
use crate::prepare_outcome::PrepareOutcome;
use crate::transfer_task::TransferTask;

/// Low-level transfer executor abstraction used by scheduler/runtime.
///
/// Most users do not implement this trait directly unless they are building a
/// custom transport backend.
///
/// # Examples
///
/// ```no_run
/// use async_trait::async_trait;
/// use rusty_cat::api::{ChunkOutcome, MeowError, PrepareOutcome, TransferTask, TransferTrait};
///
/// struct NoopExecutor;
///
/// #[async_trait]
/// impl TransferTrait for NoopExecutor {
///     async fn prepare(
///         &self,
///         _task: &TransferTask,
///         local_offset: u64,
///     ) -> Result<PrepareOutcome, MeowError> {
///         Ok(PrepareOutcome { next_offset: local_offset, total_size: local_offset })
///     }
///
///     async fn transfer_chunk(
///         &self,
///         _task: &TransferTask,
///         offset: u64,
///         _chunk_size: u64,
///         remote_total_size: u64,
///     ) -> Result<ChunkOutcome, MeowError> {
///         Ok(ChunkOutcome {
///             next_offset: offset,
///             total_size: remote_total_size,
///             done: true,
///             completion_payload: None,
///         })
///     }
/// }
/// ```
#[async_trait]
pub trait TransferTrait: Send + Sync {
    /// Prepares transfer state and computes the next offset to run.
    ///
    /// # Parameters
    ///
    /// - `task`: Immutable task snapshot.
    /// - `local_offset`: Current local persisted offset, in bytes.
    ///
    /// # Returns
    ///
    /// Returns [`PrepareOutcome`] containing next offset and total size.
    ///
    /// # Errors
    ///
    /// Return [`MeowError`] when checkpoint probing, local/remote validation,
    /// or protocol initialization fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::TransferTask;
    ///
    /// fn inspect_prepare_input(task: &TransferTask, local_offset: u64) {
    ///     let _ = (task.url(), local_offset);
    /// }
    /// ```
    async fn prepare(
        &self,
        task: &TransferTask,
        local_offset: u64,
    ) -> Result<PrepareOutcome, MeowError>;

    /// Transfers one chunk from the given offset.
    ///
    /// # Parameters
    ///
    /// - `offset`: Start byte offset for this chunk.
    /// - `chunk_size`: Desired chunk size in bytes (`>= 1` recommended).
    /// - `remote_total_size`: For download, use `prepare.total_size`; for
    ///   upload, usually equals `task.total_size()`.
    ///
    /// # Errors
    ///
    /// Return [`MeowError`] when chunk transfer fails, range validation fails,
    /// or local file I/O fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::TransferTask;
    ///
    /// fn inspect_chunk_input(task: &TransferTask, offset: u64, chunk_size: u64, remote_total_size: u64) {
    ///     let _ = (task.file_name(), offset, chunk_size, remote_total_size);
    /// }
    /// ```
    async fn transfer_chunk(
        &self,
        task: &TransferTask,
        offset: u64,
        chunk_size: u64,
        remote_total_size: u64,
    ) -> Result<ChunkOutcome, MeowError>;

    /// Handles protocol-specific cancel semantics.
    ///
    /// Default implementation is a no-op.
    ///
    /// # Errors
    ///
    /// Implementations should return [`MeowError`] if remote abort/cancel
    /// actions fail.
    async fn cancel(&self, _task: &TransferTask) -> Result<(), MeowError> {
        Ok(())
    }

    /// Whether this executor may transfer chunks of `task` concurrently and out
    /// of order (intra-file parallel parts).
    ///
    /// Default `false` keeps every executor on the strict-serial path. When this
    /// returns `true`, the scheduler may dispatch up to `max_parts_in_flight`
    /// chunks of the same file at once and finalize the upload via [`Self::complete`]
    /// instead of letting any single chunk finalize inline.
    ///
    /// **Contract:** an executor that returns `true` here MUST also override
    /// [`Self::transfer_chunk_part`] (to upload a chunk WITHOUT finalizing) and
    /// [`Self::complete`] (to finalize exactly once). Leaving those at their
    /// defaults while returning `true` would finalize the upload from whichever
    /// chunk reaches the end first, corrupting out-of-order uploads.
    fn supports_parallel_parts(&self, _task: &TransferTask) -> bool {
        false
    }

    /// Transfers one chunk WITHOUT finalizing the transfer.
    ///
    /// Used by the parallel-parts path: completion is hoisted to a single
    /// [`Self::complete`] call made by the scheduler after every part has been
    /// uploaded as a contiguous prefix, so individual chunks must not finalize.
    ///
    /// The default delegates to [`Self::transfer_chunk`], which is correct for
    /// executors that do not opt into parallel parts (they never take this path);
    /// executors returning `true` from [`Self::supports_parallel_parts`] MUST
    /// override this to skip finalization.
    ///
    /// # Errors
    ///
    /// Same as [`Self::transfer_chunk`].
    async fn transfer_chunk_part(
        &self,
        task: &TransferTask,
        offset: u64,
        chunk_size: u64,
        remote_total_size: u64,
    ) -> Result<ChunkOutcome, MeowError> {
        self.transfer_chunk(task, offset, chunk_size, remote_total_size)
            .await
    }

    /// Finalizes a transfer after all parts have been uploaded (parallel path).
    ///
    /// Called exactly once by the scheduler once the whole file has arrived as a
    /// contiguous prefix and every in-flight part has joined. The default is a
    /// no-op (`Ok(None)`); the bundled executor delegates to the upload
    /// protocol's completion step.
    ///
    /// # Errors
    ///
    /// Return [`MeowError`] when the final commit/merge call fails.
    async fn complete(&self, _task: &TransferTask) -> Result<Option<String>, MeowError> {
        Ok(None)
    }
}
