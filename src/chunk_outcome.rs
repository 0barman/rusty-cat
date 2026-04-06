#[derive(Debug, Clone, Copy)]
pub struct ChunkOutcome {
    pub next_offset: u64,
    pub total_size: u64,
    pub done: bool,
}
