//! The segment table: which byte ranges exist, who works on them, and how far
//! each one got.
//!
//! Each segment tracks two positions. `reserved` is how far a connection has
//! claimed data (received, maybe still in its write buffer); `written` is how
//! far data is on disk. Splitting uses `reserved`, so a split never hands out
//! bytes a connection already holds; checkpoints use `written`, so a resume
//! never trusts bytes that were not stored.

use std::sync::Arc;

use parking_lot::{Mutex, RwLock};

use crate::snapshot::SegmentSnapshot;
use crate::storage::control::SavedSegment;

#[derive(Debug)]
pub(crate) struct Segment {
    start: u64,
    state: Mutex<SegmentState>,
}

#[derive(Debug)]
struct SegmentState {
    /// Exclusive. Shrinks when another connection takes over the tail.
    end: u64,
    reserved: u64,
    written: u64,
    claimed: bool,
}

impl Segment {
    fn new(start: u64, end: u64, written: u64, claimed: bool) -> Self {
        let state = SegmentState {
            end,
            reserved: written,
            written,
            claimed,
        };
        Self {
            start,
            state: Mutex::new(state),
        }
    }

    /// Starts a new attempt: forgets reservations of a failed one and returns
    /// the range still missing, or `None` when the segment is complete.
    pub fn begin(&self) -> Option<(u64, u64)> {
        let mut state = self.state.lock();
        state.reserved = state.written;
        let position = self.start + state.written;
        (position < state.end).then_some((position, state.end))
    }

    /// Claims up to `len` incoming bytes; fewer when the end moved.
    pub fn reserve(&self, len: usize) -> usize {
        let mut state = self.state.lock();
        let free = state.end.saturating_sub(self.start + state.reserved);
        let accepted = (len as u64).min(free);
        state.reserved += accepted;
        accepted as usize
    }

    pub fn fully_reserved(&self) -> bool {
        let state = self.state.lock();
        self.start + state.reserved >= state.end
    }

    /// Offset where the next buffered bytes belong.
    pub fn write_offset(&self) -> u64 {
        self.start + self.state.lock().written
    }

    pub fn commit(&self, len: usize) {
        self.state.lock().written += len as u64;
    }

    pub fn written(&self) -> u64 {
        self.state.lock().written
    }

    /// Bytes received so far, including those still in a write buffer.
    fn received(&self) -> u64 {
        self.state.lock().reserved
    }

    pub fn release(&self) {
        self.state.lock().claimed = false;
    }

    /// Bytes of this segment no connection has received yet.
    pub fn unreserved(&self) -> u64 {
        let state = self.state.lock();
        state.end.saturating_sub(self.start + state.reserved)
    }

    fn snapshot(&self) -> SegmentSnapshot {
        let state = self.state.lock();
        SegmentSnapshot {
            start: self.start,
            end: state.end,
            downloaded: state.written,
        }
    }

    fn saved(&self) -> SavedSegment {
        let state = self.state.lock();
        SavedSegment {
            start: self.start,
            end: state.end,
            written: state.written,
        }
    }
}

#[derive(Debug)]
pub(crate) struct SegmentTable {
    segments: RwLock<Vec<Arc<Segment>>>,
    min_split: u64,
}

impl SegmentTable {
    /// Splits `total` bytes into up to `parts` equal segments, never smaller
    /// than `min_split`.
    pub fn new(total: u64, parts: usize, min_split: u64) -> Self {
        let parts = (parts as u64).min(total / min_split).max(1);
        let size = total.div_ceil(parts);
        let segments = (0..parts)
            .map(|i| (i * size, ((i + 1) * size).min(total)))
            .filter(|(start, end)| start < end)
            .map(|(start, end)| Arc::new(Segment::new(start, end, 0, false)))
            .collect();
        Self {
            segments: RwLock::new(segments),
            min_split,
        }
    }

    pub fn restore(saved: &[SavedSegment], min_split: u64) -> Self {
        let segments = saved
            .iter()
            .map(|s| Arc::new(Segment::new(s.start, s.end, s.written, false)))
            .collect();
        Self {
            segments: RwLock::new(segments),
            min_split,
        }
    }

