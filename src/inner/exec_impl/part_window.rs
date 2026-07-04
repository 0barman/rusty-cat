//! Windowed dispatch + contiguous-prefix accounting for intra-file parallel
//! parts (optimization item ④).
//!
//! This is the *pure* core of the opt-in parallel upload path. It decides which
//! part offsets to dispatch (bounded to `max_in_flight`) and reduces
//! out-of-order part completions to a single monotonic contiguous watermark, so
//! the executor only ever persists/emits a true contiguous prefix and never a
//! hole. It performs no I/O and holds no async state, so the dangerous
//! correctness logic (watermark never skips a gap, last short part clamps to
//! total, completion fires only when fully contiguous AND nothing in flight) can
//! be exhaustively unit-tested in isolation from the HTTP/JoinSet shell.

use std::collections::BTreeSet;

use crate::{InnerErrorCode, MeowError};

/// Bounded windowed dispatcher + contiguous-prefix tracker for one file's
/// parallel parts. Pure logic, no I/O.
pub(crate) struct PartWindow {
    /// Chunk size; every dispatched part starts at a `start_offset + k*chunk`
    /// boundary, matching the protocol's offset-derived part identity.
    chunk: u64,
    /// Total bytes to upload; the last part is clamped to `total`.
    total: u64,
    /// Max parts allowed in flight simultaneously (always `>= 1`).
    max_in_flight: usize,
    /// Next offset to hand out; advances by `chunk`, clamped to `total`.
    next_dispatch: u64,
    /// Completed part *start* offsets not yet absorbed into the watermark.
    completed: BTreeSet<u64>,
    /// Largest contiguous prefix end measured from the original start offset.
    watermark: u64,
    /// Parts currently dispatched but not yet settled (done/failed/cancelled).
    in_flight: usize,
}

impl PartWindow {
    /// Creates a window resuming from `start_offset` (the contiguous prefix that
    /// `prepare` already established), bounded to `max_in_flight` parts.
    pub(crate) fn new(start_offset: u64, chunk: u64, total: u64, max_in_flight: usize) -> Self {
        let start = start_offset.min(total);
        Self {
            chunk: chunk.max(1),
            total,
            max_in_flight: max_in_flight.max(1),
            next_dispatch: start,
            completed: BTreeSet::new(),
            watermark: start,
            in_flight: 0,
        }
    }

    /// Returns the next part offset to dispatch, or `None` when the window is
    /// full or every byte has been handed out. Increments the in-flight count.
    pub(crate) fn take_dispatch(&mut self) -> Option<u64> {
        if self.in_flight >= self.max_in_flight || self.next_dispatch >= self.total {
            return None;
        }
        let off = self.next_dispatch;
        self.next_dispatch = (off + self.chunk).min(self.total);
        self.in_flight += 1;
        Some(off)
    }

    /// Records a successfully-completed part by its start offset, then advances
    /// the contiguous watermark over any now-contiguous completed parts.
    ///
    /// Returns `Ok(Some(watermark))` iff the watermark advanced (so the caller
    /// emits exactly one Progress); `Ok(None)` means the part landed above a
    /// still-open hole. Returns `Err` if called without a matching dispatch
    /// (`in_flight == 0`) — an internal accounting violation surfaced as an
    /// error instead of a panic, so the executor can fail the task gracefully.
    pub(crate) fn on_done(&mut self, offset: u64) -> Result<Option<u64>, MeowError> {
        if self.in_flight == 0 {
            return Err(MeowError::from_code_str(
                InnerErrorCode::InvalidTaskState,
                "internal: on_done called without a matching dispatch",
            ));
        }
        self.in_flight -= 1;
        self.completed.insert(offset);
        let before = self.watermark;
        while self.watermark < self.total && self.completed.remove(&self.watermark) {
            self.watermark = (self.watermark + self.chunk).min(self.total);
        }
        if self.watermark != before {
            Ok(Some(self.watermark))
        } else {
            Ok(None)
        }
    }

    /// Records a part that settled without success (cancelled/failed): frees its
    /// in-flight slot so the join barrier can quiesce, without moving the
    /// watermark.
    pub(crate) fn on_settled_without_progress(&mut self) {
        self.in_flight = self.in_flight.saturating_sub(1);
    }

    /// Current contiguous prefix end — the only value safe to persist/emit.
    pub(crate) fn watermark(&self) -> u64 {
        self.watermark
    }

    /// Whether every byte is uploaded as a contiguous prefix AND nothing is in
    /// flight — the only condition under which `complete` may fire exactly once.
    pub(crate) fn is_complete(&self) -> bool {
        self.watermark >= self.total && self.in_flight == 0
    }
}

#[cfg(test)]
mod tests {
    use super::PartWindow;

    // --- dispatch windowing ---------------------------------------------

