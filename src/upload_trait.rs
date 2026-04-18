use async_trait::async_trait;
use crate::http_breakpoint::UploadResumeInfo;
use crate::{MeowError, TransferTask};

/// Context for upload prepare stage.
#[derive(Debug, Clone, Copy)]
pub struct UploadPrepareCtx<'a> {
    /// HTTP client used for requests.
    pub client: &'a reqwest::Client,
    /// Immutable task snapshot.
    pub task: &'a TransferTask,
    /// Locally confirmed uploaded offset in bytes.
    ///
    /// Range: `>= 0`.
    pub local_offset: u64,
}

/// Context for upload chunk stage.
#[derive(Debug, Clone, Copy)]
pub struct UploadChunkCtx<'a> {
    /// HTTP client used for requests.
    pub client: &'a reqwest::Client,
    /// Immutable task snapshot.
    pub task: &'a TransferTask,
    /// Raw bytes for the current chunk.
    pub chunk: &'a [u8],
    /// Start offset of this chunk in the full file.
    ///
    /// Range: `>= 0`.
    pub offset: u64,
}

/// Custom breakpoint upload protocol.
///
/// Implementors are responsible for request construction and response parsing.
/// The executor handles file I/O, chunking, retries, progress, and scheduling.
///
/// # Examples
///
/// ```no_run
/// use async_trait::async_trait;
/// use rusty_cat::api::{
///     BreakpointUpload, MeowError, UploadChunkCtx, UploadPrepareCtx, UploadResumeInfo,
/// };
///
/// struct MyUploadProtocol;
///
/// #[async_trait]
/// impl BreakpointUpload for MyUploadProtocol {
///     async fn prepare(&self, _ctx: UploadPrepareCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
///         Ok(UploadResumeInfo::default())
///     }
///
///     async fn upload_chunk(&self, ctx: UploadChunkCtx<'_>) -> Result<UploadResumeInfo, MeowError> {
///         let _ = (ctx.task.file_name(), ctx.offset);
///         Ok(UploadResumeInfo {
///             completed_file_id: None,
///             next_byte: Some(ctx.offset + ctx.chunk.len() as u64),
///         })
///     }
/// }
/// ```
///
/// # Executor integration contract
///
/// - If [`UploadResumeInfo::completed_file_id`] is `Some`, the executor treats
///   the upload as fully completed and stops sending further chunks.
/// - [`UploadResumeInfo::next_byte`] is a server-suggested next offset; the
///   executor merges it with local offset before continuing.
/// - When computed next offset reaches `task.total_size()`, the executor calls
///   [`BreakpointUpload::complete_upload`] unless completion was already
///   indicated by `completed_file_id`.
/// - When user cancels an upload task, executor calls
///   [`BreakpointUpload::abort_upload`].
#[async_trait]
pub trait BreakpointUpload: Send + Sync {
    /// Prepare stage before first chunk upload.
    ///
    /// Typical responsibilities include creating upload session and querying
    /// already uploaded offset on remote side.
    ///
    /// # Parameters
    ///
    /// - `client`: Shared HTTP client used by executor.
    /// - `task`: Upload task snapshot with URL/method/headers/file metadata.
    /// - `local_offset`: Locally confirmed uploaded offset.
    ///
    /// # Returns
    ///
    /// - `Ok(info)`: Server resume info used by executor to compute next offset.
    /// - `Err`: Prepare failed and task enters error path.
    ///
    /// # Errors
    ///
    /// Return [`MeowError`] when remote session creation/checkpoint probing
    /// fails, request signing fails, or protocol payload parsing fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::UploadPrepareCtx;
    ///
    /// fn read_prepare_ctx(ctx: UploadPrepareCtx<'_>) {
    ///     let _ = (ctx.task.url(), ctx.local_offset);
    /// }
    /// ```
    async fn prepare(&self, ctx: UploadPrepareCtx<'_>) -> Result<UploadResumeInfo, MeowError>;

    /// Uploads a single chunk.
    ///
    /// Executor guarantees chunk bounds are valid and chunk bytes match the
    /// provided `offset`.
    ///
    /// # Errors
    ///
    /// Return [`MeowError`] when chunk request fails, server rejects the chunk,
    /// or protocol response parsing fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::UploadChunkCtx;
    ///
    /// fn read_chunk_ctx(ctx: UploadChunkCtx<'_>) {
    ///     let _ = (ctx.offset, ctx.chunk.len(), ctx.task.total_size());
    /// }
    /// ```
    async fn upload_chunk(&self, ctx: UploadChunkCtx<'_>) -> Result<UploadResumeInfo, MeowError>;

    /// Finalization step after all chunk bytes are uploaded.
    ///
    /// Typical use case: multipart-complete API calls. Return value is an
    /// optional provider-defined payload that will be forwarded to
    /// `MeowClient::enqueue` complete callback.
    ///
    /// Default implementation is a no-op (`Ok(None)`).
    ///
    /// # Errors
    ///
    /// Implementations should return [`MeowError`] if final commit/merge API
    /// fails.
    async fn complete_upload(
        &self,
        _client: &reqwest::Client,
        _task: &TransferTask,
    ) -> Result<Option<String>, MeowError> {
        Ok(None)
    }

    /// Abort/cleanup hook called when user cancels an upload task.
    ///
    /// Typical use case: abort multipart session or remove temporary objects.
    /// Default implementation is a no-op.
    ///
    /// # Errors
    ///
    /// Implementations should return [`MeowError`] when cleanup or abort API
    /// calls fail.
    async fn abort_upload(
        &self,
        _client: &reqwest::Client,
        _task: &TransferTask,
    ) -> Result<(), MeowError> {
        Ok(())
    }
}
