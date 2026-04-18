use crate::direction::Direction;

/// Lightweight runtime snapshot for monitoring current scheduler state.
#[derive(Debug, Clone)]
pub struct TransferSnapshot {
    /// Number of queued transfer groups waiting to run.
    ///
    /// Range: `>= 0`.
    pub queued_groups: usize,
    /// Number of active transfer groups currently running.
    ///
    /// Range: `>= 0`.
    pub active_groups: usize,
    /// Keys of active groups as `(direction, dedup_key)`.
    pub active_keys: Vec<(Direction, String)>,
}