    #[test]
    fn dispatch_hands_out_aligned_offsets_up_to_total() {
        // total=25, chunk=10 => parts at 0,10,20 (last is short: 5 bytes).
        let mut w = PartWindow::new(0, 10, 25, 8);
        assert_eq!(w.take_dispatch(), Some(0));
        assert_eq!(w.take_dispatch(), Some(10));
        assert_eq!(w.take_dispatch(), Some(20));
        // Nothing left to dispatch (next_dispatch reached total).
        assert_eq!(w.take_dispatch(), None);
    }

    #[test]
    fn dispatch_is_bounded_by_max_in_flight() {
        let mut w = PartWindow::new(0, 10, 1000, 2);
        assert_eq!(w.take_dispatch(), Some(0));
        assert_eq!(w.take_dispatch(), Some(10));
        // Window full at 2 in flight.
        assert_eq!(w.take_dispatch(), None);
        // Completing one frees a slot; next offset continues the sequence.
        assert_eq!(w.on_done(0).unwrap(), Some(10));
        assert_eq!(w.take_dispatch(), Some(20));
    }

    // --- contiguous watermark (the core anti-hole invariant) ------------

    #[test]
    fn in_order_completion_advances_watermark_each_step() {
        let mut w = PartWindow::new(0, 10, 30, 8);
        w.take_dispatch();
        w.take_dispatch();
        w.take_dispatch();
        assert_eq!(w.on_done(0).unwrap(), Some(10));
        assert_eq!(w.on_done(10).unwrap(), Some(20));
        assert_eq!(w.on_done(20).unwrap(), Some(30));
        assert!(w.is_complete());
    }

    #[test]
    fn out_of_order_high_part_first_does_not_advance_past_a_hole() {
        let mut w = PartWindow::new(0, 10, 30, 8);
        w.take_dispatch(); // 0
        w.take_dispatch(); // 10
        w.take_dispatch(); // 20
        // Highest part lands first: watermark must NOT move (hole at 0..20).
        assert_eq!(w.on_done(20).unwrap(), None);
        assert_eq!(w.watermark(), 0);
        assert!(!w.is_complete());
        // Middle part lands: still a hole at 0..10.
        assert_eq!(w.on_done(10).unwrap(), None);
        assert_eq!(w.watermark(), 0);
        // First part fills the gap: watermark jumps over ALL contiguous parts.
        assert_eq!(w.on_done(0).unwrap(), Some(30));
        assert_eq!(w.watermark(), 30);
        assert!(w.is_complete());
    }

    #[test]
    fn short_last_part_clamps_watermark_to_total() {
        // total=25, chunk=10 => last part starts at 20, size 5.
        let mut w = PartWindow::new(0, 10, 25, 8);
        w.take_dispatch();
        w.take_dispatch();
        w.take_dispatch();
        w.on_done(0).unwrap();
        w.on_done(10).unwrap();
        // Completing the short last part must reach exactly total, not 30.
        assert_eq!(w.on_done(20).unwrap(), Some(25));
        assert_eq!(w.watermark(), 25);
        assert!(w.is_complete());
    }

    // --- completion gating ----------------------------------------------

    #[test]
    fn is_complete_requires_watermark_total_and_nothing_in_flight() {
        let mut w = PartWindow::new(0, 10, 20, 8);
        w.take_dispatch(); // 0
        w.take_dispatch(); // 10
        // First completes, watermark=10, but a part is still in flight.
        w.on_done(0).unwrap();
        assert!(!w.is_complete());
        // Second completes, watermark=20 and in_flight back to 0.
        w.on_done(10).unwrap();
        assert!(w.is_complete());
    }

    #[test]
    fn settled_without_progress_frees_a_slot_but_does_not_advance() {
        let mut w = PartWindow::new(0, 10, 30, 1);
        assert_eq!(w.take_dispatch(), Some(0));
        assert_eq!(w.take_dispatch(), None); // window full (max 1)
        // A failed/cancelled part settles: the slot is freed (so the next offset
        // becomes dispatchable) and the watermark does NOT advance.
        w.on_settled_without_progress();
        assert_eq!(w.watermark(), 0);
        assert_eq!(w.take_dispatch(), Some(10));
    }

    // --- resume (start_offset != 0) -------------------------------------

    #[test]
    fn resume_starts_dispatch_and_watermark_at_start_offset() {
        // Resuming a 30-byte upload past the first chunk.
        let mut w = PartWindow::new(10, 10, 30, 8);
        assert_eq!(w.watermark(), 10);
        assert_eq!(w.take_dispatch(), Some(10));
        assert_eq!(w.take_dispatch(), Some(20));
        assert_eq!(w.take_dispatch(), None);
        assert_eq!(w.on_done(10).unwrap(), Some(20));
        assert_eq!(w.on_done(20).unwrap(), Some(30));
        assert!(w.is_complete());
    }

    #[test]
    fn already_complete_resume_is_complete_with_no_dispatch() {
        let mut w = PartWindow::new(30, 10, 30, 8);
        assert_eq!(w.take_dispatch(), None);
        assert!(w.is_complete());
    }
}