    /// Hands out work to an idle connection: first any unfinished segment
    /// nobody owns, otherwise the second half of the largest range still in
    /// flight (IDM's in-half rule). `None` when nothing is worth taking.
    pub fn claim(&self) -> Option<Arc<Segment>> {
        let mut segments = self.segments.write();

        for segment in segments.iter() {
            let mut state = segment.state.lock();
            if !state.claimed && segment.start + state.written < state.end {
                state.claimed = true;
                return Some(Arc::clone(segment));
            }
        }

        let victim = segments.iter().max_by_key(|segment| segment.unreserved())?;
        let tail = {
            let mut state = victim.state.lock();
            let free = state.end.saturating_sub(victim.start + state.reserved);
            if free < 2 * self.min_split {
                return None;
            }
            let middle = victim.start + state.reserved + free / 2;
            let tail = Segment::new(middle, state.end, 0, true);
            state.end = middle;
            Arc::new(tail)
        };
        segments.push(Arc::clone(&tail));
        Some(tail)
    }

    pub fn written(&self) -> u64 {
        self.segments
            .read()
            .iter()
            .map(|segment| segment.written())
            .sum()
    }

    /// For progress display; checkpoints use [`Self::written`] instead.
    pub fn received(&self) -> u64 {
        self.segments
            .read()
            .iter()
            .map(|segment| segment.received())
            .sum()
    }

    pub fn snapshot(&self) -> Vec<SegmentSnapshot> {
        let mut snapshots: Vec<_> = self.segments.read().iter().map(|s| s.snapshot()).collect();
        snapshots.sort_by_key(|s| s.start);
        snapshots
    }

    pub fn saved(&self) -> Vec<SavedSegment> {
        let mut saved: Vec<_> = self.segments.read().iter().map(|s| s.saved()).collect();
        saved.sort_by_key(|s| s.start);
        saved
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    fn assert_covers(table: &SegmentTable, total: u64) {
        let saved = table.saved();
        assert_eq!(saved.first().unwrap().start, 0);
        assert_eq!(saved.last().unwrap().end, total);
        assert!(
            saved.windows(2).all(|pair| pair[0].end == pair[1].start),
            "{saved:?}"
        );
    }

    #[test]
    fn initial_split_respects_minimum_size() {
        assert_eq!(SegmentTable::new(10 * MIB, 8, MIB).saved().len(), 8);
        assert_eq!(SegmentTable::new(3 * MIB, 8, MIB).saved().len(), 3);
        assert_eq!(SegmentTable::new(100, 8, MIB).saved().len(), 1);
        assert_covers(&SegmentTable::new(10 * MIB + 7, 3, MIB), 10 * MIB + 7);
    }

    #[test]
    fn idle_connection_takes_half_of_the_largest_range() {
        let table = SegmentTable::new(8 * MIB, 1, MIB);
        let first = table.claim().unwrap();
        assert_eq!(first.begin(), Some((0, 8 * MIB)));
        assert_eq!(first.reserve(2 * MIB as usize), 2 * MIB as usize);

        let tail = table.claim().unwrap();
        assert_eq!(tail.begin(), Some((5 * MIB, 8 * MIB)));
        assert_covers(&table, 8 * MIB);

        // The first connection may now only fill up to the split point.
        assert_eq!(first.reserve(10 * MIB as usize), 3 * MIB as usize);
        assert!(first.fully_reserved());
    }

    #[test]
    fn small_remainders_are_not_split() {
        let table = SegmentTable::new(3 * MIB, 1, MIB);
        let only = table.claim().unwrap();
        only.reserve(2 * MIB as usize);
        assert!(table.claim().is_none());
    }

    #[test]
    fn released_unfinished_segments_are_handed_out_again() {
        let table = SegmentTable::new(4 * MIB, 2, MIB);
        let first = table.claim().unwrap();
        let _second = table.claim().unwrap();
        first.reserve(1024);
        first.commit(1024);
        first.release();
        let again = table.claim().unwrap();
        assert_eq!(again.begin(), Some((1024, 2 * MIB)));
    }

    #[test]
    fn restore_keeps_progress() {
        let saved = [
            SavedSegment {
                start: 0,
                end: 10,
                written: 10,
            },
            SavedSegment {
                start: 10,
                end: 20,
                written: 4,
            },
        ];
        let table = SegmentTable::restore(&saved, MIB);
        assert_eq!(table.written(), 14);
        assert_eq!(table.claim().unwrap().begin(), Some((14, 20)));
    }
}
