//! Budgeted newline scanning over the block cache. `ForwardScan` and
//! `BackwardScan` are the engine's only navigation read loops: every scan
//! states a byte budget per step, its window and origin are captured once at
//! construction, and the cursor lives inside the object — a pending
//! continuation is the same value as the interactive attempt. `fill_lines`
//! is the viewport's single-call read. Budgets bound read work at block
//! granularity: a result found in a block already in hand is returned even
//! past the nominal byte budget — total I/O stays within one block of the
//! budget, and an answer from paid-for bytes always beats a clamp.
use crate::cache::BlockCache;
/// One chunk's outcome of a resumable forward scan.
#[derive(Debug, PartialEq, Eq)]
pub enum FwdStep {
    /// The requested line start.
    Found(u64),
    /// EOF arrived first; the payload is the anchor to clamp to — the last
    /// real line start the scan saw, or the origin when it saw none. It is
    /// always a definitive answer, never a sentinel.
    Eof(u64),
    /// The chunk is consumed; call `step` again to continue.
    More,
}
/// A resumable scan for the `n`-th line start after a fixed origin. The
/// origin and target are captured at construction and the cursor never
/// leaves the object, so a pending continuation is the same value as the
/// interactive attempt — there is no cursor handoff to get wrong. Behavior
/// after a terminal outcome (`Found`/`Eof`) is unspecified; callers stop.
pub struct ForwardScan {
    origin: u64,
    pos: u64,
    needed: usize,
    chunk: usize,
    last_found: Option<u64>,
    scanned: u64,
}
impl ForwardScan {
    /// Scans for the `n`-th line start strictly after line-start `from`
    /// (`n == 0` resolves to `from` itself). Reads at most `chunk` bytes per
    /// step, clamped to one so every step makes progress.
    pub fn new(from: u64, n: usize, chunk: usize) -> ForwardScan {
        ForwardScan {
            origin: from,
            pos: from,
            needed: n,
            chunk: chunk.max(1),
            last_found: None,
            scanned: 0,
        }
    }
    /// Bytes consumed so far; spawn sites seed the progress channel with it.
    pub fn scanned(&self) -> u64 {
        self.scanned
    }
    /// Consumes up to one chunk of bytes looking for the target line start,
    /// never reporting a start at or past EOF (an EOF-adjacent trailing
    /// newline terminates the scan instead).
    pub async fn step(&mut self, cache: &crate::cache::BlockCache) -> anyhow::Result<FwdStep> {
        if self.needed == 0 {
            return Ok(FwdStep::Found(self.pos));
        }
        let size = cache.size();
        let bs = cache.block_size() as u64;
        let mut spent = 0usize;
        while self.pos < size && spent < self.chunk {
            let block = cache.block(self.pos / bs).await?;
            let lo = (self.pos % bs) as usize;
            let slice = &block[lo.min(block.len())..];
            if slice.is_empty() {
                break;
            }
            for (i, &b) in slice.iter().enumerate() {
                if b == b'\n' {
                    let ls = self.pos + i as u64 + 1;
                    // terminal returns count their partial block, so
                    // `scanned` stays literally "bytes consumed" (ERRATUM 3c#2).
                    if ls >= size {
                        self.scanned += i as u64 + 1;
                        return Ok(FwdStep::Eof(self.clamp()));
                    }
                    self.last_found = Some(ls);
                    self.needed -= 1;
                    if self.needed == 0 {
                        self.scanned += i as u64 + 1;
                        return Ok(FwdStep::Found(ls));
                    }
                }
            }
            self.pos += slice.len() as u64;
            self.scanned += slice.len() as u64;
            spent += slice.len();
        }
        if self.pos >= size {
            Ok(FwdStep::Eof(self.clamp()))
        } else {
            Ok(FwdStep::More)
        }
    }
    /// Drives the scan to its terminal anchor in chunk-sized steps, publishing
    /// progress after each and yielding so aborts have a guaranteed point to
    /// take effect on a hot cache.
    pub async fn complete(
        mut self,
        cache: &crate::cache::BlockCache,
        tx: tokio::sync::watch::Sender<crate::resolve::Progress>,
        span: u64,
    ) -> anyhow::Result<u64> {
        loop {
            match self.step(cache).await? {
                FwdStep::Found(s) | FwdStep::Eof(s) => return Ok(s),
                FwdStep::More => {
                    let _ = tx.send(crate::resolve::Progress {
                        scanned: self.scanned,
                        span,
                    });
                    tokio::task::yield_now().await;
                }
            }
        }
    }
    fn clamp(&self) -> u64 {
        self.last_found.unwrap_or(self.origin)
    }
}
/// One chunk's outcome of a resumable backward scan.
#[derive(Debug, PartialEq, Eq)]
pub enum BwdStep {
    /// The byte just after the target newline — a line start.
    Found(u64),
    /// Fewer than `n` newlines exist in the window; the line start is 0.
    Top,
    /// The chunk is consumed; call `step` again to continue.
    More,
}
/// A resumable scan for the `n`-th newline above a fixed position. The search
/// window is captured once, at construction: the byte at `pos - 1` is
/// excluded, because for a line-start `pos` it is the newline that made it a
/// line start, and for `pos == size` it is a trailing newline that must not
/// count as a line above. Resumption is just another `step` — the exclusion
/// is never re-derived, so the off-by-one class from bare resume cursors is
/// unrepresentable. Behavior after a terminal outcome is unspecified.
pub struct BackwardScan {
    hi: u64,
    needed: usize,
    chunk: usize,
    scanned: u64,
}
impl BackwardScan {
    /// Scans `[0, pos - 1)` downward for the `n`-th newline, reading at most
    /// `chunk` bytes per step (clamped to one so every step makes progress).
    /// Callers guard `n >= 1`; `n == 0` resolves as `Top` without reading.
    pub fn new(pos: u64, n: usize, chunk: usize) -> BackwardScan {
        BackwardScan {
            hi: pos.saturating_sub(1),
            needed: n,
            chunk: chunk.max(1),
            scanned: 0,
        }
    }
    /// Bytes consumed so far; spawn sites seed the progress channel with it.
    pub fn scanned(&self) -> u64 {
        self.scanned
    }
    /// Bytes not yet searched. At construction this is the full window, which
    /// makes an honest progress span for a scan started as its own phase.
    pub fn remaining_bytes(&self) -> u64 {
        self.hi
    }
    /// Consumes up to one chunk of bytes searching downward.
    pub async fn step(&mut self, cache: &crate::cache::BlockCache) -> anyhow::Result<BwdStep> {
        if self.needed == 0 {
            return Ok(BwdStep::Top);
        }
        // a window past EOF can never make progress (past-EOF blocks are
        // empty, and the empty-slice break skips the `hi = lo` advance), so
        // an out-of-range position must degrade to scanning the real bytes
        // rather than stepping forever (ERRATUM 3c#3).
        self.hi = self.hi.min(cache.size());
        let bs = cache.block_size() as u64;
        let mut spent = 0usize;
        while self.hi > 0 && spent < self.chunk {
            let idx = (self.hi - 1) / bs;
            let lo = idx * bs;
            let block = cache.block(idx).await?;
            let take = ((self.hi - lo) as usize).min(block.len());
            let slice = &block[..take];
            if slice.is_empty() {
                break;
            }
            for (i, &b) in slice.iter().enumerate().rev() {
                if b == b'\n' {
                    self.needed -= 1;
                    if self.needed == 0 {
                        // count the partially examined block, so `scanned`
                        // stays literally "bytes consumed" (ERRATUM 3c#2).
                        self.scanned += (slice.len() - i) as u64;
                        return Ok(BwdStep::Found(lo + i as u64 + 1));
                    }
                }
            }
            spent += slice.len();
            self.scanned += slice.len() as u64;
            self.hi = lo;
        }
        if self.hi == 0 {
            Ok(BwdStep::Top)
        } else {
            Ok(BwdStep::More)
        }
    }
    /// Drives the scan to its terminal anchor (`Top` is the definitive anchor
    /// 0) in chunk-sized steps, publishing progress after each and yielding so
    /// aborts have a guaranteed point to take effect on a hot cache.
    pub async fn complete(
        mut self,
        cache: &crate::cache::BlockCache,
        tx: tokio::sync::watch::Sender<crate::resolve::Progress>,
        span: u64,
    ) -> anyhow::Result<u64> {
        loop {
            match self.step(cache).await? {
                BwdStep::Found(s) => return Ok(s),
                BwdStep::Top => return Ok(0),
                BwdStep::More => {
                    let _ = tx.send(crate::resolve::Progress {
                        scanned: self.scanned,
                        span,
                    });
                    tokio::task::yield_now().await;
                }
            }
        }
    }
}
/// Collects bytes from `from` up to and including the `rows`-th newline, EOF,
/// or `budget`; the bool reports whether the scan stopped at EOF or budget.
pub async fn fill_lines(
    cache: &BlockCache,
    from: u64,
    rows: usize,
    budget: usize,
) -> anyhow::Result<(Vec<u8>, bool)> {
    let size = cache.size();
    let bs = cache.block_size() as u64;
    let mut buf: Vec<u8> = Vec::new();
    let mut pos = from;
    let mut newlines = 0usize;
    while newlines < rows && pos < size && buf.len() < budget {
        let block = cache.block(pos / bs).await?;
        let lo = (pos % bs) as usize;
        let slice = &block[lo.min(block.len())..];
        if slice.is_empty() {
            break;
        }
        let mut take = slice.len();
        for (i, &b) in slice.iter().enumerate() {
            if b == b'\n' {
                newlines += 1;
                if newlines == rows {
                    take = i + 1;
                    break;
                }
            }
        }
        buf.extend_from_slice(&slice[..take]);
        pos += take as u64;
    }
    let stopped = pos >= size || buf.len() >= budget;
    Ok((buf, stopped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::MockSource;
    use std::sync::Arc;
    fn cache(data: &'static [u8], block_size: usize) -> BlockCache {
        BlockCache::new(
            Arc::new(MockSource::new(bytes::Bytes::from_static(data))),
            block_size,
            1 << 20,
        )
    }
    #[tokio::test]
    async fn fill_collects_rows_worth_of_lines() {
        let c = cache(b"a\nb\nc\nd\n", 4);
        let (buf, stopped) = fill_lines(&c, 2, 2, 1 << 10).await.unwrap();
        assert_eq!(&buf[..], b"b\nc\n");
        assert!(!stopped);
    }
    #[tokio::test]
    async fn fill_stops_at_eof_and_reports_it() {
        let c = cache(b"a\nbc", 4);
        let (buf, stopped) = fill_lines(&c, 2, 5, 1 << 10).await.unwrap();
        assert_eq!(&buf[..], b"bc");
        assert!(stopped);
    }
    #[tokio::test]
    async fn fill_respects_the_budget() {
        let data: &'static [u8] = Box::leak(vec![b'x'; 4096].into_boxed_slice());
        let c = cache(data, 16);
        let (buf, stopped) = fill_lines(&c, 0, 2, 64).await.unwrap();
        assert!(
            buf.len() >= 64 && buf.len() <= 64 + 16,
            "budget overshoot: {}",
            buf.len()
        );
        assert!(stopped);
    }
    #[tokio::test]
    async fn forward_scan_finds_nth_line_start() {
        let c = cache(b"a\nb\nc\nd\n", 4);
        let mut s = ForwardScan::new(0, 2, 1 << 10);
        match s.step(&c).await.unwrap() {
            FwdStep::Found(start) => assert_eq!(start, 4),
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn forward_scan_eof_clamps_to_last_real_line_start() {
        // the trailing newline before EOF is not a line start; the clamp is
        // the last real one, delivered as a definitive anchor (no sentinel).
        let c = cache(b"a\nb\nc\n", 4);
        let mut s = ForwardScan::new(0, 99, 1 << 10);
        match s.step(&c).await.unwrap() {
            FwdStep::Eof(clamp) => assert_eq!(clamp, 4),
            other => panic!("expected Eof, got {other:?}"),
        }
        // pins the fast-Eof branch's accounting, the twin of the Found
        // branch asserted in forward_scan_scanned_counts_the_terminal_block.
        assert_eq!(s.scanned(), 6, "all six bytes were examined");
    }
    #[tokio::test]
    async fn forward_scan_scanned_counts_the_terminal_block() {
        // a terminal step examines part of a block before returning; those
        // bytes are consumed and must be counted (ERRATUM 3c#2).
        let data: &'static [u8] = Box::leak({
            let mut v = vec![b'x'; 64];
            v.extend_from_slice(b"\ny");
            v.into_boxed_slice()
        });
        let c = cache(data, 16);
        let mut s = ForwardScan::new(0, 1, 1 << 10);
        match s.step(&c).await.unwrap() {
            FwdStep::Found(start) => assert_eq!(start, 65),
            other => panic!("expected Found, got {other:?}"),
        }
        assert_eq!(s.scanned(), 65, "bytes 0..=64 were all examined");
    }
    #[tokio::test]
    async fn forward_scan_eof_clamps_to_origin_when_no_newline_seen() {
        let c = cache(b"abcdef", 4);
        let mut s = ForwardScan::new(0, 1, 1 << 10);
        match s.step(&c).await.unwrap() {
            FwdStep::Eof(clamp) => assert_eq!(clamp, 0),
            other => panic!("expected Eof, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn forward_scan_resumes_across_chunks_without_handoff() {
        // 4096 x's then a newline then y: one chunk cannot reach it; stepping
        // the same object to the end must, with no cursor re-derivation.
        let data: &'static [u8] = Box::leak({
            let mut v = vec![b'x'; 4096];
            v.extend_from_slice(b"\ny");
            v.into_boxed_slice()
        });
        let c = cache(data, 16);
        let mut s = ForwardScan::new(0, 1, 64);
        let mut steps = 0usize;
        loop {
            match s.step(&c).await.unwrap() {
                FwdStep::Found(start) => {
                    assert_eq!(start, 4097);
                    assert!(steps >= 2, "must have taken multiple chunks");
                    break;
                }
                FwdStep::Eof(clamp) => panic!("unexpected Eof({clamp})"),
                FwdStep::More => {
                    steps += 1;
                    assert!(
                        s.scanned() >= 64 * steps as u64 - 16,
                        "scanned() tracks chunks"
                    );
                }
            }
        }
    }
    #[tokio::test]
    async fn forward_scan_zero_n_is_the_origin() {
        let c = cache(b"a\nb\n", 4);
        let mut s = ForwardScan::new(2, 0, 1 << 10);
        match s.step(&c).await.unwrap() {
            FwdStep::Found(start) => assert_eq!(start, 2),
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn forward_scan_zero_chunk_still_makes_progress() {
        // the clamp to one byte lives in the constructor now, not at call sites.
        let c = cache(b"a\nb\n", 4);
        let mut s = ForwardScan::new(0, 1, 0);
        loop {
            match s.step(&c).await.unwrap() {
                FwdStep::Found(start) => {
                    assert_eq!(start, 2);
                    break;
                }
                FwdStep::More => {}
                other => panic!("unexpected {other:?}"),
            }
        }
    }
    #[tokio::test]
    async fn forward_scan_complete_resolves_like_stepping() {
        let data: &'static [u8] = Box::leak({
            let mut v = vec![b'x'; 1024];
            v.extend_from_slice(b"\ny");
            v.into_boxed_slice()
        });
        let c = cache(data, 16);
        let s = ForwardScan::new(0, 1, 64);
        let (tx, rx) = tokio::sync::watch::channel(crate::resolve::Progress {
            scanned: 0,
            span: 1026,
        });
        let got = s.complete(&c, tx, 1026).await.unwrap();
        assert_eq!(got, 1025);
        assert!(rx.borrow().scanned > 0, "progress published per chunk");
    }
    #[tokio::test]
    async fn backward_scan_finds_nth_newline() {
        let c = cache(b"aaaa\nbbbb\ncccc\n", 4);
        let mut s = BackwardScan::new(10, 1, 1 << 10);
        match s.step(&c).await.unwrap() {
            BwdStep::Found(start) => assert_eq!(start, 5),
            other => panic!("expected Found, got {other:?}"),
        }
        let mut s = BackwardScan::new(14, 2, 1 << 10);
        match s.step(&c).await.unwrap() {
            BwdStep::Found(start) => assert_eq!(start, 5),
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn backward_scan_top_when_fewer_newlines_exist() {
        let c = cache(b"a\nb\n", 4);
        let mut s = BackwardScan::new(2, 99, 1 << 10);
        match s.step(&c).await.unwrap() {
            BwdStep::Top => {}
            other => panic!("expected Top, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn backward_scan_resume_covers_the_boundary_byte() {
        // the 3b P1 bug class: 4097 bytes through 16-byte blocks with a
        // 64-byte chunk makes the first step end exactly at hi = 4032; the
        // newline at byte 4031 is the boundary byte an entry-semantics
        // re-derivation (`nth_newline_before(4032)` searching [0, 4031))
        // would skip. the exclusion happens once, at construction, so the
        // resumed step must find it.
        let data: &'static [u8] = Box::leak({
            let mut v = vec![b'x'; 4097];
            v[4031] = b'\n';
            v.into_boxed_slice()
        });
        let c = cache(data, 16);
        let mut s = BackwardScan::new(4097, 1, 64);
        match s.step(&c).await.unwrap() {
            BwdStep::More => {}
            other => panic!("expected More on the first chunk, got {other:?}"),
        }
        match s.step(&c).await.unwrap() {
            BwdStep::Found(start) => assert_eq!(start, 4032, "the boundary-byte line start"),
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn backward_scan_excludes_the_entry_byte() {
        // from line-start 5, the newline at byte 4 (which made 5 a line
        // start) must not count; the next one up is at byte 1.
        let c = cache(b"a\nbb\ncc\n", 4);
        let mut s = BackwardScan::new(5, 1, 1 << 10);
        match s.step(&c).await.unwrap() {
            BwdStep::Found(start) => assert_eq!(start, 2),
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn backward_scan_from_zero_is_top() {
        let c = cache(b"a\nb\n", 4);
        let mut s = BackwardScan::new(0, 1, 1 << 10);
        match s.step(&c).await.unwrap() {
            BwdStep::Top => {}
            other => panic!("expected Top, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn backward_scan_scanned_counts_the_terminal_block() {
        // same contract as the forward twin: the partially examined terminal
        // block counts toward bytes consumed (ERRATUM 3c#2).
        let data: &'static [u8] = Box::leak({
            let mut v = b"a\n".to_vec();
            v.extend_from_slice(&[b'x'; 62]);
            v.into_boxed_slice()
        });
        let c = cache(data, 16);
        let mut s = BackwardScan::new(64, 1, 1 << 10);
        match s.step(&c).await.unwrap() {
            BwdStep::Found(start) => assert_eq!(start, 2),
            other => panic!("expected Found, got {other:?}"),
        }
        assert_eq!(s.scanned(), 62, "bytes 1..=62 were all examined");
    }
    #[tokio::test]
    async fn backward_scan_complete_resolves_top_as_zero() {
        let data: &'static [u8] = Box::leak(vec![b'x'; 1024].into_boxed_slice());
        let c = cache(data, 16);
        let s = BackwardScan::new(1024, 1, 64);
        let (tx, rx) = tokio::sync::watch::channel(crate::resolve::Progress {
            scanned: 0,
            span: 1024,
        });
        let got = s.complete(&c, tx, 1024).await.unwrap();
        assert_eq!(got, 0);
        assert!(rx.borrow().scanned > 0, "progress published per chunk");
    }
    #[test]
    fn backward_scan_reports_its_window() {
        // construction needs no cache: the window is pure arithmetic.
        let s = BackwardScan::new(5, 1, 1 << 10);
        assert_eq!(s.remaining_bytes(), 4);
        assert_eq!(BackwardScan::new(0, 1, 1).remaining_bytes(), 0);
    }
    #[tokio::test]
    async fn backward_scan_out_of_range_position_terminates() {
        // hi far past EOF lands on an empty block; without the clamp the
        // empty-slice break never advances hi and step() returns More
        // forever (ERRATUM 3c#3). a single More here IS the hang.
        let c = cache(b"xxxxxxxxxx", 16);
        let mut s = BackwardScan::new(64, 1, 1 << 10);
        match s.step(&c).await.unwrap() {
            BwdStep::Top => {}
            other => panic!("expected Top, got {other:?} (no progress on an out-of-range window)"),
        }
    }
}

#[cfg(test)]
mod props {
    use super::*;
    use crate::source::MockSource;
    use proptest::prelude::*;
    use proptest::test_runner::TestCaseError;
    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
    }
    proptest! {
        #[test]
        fn forward_stepped_chunks_agree_with_one_giant_step(
            data in proptest::collection::vec(
                prop_oneof![2 => Just(b'\n'), 8 => any::<u8>()],
                0..128,
            ),
            block_size in 1usize..32,
            chunk in 1usize..24,
            from in 0u64..128,
            n in 0usize..6,
        ) {
            let from = from.min(data.len() as u64);
            let c = crate::cache::BlockCache::new(
                std::sync::Arc::new(MockSource::new(data.clone())),
                block_size,
                1 << 20,
            );
            rt().block_on(async {
                let mut big = ForwardScan::new(from, n, usize::MAX);
                let oracle = big.step(&c).await.unwrap();
                let mut small = ForwardScan::new(from, n, chunk);
                let got = loop {
                    match small.step(&c).await.unwrap() {
                        FwdStep::More => {}
                        terminal => break terminal,
                    }
                };
                prop_assert_eq!(&got, &oracle, "chunked scan diverged from unbudgeted scan");
                Ok::<(), TestCaseError>(())
            })?;
        }
        #[test]
        fn backward_stepped_chunks_agree_with_one_giant_step(
            data in proptest::collection::vec(
                prop_oneof![2 => Just(b'\n'), 8 => any::<u8>()],
                0..128,
            ),
            block_size in 1usize..32,
            chunk in 1usize..24,
            pos in 0u64..128,
            n in 1usize..6,
        ) {
            let pos = pos.min(data.len() as u64);
            let c = crate::cache::BlockCache::new(
                std::sync::Arc::new(MockSource::new(data.clone())),
                block_size,
                1 << 20,
            );
            rt().block_on(async {
                let mut big = BackwardScan::new(pos, n, usize::MAX);
                let oracle = big.step(&c).await.unwrap();
                let mut small = BackwardScan::new(pos, n, chunk);
                let got = loop {
                    match small.step(&c).await.unwrap() {
                        BwdStep::More => {}
                        terminal => break terminal,
                    }
                };
                prop_assert_eq!(&got, &oracle, "chunked scan diverged from unbudgeted scan");
                Ok::<(), TestCaseError>(())
            })?;
        }
    }
}
