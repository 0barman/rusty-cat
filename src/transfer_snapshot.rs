use crate::direction::Direction;

#[derive(Debug, Clone)]
pub struct TransferSnapshot {
    pub queued_groups: usize,
    pub active_groups: usize,
    pub active_keys: Vec<(Direction, String)>,
}
