/// Output of [`crate::transfer_executor_trait::TransferTrait::prepare`].
#[derive(Debug, Clone, Copy)]
pub struct PrepareOutcome {
    /// Next transfer offset in bytes.
    ///
    /// Range: `0..=total_size`.
    pub next_offset: u64,
    /// Known total resource size in bytes.
    ///
    /// Range: `>= 0`.
    pub total_size: u64,
}
