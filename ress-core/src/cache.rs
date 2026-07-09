//! The shared block cache every consumer reads file bytes through. Blocks are
//! fixed-size and keyed by index; concurrent misses for one block coalesce into
//! a single physical read; eviction is scan-resistant (SLRU): first-touch blocks
//! live in a probationary segment and only re-referenced blocks are promoted, so
//! a one-pass scan can never evict the interactive working set.
use crate::source::BlockSource;
use bytes::Bytes;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

/// A cloneable read error that keeps the underlying chain traversable, so
/// coalesced waiters preserve `{:#}` diagnostics exactly like the fetcher.
#[derive(Debug, Clone)]
struct SharedError(Arc<anyhow::Error>);
impl std::fmt::Display for SharedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self.0.as_ref(), f)
    }
}
impl std::error::Error for SharedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        // skip the chain's head: Display above already renders it.
        self.0.chain().nth(1)
    }
}

/// A block-aligned, scan-resistant cache over a `BlockSource`.
pub struct BlockCache {
    source: Arc<dyn BlockSource>,
    block_size: usize,
    state: Mutex<State>,
}
struct State {
    // first-touch blocks; a full-file scan lives and dies here.
    probation: lru::LruCache<u64, Bytes>,
    // re-referenced blocks; the interactive working set.
    protected: lru::LruCache<u64, Bytes>,
    // one watch channel per block being fetched; waiters clone the receiver.
    in_flight: HashMap<u64, tokio::sync::watch::Receiver<Option<Result<Bytes, SharedError>>>>,
}
/// Removes a fetcher's `in_flight` registration if the fetching future is
/// dropped mid-read — e.g. an aborted scan — instead of completing
/// normally. A waiter on the same block self-heals on the dropped watch
/// channel, but a block that is never requested again would otherwise
/// leak its entry forever. The normal path disarms the guard before it
/// removes the entry itself under the publish lock, so `drop` is then a
/// no-op.
struct InFlightGuard<'a> {
    cache: &'a BlockCache,
    idx: u64,
    rx: tokio::sync::watch::Receiver<Option<Result<Bytes, SharedError>>>,
    armed: bool,
}
impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let mut st = self.cache.state.lock().unwrap();
            if st
                .in_flight
                .get(&self.idx)
                .is_some_and(|cur| cur.same_channel(&self.rx))
            {
                st.in_flight.remove(&self.idx);
            }
        }
    }
}
impl BlockCache {
    /// Creates a cache holding at most `capacity_bytes` of block data (rounded
    /// down to whole blocks, minimum two), split 1:3 probationary:protected; a
    /// zero block size is treated as one byte.
    pub fn new(source: Arc<dyn BlockSource>, block_size: usize, capacity_bytes: usize) -> Self {
        // cap the block count: `lru` preallocates its map eagerly, so a tiny
        // block size against a large byte capacity must not allocate millions
        // of entries up front (65,536 blocks ≈ 64 GiB at the default block size,
        // far beyond any real configuration).
        let blocks = (capacity_bytes / block_size.max(1)).clamp(2, 1 << 16);
        let prob = NonZeroUsize::new((blocks / 4).max(1)).unwrap();
        let prot = NonZeroUsize::new((blocks - prob.get()).max(1)).unwrap();
        Self {
            source,
            block_size: block_size.max(1),
            state: Mutex::new(State {
                probation: lru::LruCache::new(prob),
                protected: lru::LruCache::new(prot),
                in_flight: HashMap::new(),
            }),
        }
    }
    /// Total size of the underlying source in bytes.
    pub fn size(&self) -> u64 {
        self.source.size()
    }
    /// The fixed block size in bytes.
    pub fn block_size(&self) -> usize {
        self.block_size
    }
    /// Returns the bytes of block `idx` (short at EOF, empty past EOF), reading
    /// through the source on a miss. Concurrent misses for the same block share
    /// one physical read; if a fetcher is cancelled mid-read, a waiter retries.
    /// A probationary hit counts as a re-reference and promotes the block.
    pub async fn block(&self, idx: u64) -> anyhow::Result<Bytes> {
        self.get(idx, true).await
    }
    /// Like `block`, but a probationary hit does NOT promote: prefetch warming
    /// must not count as a re-reference, or repeated fills would push
    /// never-viewed blocks into the protected segment and evict the
    /// interactive working set.
    pub(crate) async fn warm(&self, idx: u64) -> anyhow::Result<Bytes> {
        self.get(idx, false).await
    }
    async fn get(&self, idx: u64, promote: bool) -> anyhow::Result<Bytes> {
        loop {
            let (fetch_tx, mut wait_rx) = {
                let mut st = self.state.lock().unwrap();
                if let Some(b) = st.protected.get(&idx) {
                    return Ok(b.clone());
                }
                if promote {
                    if let Some(b) = Self::promote_entry(&mut st, idx) {
                        return Ok(b);
                    }
                } else if let Some(b) = st.probation.get(&idx) {
                    return Ok(b.clone());
                }
                if let Some(rx) = st.in_flight.get(&idx) {
                    (None, rx.clone())
                } else {
                    let (tx, rx) = tokio::sync::watch::channel(None);
                    st.in_flight.insert(idx, rx.clone());
                    (Some(tx), rx)
                }
            };
            if let Some(tx) = fetch_tx {
                // a guard against leaking this registration if the future is
                // aborted at the read below; disarmed once the read returns.
                let mut guard = InFlightGuard {
                    cache: self,
                    idx,
                    rx: wait_rx.clone(),
                    armed: true,
                };
                let offset = idx * self.block_size as u64;
                let res = self.source.read_block(offset, self.block_size).await;
                guard.armed = false;
                let mut st = self.state.lock().unwrap();
                st.in_flight.remove(&idx);
                return match res {
                    Ok(bytes) => {
                        st.probation.put(idx, bytes.clone());
                        let _ = tx.send(Some(Ok(bytes.clone())));
                        Ok(bytes)
                    }
                    Err(e) => {
                        // errors are not cached; the fetcher's caller and every
                        // coalesced waiter all receive the full chain through a
                        // shared, cloneable wrapper.
                        let shared = SharedError(Arc::new(e.context(format!("read block {idx}"))));
                        let _ = tx.send(Some(Err(shared.clone())));
                        Err(anyhow::Error::new(shared))
                    }
                };
            }
            loop {
                let published = wait_rx.borrow().clone();
                if let Some(res) = published {
                    return match res {
                        Ok(bytes) => {
                            if promote {
                                // a promoting caller that coalesced with an
                                // in-flight (often prefetch) read must leave
                                // the same state as arriving after the fill:
                                // the interactive touch counts toward
                                // promotion.
                                let mut st = self.state.lock().unwrap();
                                if Self::promote_entry(&mut st, idx).is_none()
                                    && st.protected.get(&idx).is_none()
                                {
                                    // evicted between publish and wake-up: this
                                    // is still fill + display — the same two
                                    // touches that promote in every other
                                    // interleaving — so the outcome must not
                                    // depend on churn timing.
                                    Self::insert_protected(&mut st, idx, bytes.clone());
                                }
                            }
                            Ok(bytes)
                        }
                        Err(e) => Err(anyhow::Error::new(e)),
                    };
                }
                if wait_rx.changed().await.is_err() {
                    // the fetcher was cancelled mid-read: clean up its stale entry
                    // (guarded, so a fresh fetcher's entry is never clobbered) and
                    // retry from the top as a fetcher candidate.
                    let mut st = self.state.lock().unwrap();
                    if st
                        .in_flight
                        .get(&idx)
                        .is_some_and(|cur| cur.same_channel(&wait_rx))
                    {
                        st.in_flight.remove(&idx);
                    }
                    break;
                }
            }
        }
    }
    /// Inserts bytes into the protected segment, demoting any evicted victim
    /// back to probation.
    fn insert_protected(st: &mut State, idx: u64, bytes: Bytes) {
        if let Some((k, v)) = st.protected.push(idx, bytes)
            && k != idx
        {
            st.probation.put(k, v);
        }
    }
    /// Moves a probationary entry into the protected segment and returns its
    /// bytes; `None` when the block is not probationary.
    fn promote_entry(st: &mut State, idx: u64) -> Option<Bytes> {
        let b = st.probation.pop(&idx)?;
        // second touch promotes.
        Self::insert_protected(st, idx, b.clone());
        Some(b)
    }
    /// Number of in-flight registrations; leak detection in tests.
    #[cfg(test)]
    pub(crate) fn in_flight_len(&self) -> usize {
        self.state.lock().unwrap().in_flight.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::MockSource;
    fn cache(
        data: &'static [u8],
        block_size: usize,
        capacity_bytes: usize,
    ) -> (Arc<MockSource>, BlockCache) {
        let src = Arc::new(MockSource::new(Bytes::from_static(data)));
        let c = BlockCache::new(src.clone(), block_size, capacity_bytes);
        (src, c)
    }
    #[tokio::test]
    async fn returns_block_bytes_and_caches_them() {
        let (src, c) = cache(b"0123456789", 4, 64);
        assert_eq!(&c.block(1).await.unwrap()[..], b"4567");
        assert_eq!(&c.block(1).await.unwrap()[..], b"4567");
        assert_eq!(src.read_count(), 1);
    }
    #[tokio::test]
    async fn short_tail_block_and_empty_past_eof() {
        let (_, c) = cache(b"0123456789", 4, 64);
        assert_eq!(&c.block(2).await.unwrap()[..], b"89");
        assert!(c.block(5).await.unwrap().is_empty());
    }
    #[tokio::test]
    async fn concurrent_misses_coalesce_into_one_read() {
        let src = Arc::new(
            MockSource::new(Bytes::from_static(b"0123456789"))
                .with_latency(std::time::Duration::from_millis(20)),
        );
        let c = Arc::new(BlockCache::new(src.clone(), 4, 64));
        let mut joins = Vec::new();
        for _ in 0..8 {
            let c = c.clone();
            joins.push(tokio::spawn(async move { c.block(0).await.unwrap() }));
        }
        for j in joins {
            assert_eq!(&j.await.unwrap()[..], b"0123");
        }
        assert_eq!(src.read_count(), 1);
    }
    #[tokio::test]
    async fn one_pass_scan_does_not_evict_rereferenced_blocks() {
        // capacity 4 blocks: touch block 0 twice (promoted to protected), then
        // stream blocks 1..=8 once each; block 0 must survive the scan.
        let data: &'static [u8] = Box::leak(vec![b'x'; 36].into_boxed_slice());
        let (src, c) = cache(data, 4, 16);
        let _ = c.block(0).await.unwrap();
        let _ = c.block(0).await.unwrap();
        for idx in 1..=8u64 {
            let _ = c.block(idx).await.unwrap();
        }
        let before = src.read_count();
        let _ = c.block(0).await.unwrap();
        assert_eq!(src.read_count(), before, "block 0 was evicted by the scan");
    }
    #[tokio::test]
    async fn warm_does_not_promote_into_the_protected_segment() {
        // capacity 4 blocks (probation 1): warming a block twice must leave it
        // probationary, so the next warmed block evicts it — unlike block(),
        // whose second touch would have promoted it to protected.
        let (src, c) = {
            let src = Arc::new(MockSource::new(Bytes::from_static(b"0123456789abcdef")));
            let c = BlockCache::new(src.clone(), 4, 16);
            (src, c)
        };
        let _ = c.warm(0).await.unwrap();
        let _ = c.warm(0).await.unwrap();
        let _ = c.warm(1).await.unwrap();
        let _ = c.block(0).await.unwrap();
        assert_eq!(
            src.read_count(),
            3,
            "block 0 should have been evicted from probation, not promoted"
        );
    }
    #[tokio::test]
    async fn foreground_read_coalescing_with_a_prefetch_still_promotes() {
        // an interactive read that coalesces with an in-flight prefetch fill
        // must promote just like one that arrives after the fill completes.
        let src = Arc::new(
            MockSource::new(Bytes::from_static(b"0123456789abcdef"))
                .with_latency(std::time::Duration::from_millis(20)),
        );
        let c = Arc::new(BlockCache::new(src.clone(), 4, 16));
        let warm = tokio::spawn({
            let c = c.clone();
            async move { c.warm(0).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let b = c.block(0).await.unwrap();
        assert_eq!(&b[..], b"0123");
        let _ = warm.await.unwrap();
        // the interactive touch must have promoted block 0: streaming another
        // block through the size-1 probation segment cannot evict it.
        let _ = c.warm(1).await.unwrap();
        let before = src.read_count();
        let _ = c.block(0).await.unwrap();
        assert_eq!(
            src.read_count(),
            before,
            "coalesced interactive read failed to promote"
        );
    }
    #[tokio::test]
    async fn coalesced_read_promotes_even_when_churn_evicts_first() {
        // two prefetch fills complete back-to-back, so the second evicts the
        // first from the size-1 probation segment before the coalesced
        // interactive waiter runs; fill + display must still land the block
        // in protected — never a churn-timing lottery.
        let src = Arc::new(
            MockSource::new(Bytes::from_static(b"0123456789abcdef"))
                .with_latency(std::time::Duration::from_millis(20)),
        );
        let c = Arc::new(BlockCache::new(src.clone(), 4, 16));
        let w0 = tokio::spawn({
            let c = c.clone();
            async move { c.warm(0).await }
        });
        let w1 = tokio::spawn({
            let c = c.clone();
            async move { c.warm(1).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let b = c.block(0).await.unwrap();
        assert_eq!(&b[..], b"0123");
        let _ = w0.await.unwrap();
        let _ = w1.await.unwrap();
        let _ = c.warm(2).await.unwrap();
        let before = src.read_count();
        let _ = c.block(0).await.unwrap();
        assert_eq!(
            src.read_count(),
            before,
            "displayed block lost to churn-timing race"
        );
    }
    #[tokio::test]
    async fn unreferenced_blocks_are_evicted_when_capacity_is_exceeded() {
        // capacity 2 blocks, touch 0,1,2 once: block 0 must be gone (re-read).
        let (src, c) = cache(b"0123456789ab", 4, 8);
        let _ = c.block(0).await.unwrap();
        let _ = c.block(1).await.unwrap();
        let _ = c.block(2).await.unwrap();
        let before = src.read_count();
        let _ = c.block(0).await.unwrap();
        assert_eq!(
            src.read_count(),
            before + 1,
            "block 0 should have been evicted"
        );
    }
    #[tokio::test]
    async fn tiny_block_size_does_not_preallocate_huge_maps() {
        // block_size 1 against the 256 MiB default capacity must construct
        // instantly (block count clamps) and still serve reads.
        let src = Arc::new(MockSource::new(Bytes::from_static(b"0123456789")));
        let c = BlockCache::new(src, 1, 256 << 20);
        assert_eq!(&c.block(3).await.unwrap()[..], b"3");
    }
    #[tokio::test]
    async fn zero_block_size_is_sanitized() {
        // a zero block size would divide-by-zero in the scanners; the cache
        // stores at least one byte per block.
        let src = Arc::new(MockSource::new(Bytes::from_static(b"0123")));
        let c = BlockCache::new(src, 0, 16);
        assert_eq!(c.block_size(), 1);
        assert_eq!(&c.block(2).await.unwrap()[..], b"2");
    }
    #[tokio::test]
    async fn read_errors_propagate_with_context() {
        struct Failing;
        #[async_trait::async_trait]
        impl BlockSource for Failing {
            fn size(&self) -> u64 {
                8
            }
            async fn read_block(&self, _offset: u64, _len: usize) -> anyhow::Result<Bytes> {
                Err(anyhow::anyhow!("boom"))
            }
        }
        let c = BlockCache::new(Arc::new(Failing), 4, 16);
        let err = c.block(0).await.unwrap_err();
        assert!(format!("{err:#}").contains("boom"));
    }
    #[tokio::test]
    async fn cancelled_fetcher_does_not_wedge_the_block() {
        // aborting a fetcher mid-read must leave the block fetchable: the stale
        // in-flight entry is cleaned up and the next caller becomes the fetcher.
        let src = Arc::new(
            MockSource::new(Bytes::from_static(b"0123456789"))
                .with_latency(std::time::Duration::from_millis(50)),
        );
        let c = Arc::new(BlockCache::new(src.clone(), 4, 64));
        let fetcher = tokio::spawn({
            let c = c.clone();
            async move { c.block(0).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        fetcher.abort();
        let bytes = tokio::time::timeout(std::time::Duration::from_secs(2), c.block(0))
            .await
            .expect("second caller wedged on a cancelled fetcher")
            .unwrap();
        assert_eq!(&bytes[..], b"0123");
    }
    #[tokio::test]
    async fn aborted_fetcher_does_not_leak_its_registration() {
        // cancelling a scan mid-cold-read must clean the in-flight entry even
        // if that block is never requested again.
        let src = Arc::new(
            MockSource::new(Bytes::from_static(b"0123456789"))
                .with_latency(std::time::Duration::from_millis(50)),
        );
        let c = Arc::new(BlockCache::new(src, 4, 64));
        let fetcher = tokio::spawn({
            let c = c.clone();
            async move { c.block(0).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert_eq!(c.in_flight_len(), 1, "fetch should be registered");
        fetcher.abort();
        let _ = fetcher.await;
        assert_eq!(
            c.in_flight_len(),
            0,
            "aborted fetch left a stale in-flight registration"
        );
    }
    #[tokio::test]
    async fn concurrent_waiters_all_observe_the_same_error() {
        struct FailingSlow;
        #[async_trait::async_trait]
        impl BlockSource for FailingSlow {
            fn size(&self) -> u64 {
                8
            }
            async fn read_block(&self, _offset: u64, _len: usize) -> anyhow::Result<Bytes> {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                Err(anyhow::anyhow!("boom"))
            }
        }
        let c = Arc::new(BlockCache::new(Arc::new(FailingSlow), 4, 16));
        let mut joins = Vec::new();
        for _ in 0..4 {
            let c = c.clone();
            joins.push(tokio::spawn(async move { c.block(0).await }));
        }
        for j in joins {
            let err = j.await.unwrap().unwrap_err();
            assert!(format!("{err:#}").contains("boom"));
            assert!(
                err.chain().count() >= 2,
                "waiter lost the error chain: {err:#}"
            );
        }
    }
}
