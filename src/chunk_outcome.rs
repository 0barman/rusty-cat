/// Result of processing a single upload/download chunk.
///
/// # Examples
///
/// ```
/// use rusty_cat::chunk_outcome::ChunkOutcome;
///
/// let o = ChunkOutcome {
///     next_offset: 1024,
///     total_size: 4096,
///     done: false,
///     completion_payload: None,
/// };
/// assert!(!o.done);
/// ```
#[derive(Debug, Clone)]
pub struct ChunkOutcome {
    /// Next byte offset to continue transfer from.
    ///
    /// Range: `0..=total_size`.
    pub next_offset: u64,
    /// Total remote or local file size used by current transfer session.
    ///
    /// Range: `>= 0`.
    pub total_size: u64,
    /// Whether the transfer is fully completed after this chunk.
    pub done: bool,
    /// Optional payload attached when the transfer reaches completion.
    ///
    /// For upload tasks this is populated from upload protocol
    /// `complete_upload` return value; download tasks usually keep `None`.
    pub completion_payload: Option<String>,
}
