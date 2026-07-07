//! Budgeted newline scanning over the block cache. These are the engine's only
//! read loops: every caller states a byte budget, so no navigation or render
//! path can read without limit. Budgets bound read work at block granularity:
//! a result found in a block that is already in hand is returned even when it
//! lies past the nominal byte budget — total I/O stays within one block of the
//! budget, and an answer from paid-for bytes always beats a clamp.
use crate::cache::BlockCache;
/// Outcome of a forward scan for a line start.
#[derive(Debug, PartialEq, Eq)]
pub enum Forward {
    /// The requested line start.
    Found { start: u64 },
    /// EOF first; `last_start` is the furthest line start seen (>= `from`).
    Eof { last_start: u64 },
    /// Budget first; `last_start` is the furthest line start seen (>= `from`).
    Budget { last_start: u64 },
}
/// Outcome of a backward scan for a newline.
#[derive(Debug, PartialEq, Eq)]
pub enum Backward {
    /// The byte just after the n-th newline strictly before `pos`.
    Found { start: u64 },
    /// Fewer than `n` newlines exist above; the line start is offset 0.
    Top,
    /// Budget exhausted before finding the n-th newline.
    Budget,
}
/// Returns the start of the `n`-th line after line-start `from`, never at or
/// past EOF (an EOF-adjacent trailing newline reports the previous start).
pub async fn nth_line_start_after(
    cache: &BlockCache,
    from: u64,
    n: usize,
    budget: usize,
) -> anyhow::Result<Forward> {
    let size = cache.size();
    let bs = cache.block_size() as u64;
    let mut pos = from;
    let mut last_start = from;
    let mut found = 0usize;
    let mut spent = 0usize;
    while found < n && pos < size && spent < budget {
        let block = cache.block(pos / bs).await?;
        let lo = (pos % bs) as usize;
        let slice = &block[lo.min(block.len())..];
        if slice.is_empty() {
            break;
        }
        for (i, &b) in slice.iter().enumerate() {
            if b == b'\n' {
                let ls = pos + i as u64 + 1;
                if ls >= size {
                    return Ok(Forward::Eof { last_start });
                }
                last_start = ls;
                found += 1;
                if found == n {
                    return Ok(Forward::Found { start: ls });
                }
            }
        }
        pos += slice.len() as u64;
        spent += slice.len();
    }
    if found == n {
        // reachable only for n == 0 (the loop returns on the n-th find): the
        // 0th line start after `from` is `from` itself. do not remove.
        Ok(Forward::Found { start: last_start })
    } else if pos >= size {
        Ok(Forward::Eof { last_start })
    } else {
        Ok(Forward::Budget { last_start })
    }
}
/// Returns the byte after the `n`-th newline strictly before `pos` (searching
/// `[0, pos-1)`), `Top` when fewer exist, or `Budget` when the window runs out.
pub async fn nth_newline_before(
    cache: &BlockCache,
    pos: u64,
    n: usize,
    budget: usize,
) -> anyhow::Result<Backward> {
    if pos <= 1 {
        return Ok(Backward::Top);
    }
    let bs = cache.block_size() as u64;
    let mut hi = pos - 1;
    let mut found = 0usize;
    let mut spent = 0usize;
    while hi > 0 && spent < budget {
        let idx = (hi - 1) / bs;
        let lo = idx * bs;
        let block = cache.block(idx).await?;
        let take = ((hi - lo) as usize).min(block.len());
        let slice = &block[..take];
        for (i, &b) in slice.iter().enumerate().rev() {
            if b == b'\n' {
                found += 1;
                if found == n {
                    return Ok(Backward::Found {
                        start: lo + i as u64 + 1,
                    });
                }
            }
        }
        spent += slice.len();
        hi = lo;
    }
    if hi == 0 {
        Ok(Backward::Top)
    } else {
        Ok(Backward::Budget)
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
    async fn forward_finds_nth_line_start() {
        let c = cache(b"a\nb\nc\nd\n", 4);
        match nth_line_start_after(&c, 0, 2, 1 << 10).await.unwrap() {
            Forward::Found { start } => assert_eq!(start, 4),
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn forward_eof_carries_last_line_start() {
        let c = cache(b"a\nb\nc", 4);
        match nth_line_start_after(&c, 0, 99, 1 << 10).await.unwrap() {
            Forward::Eof { last_start } => assert_eq!(last_start, 4),
            other => panic!("expected Eof, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn forward_budget_stops_the_scan() {
        // one giant line, tiny budget: no line start found, budget reported.
        let data: &'static [u8] = Box::leak(vec![b'x'; 4096].into_boxed_slice());
        let c = cache(data, 16);
        match nth_line_start_after(&c, 0, 1, 64).await.unwrap() {
            Forward::Budget { last_start } => assert_eq!(last_start, 0),
            other => panic!("expected Budget, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn backward_finds_nth_newline() {
        let c = cache(b"aaaa\nbbbb\ncccc\n", 4);
        match nth_newline_before(&c, 10, 1, 1 << 10).await.unwrap() {
            Backward::Found { start } => assert_eq!(start, 5),
            other => panic!("expected Found, got {other:?}"),
        }
        match nth_newline_before(&c, 14, 2, 1 << 10).await.unwrap() {
            Backward::Found { start } => assert_eq!(start, 5),
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn backward_top_when_fewer_lines_exist() {
        let c = cache(b"a\nb\n", 4);
        match nth_newline_before(&c, 2, 99, 1 << 10).await.unwrap() {
            Backward::Top => {}
            other => panic!("expected Top, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn backward_budget_stops_the_scan() {
        let data: &'static [u8] = Box::leak(vec![b'x'; 4096].into_boxed_slice());
        let c = cache(data, 16);
        match nth_newline_before(&c, 4096, 1, 64).await.unwrap() {
            Backward::Budget => {}
            other => panic!("expected Budget, got {other:?}"),
        }
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
}
