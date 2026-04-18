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
}
