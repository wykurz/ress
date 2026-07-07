//! Background prefetch: watches the viewport anchor, infers the scroll
//! direction, and keeps the next blocks warm in the shared cache so scrolling
//! never waits on a cold read. Fills are best-effort tasks bounded by a
//! semaphore; a direction change or jump simply changes what gets requested
//! next (in-flight single-block reads complete and stay useful in the cache).
use crate::cache::BlockCache;
use crate::document::Anchor;
use std::sync::{Arc, Mutex};

/// Concurrent background fills: enough to hide latency without starving
/// interactive reads through the shared source (revisit with real NFS data).
const FILL_CONCURRENCY: usize = 4;

/// Best-effort background warming of the blocks ahead of the viewport.
pub struct Prefetcher {
    cache: Arc<BlockCache>,
    depth: usize,
    sem: Arc<tokio::sync::Semaphore>,
    state: Mutex<PrefetchState>,
}
struct PrefetchState {
    last_offset: Option<u64>,
    direction: i64,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Prefetcher {
    /// A prefetcher keeping `depth` blocks warm; `depth == 0` disables it.
    pub fn new(cache: Arc<BlockCache>, depth: usize) -> Self {
        Self {
            cache,
            depth,
            sem: Arc::new(tokio::sync::Semaphore::new(FILL_CONCURRENCY)),
            state: Mutex::new(PrefetchState {
                last_offset: None,
                direction: 1,
                tasks: Vec::new(),
            }),
        }
    }
    /// Notes the viewport anchor, infers direction, and spawns fills for the
    /// next `depth` blocks that way. Cheap and non-blocking; safe outside a
    /// runtime only if `depth == 0`.
    pub fn note_viewport(&self, top: Anchor) {
        if self.depth == 0 {
            return;
        }
        let bs = self.cache.block_size() as u64;
        let offset = top.offset();
        let block = offset / bs;
        let total_blocks = self.cache.size().div_ceil(bs);
        let mut st = self.state.lock().unwrap();
        // compare offsets, not block indices: most scrolls stay inside one
        // block, and a reversal must flip the direction before the anchor
        // crosses a block boundary or the first read after turning is cold.
        if let Some(last) = st.last_offset {
            if offset > last {
                st.direction = 1;
            } else if offset < last {
                st.direction = -1;
            }
        }
        st.last_offset = Some(offset);
        let direction = st.direction;
        st.tasks.retain(|t| !t.is_finished());
        // hard cap on queued fills: on a slow source, redraws can outpace
        // completions, and the semaphore bounds active reads but not queued
        // tasks. skipping new spawns at the cap keeps the backlog finite;
        // the cache's coalescing makes any skipped block cheap to fetch later.
        if st.tasks.len() > self.depth * 2 {
            return;
        }
        for step in 1..=self.depth as i64 {
            let idx = block as i64 + direction * step;
            if idx < 0 || idx as u64 >= total_blocks {
                break;
            }
            let cache = self.cache.clone();
            let sem = self.sem.clone();
            st.tasks.push(tokio::spawn(async move {
                let Ok(_permit) = sem.acquire().await else {
                    return;
                };
                // best effort: a failed fill is retried by whoever needs the block.
                let _ = cache.warm(idx as u64).await;
            }));
        }
    }
    /// Awaits all outstanding fills; used by tests for determinism.
    pub async fn settle(&self) {
        let tasks = {
            let mut st = self.state.lock().unwrap();
            std::mem::take(&mut st.tasks)
        };
        for t in tasks {
            let _ = t.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::MockSource;
    fn cache_of(len: usize, block_size: usize) -> (Arc<MockSource>, Arc<BlockCache>) {
        let src = Arc::new(MockSource::new(vec![b'x'; len]));
        let c = Arc::new(BlockCache::new(src.clone(), block_size, 1 << 20));
        (src, c)
    }
    #[tokio::test]
    async fn prefetches_ahead_in_scroll_direction() {
        let (src, c) = cache_of(1 << 12, 64);
        let p = Prefetcher::new(c.clone(), 4);
        p.note_viewport(Anchor::TOP);
        p.note_viewport(Anchor::TOP);
        p.settle().await;
        let warmed = src.read_count();
        // blocks 1..=4 are now resident: touching them adds no source reads.
        for idx in 1..=4u64 {
            let _ = c.block(idx).await.unwrap();
        }
        assert_eq!(
            src.read_count(),
            warmed,
            "prefetched blocks were not resident"
        );
    }
    #[tokio::test]
    async fn direction_flips_when_scrolling_up() {
        let (src, c) = cache_of(1 << 12, 64);
        let p = Prefetcher::new(c.clone(), 2);
        p.note_viewport(Anchor::TOP);
        // simulate being at block 32 then moving up to block 30.
        let at = |b: u64| Anchor::at(b * 64);
        p.note_viewport(at(32));
        p.note_viewport(at(30));
        p.settle().await;
        let warmed = src.read_count();
        for idx in [29u64, 28u64] {
            let _ = c.block(idx).await.unwrap();
        }
        assert_eq!(src.read_count(), warmed, "upward prefetch missed");
    }
    #[tokio::test]
    async fn direction_flips_within_a_single_block() {
        // reversals usually happen inside one block; direction must come from
        // anchor offsets, not block indices.
        let (src, c) = cache_of(1 << 12, 64);
        let p = Prefetcher::new(c.clone(), 2);
        p.note_viewport(Anchor::at(40 * 64));
        p.note_viewport(Anchor::at(40 * 64 + 32));
        p.note_viewport(Anchor::at(40 * 64 + 16));
        p.settle().await;
        let warmed = src.read_count();
        for idx in [39u64, 38u64] {
            let _ = c.block(idx).await.unwrap();
        }
        assert_eq!(
            src.read_count(),
            warmed,
            "upward prefetch missed after in-block reversal"
        );
    }
    #[tokio::test]
    async fn depth_zero_disables_prefetch() {
        let (src, c) = cache_of(1 << 12, 64);
        let p = Prefetcher::new(c.clone(), 0);
        p.note_viewport(Anchor::TOP);
        p.settle().await;
        assert_eq!(src.read_count(), 0);
    }
}
