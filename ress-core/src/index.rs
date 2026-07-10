//! The v1 analyzer: a sparse line index built from one sequential pass.
//! `LineIndex` is pure — bytes in, queryable checkpoints out — so it tests
//! without I/O; the scan scheduler owns feeding it and publishing progress.
/// How far the background scan has progressed, published through a watch
/// channel after every ingested block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Frontier {
    pub processed_up_to: u64,
    pub lines_so_far: u64,
    pub done: bool,
}
/// Byte offsets of every 1024th line start. ~8 MB of checkpoints indexes a
/// billion lines; the at-most-1023-line tail from a checkpoint to a target
/// line is walked with a budgeted `ForwardScan` at query time.
pub struct LineIndex {
    checkpoints: Vec<u64>,
    newlines: u64,
    pos: u64,
    last_byte: Option<u8>,
    done: bool,
    reached_eof: bool,
}
const CHECKPOINT_INTERVAL: u64 = 1024;
impl LineIndex {
    /// An empty index; line 0's start (offset 0) is the first checkpoint.
    pub fn new() -> LineIndex {
        LineIndex {
            checkpoints: vec![0],
            newlines: 0,
            pos: 0,
            last_byte: None,
            done: false,
            reached_eof: false,
        }
    }
    /// Ingests the next sequential chunk (chunks arrive in order from 0).
    pub fn ingest(&mut self, chunk: &[u8]) {
        for i in memchr::memchr_iter(b'\n', chunk) {
            self.newlines += 1;
            if self.newlines.is_multiple_of(CHECKPOINT_INTERVAL) {
                // the start of 0-based line `newlines`; a start that turns
                // out to sit at EOF (trailing newline) names a phantom line,
                // not a real one — the covered branch's `line0 == newlines()`
                // boundary can retrieve it directly (ERRATUM 4a#3), so the
                // walk boundary in document.rs guards `cp.0 < size` before
                // using any checkpoint this returns.
                self.checkpoints.push(self.pos + i as u64 + 1);
            }
        }
        if let Some(&b) = chunk.last() {
            self.last_byte = Some(b);
        }
        self.pos += chunk.len() as u64;
    }
    /// Marks ingestion complete; `total_lines` becomes known. `reached_eof`
    /// is true when the scan consumed the whole file, false when it stopped
    /// early after a read error — the two leave `total_lines` counting
    /// differently, since a partial scan's frontier byte is unread but still
    /// known to exist.
    pub fn finish(&mut self, reached_eof: bool) {
        self.done = true;
        self.reached_eof = reached_eof;
    }
    /// Progress snapshot for the watch channel.
    pub fn frontier(&self) -> Frontier {
        Frontier {
            processed_up_to: self.pos,
            lines_so_far: self.newlines,
            done: self.done,
        }
    }
    /// Newlines seen so far; the start of 0-based line `n` is known once
    /// `newlines() >= n`.
    pub fn newlines(&self) -> u64 {
        self.newlines
    }
    /// The line count once ingestion has finished; this is the best-known
    /// clamp goto_line wants. When the scan reached EOF, a final byte that
    /// is not a newline closes an unterminated last line, and a trailing
    /// newline names no further (phantom) line. When the scan instead
    /// stopped early after a read error, the byte at the frontier is known
    /// to exist in the file — so the line starting there is always real,
    /// and the count is the prefix's final known line start: `newlines + 1`
    /// regardless of the last byte ingested.
    pub fn total_lines(&self) -> Option<u64> {
        if !self.done {
            return None;
        }
        if !self.reached_eof {
            return Some(self.newlines + 1);
        }
        Some(match self.last_byte {
            None => 0,
            Some(b'\n') => self.newlines,
            Some(_) => self.newlines + 1,
        })
    }
    /// The greatest checkpoint at or below 0-based line `line0`: returns
    /// `(start_offset, line0_of_checkpoint)`. Callers guarantee
    /// `line0 <= newlines()`, so the checkpoint exists.
    pub fn nearest_checkpoint(&self, line0: u64) -> (u64, u64) {
        let j = (line0 / CHECKPOINT_INTERVAL) as usize;
        let j = j.min(self.checkpoints.len() - 1);
        (self.checkpoints[j], j as u64 * CHECKPOINT_INTERVAL)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn ingested(data: &[u8], piece: usize) -> LineIndex {
        // feed in awkward piece sizes to exercise chunk-boundary math.
        let mut ix = LineIndex::new();
        for c in data.chunks(piece.max(1)) {
            ix.ingest(c);
        }
        ix.finish(true);
        ix
    }
    #[test]
    fn counts_lines_with_and_without_trailing_newline() {
        assert_eq!(ingested(b"a\nb\nc\n", 2).total_lines(), Some(3));
        assert_eq!(ingested(b"a\nb\nc", 2).total_lines(), Some(3));
        assert_eq!(ingested(b"", 2).total_lines(), Some(0));
        assert_eq!(ingested(b"\n", 2).total_lines(), Some(1));
    }
    #[test]
    fn total_is_unknown_until_finished() {
        let mut ix = LineIndex::new();
        ix.ingest(b"a\nb\n");
        assert_eq!(ix.total_lines(), None);
        assert_eq!(ix.newlines(), 2);
        ix.finish(true);
        assert_eq!(ix.total_lines(), Some(2));
    }
    #[test]
    fn error_shortened_scan_counts_the_frontier_line() {
        let mut shortened = LineIndex::new();
        shortened.ingest(b"a\nb\n");
        shortened.finish(false);
        assert_eq!(shortened.total_lines(), Some(3));
        let mut finished = LineIndex::new();
        finished.ingest(b"a\nb\n");
        finished.finish(true);
        assert_eq!(finished.total_lines(), Some(2));
    }
    #[test]
    fn frontier_reports_progress_and_completion() {
        let mut ix = LineIndex::new();
        ix.ingest(b"a\nb");
        assert_eq!(
            ix.frontier(),
            Frontier {
                processed_up_to: 3,
                lines_so_far: 1,
                done: false
            }
        );
        ix.finish(true);
        assert!(ix.frontier().done);
    }
    #[test]
    fn checkpoints_land_every_1024th_line() {
        // 2050 two-byte lines; line0 1024 starts at 2048, line0 2048 at 4096.
        let data: Vec<u8> = std::iter::repeat_n(*b"x\n", 2050).flatten().collect();
        let ix = ingested(&data, 313);
        assert_eq!(ix.nearest_checkpoint(0), (0, 0));
        assert_eq!(ix.nearest_checkpoint(1023), (0, 0));
        assert_eq!(ix.nearest_checkpoint(1024), (2048, 1024));
        assert_eq!(ix.nearest_checkpoint(2049), (4096, 2048));
    }
    #[test]
    fn chunk_boundary_straddling_newline_is_counted_once() {
        // the newline is the last byte of one chunk; the next line's start
        // is the first byte of the next chunk — no double count, no skip.
        let mut ix = LineIndex::new();
        ix.ingest(b"ab\n");
        ix.ingest(b"cd");
        ix.finish(true);
        assert_eq!(ix.total_lines(), Some(2));
        assert_eq!(ix.frontier().processed_up_to, 5);
    }
}
#[cfg(test)]
mod props {
    use super::*;
    use proptest::prelude::*;
    fn naive_line_starts(data: &[u8]) -> Vec<u64> {
        let mut starts = vec![];
        if !data.is_empty() {
            starts.push(0);
        }
        for (i, &b) in data.iter().enumerate() {
            if b == b'\n' && i + 1 < data.len() {
                starts.push(i as u64 + 1);
            }
        }
        starts
    }
    proptest! {
        #[test]
        fn agrees_with_a_naive_scan(
            data in proptest::collection::vec(
                prop_oneof![3 => Just(b'\n'), 7 => any::<u8>()],
                0..4096,
            ),
            piece in 1usize..64,
        ) {
            let ix = {
                let mut ix = LineIndex::new();
                for c in data.chunks(piece) {
                    ix.ingest(c);
                }
                ix.finish(true);
                ix
            };
            let starts = naive_line_starts(&data);
            prop_assert_eq!(ix.total_lines(), Some(starts.len() as u64));
            for (line0, &start) in starts.iter().enumerate().step_by(97) {
                let (cp_off, cp_line) = ix.nearest_checkpoint(line0 as u64);
                prop_assert!(cp_line <= line0 as u64);
                prop_assert!(cp_off <= start, "checkpoint offset past the line it names");
                if cp_line == line0 as u64 {
                    prop_assert_eq!(cp_off, start);
                }
            }
        }
    }
}
