//! The shared block cache every consumer reads file bytes through. Blocks are
//! fixed-size and keyed by index; concurrent misses for one block coalesce into
//! a single FETCH -- one registration, one detached reader, though completing a short block may
//! take several physical reads; eviction is scan-resistant (SLRU): first-touch blocks
//! live in a probationary segment and only re-referenced blocks are promoted, so
//! a one-pass scan can never evict the interactive working set.
use crate::source::BlockSource;
use bytes::Bytes;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// **A block's bytes and the one fact about them that cannot be re-derived from their length**:
/// whether the SOURCE ITSELF certified that its real data ends where they end (batch 13
/// (2026-07-29), findings #1 and #4). The certificate travels WITH the bytes, in the same value,
/// stored in the same LRU entry -- which is the whole point:
///
/// - It cannot be read for the wrong length. Finality used to live in a `HashSet<u64>` keyed by
///   block INDEX, so it certified whatever length happened to be cached when someone later asked.
///   Two racing refills over `\nabczef` produced lengths 7 and 6; the 6 landed under a certificate
///   earned at 7, and `meter::classify` then certified byte 6 as the end of a file whose data runs
///   to 7 -- `search_next("e$")` reported a match the complete file does not contain.
/// - It cannot outlive the bytes it describes. Eviction drops the entry, and the certificate with
///   it, so there is no per-index side table growing with file size behind the bounded LRUs
///   (finding #5), and no state from a previous cache generation applying to a fresh fetch.
/// - It cannot be dropped by a shortcut that only passes bytes along. `Fetched::whole_block`
///   carries a `Block`, so `SearchForward`'s own seed-block reuse re-derives its `certifies_end`
///   from the same fact the read path would have (finding #4), instead of hardcoding `false`.
#[derive(Clone, Debug)]
pub(crate) struct Block {
    bytes: Bytes,
    /// Set ONLY by a refill read that came back EMPTY at `block_start + bytes.len()` -- the
    /// source's own "nothing here" (`source.rs`: "an empty buffer when `offset >= size`"). Never
    /// inferred from a short length, which certifies nothing on its own (`meter.rs`'s own P1-1
    /// law), and never set for a block that simply ends where the file's own CLAIMED size does:
    /// that block has nothing beyond it to fetch, but nobody has asked the source about it either.
    ends_data: bool,
}
impl Block {
    /// The ordinary case: bytes nobody has certified anything about. `pub(crate)` so a consumer can
    /// build the empty stand-in a wholly-empty read needs (`SearchBackward`'s own `warmed` reuse
    /// slot, `scan.rs`) -- deliberately the ONLY constructor outside this module, since `false` is
    /// the conservative value. There is no way anywhere in this crate to mint `ends_data: true`
    /// except through this cache observing the source answer empty: the certificate is earned, in
    /// the same spirit as `meter::CertifiedEnd`'s own private constructor.
    pub(crate) fn uncertified(bytes: Bytes) -> Self {
        Self {
            bytes,
            ends_data: false,
        }
    }
    /// Whether the source certified that its real data ends exactly at the end of these bytes.
    pub(crate) fn ends_data(&self) -> bool {
        self.ends_data
    }
}
/// Read access to the bytes, so every existing `block.len()` / `block.slice(..)` / `&block[..]`
/// reads exactly as it did when this was a bare `Bytes`. There is deliberately no `DerefMut` and
/// no public field: the certificate and the bytes are only ever replaced together, by
/// `BlockCache::absorb`.
impl std::ops::Deref for Block {
    type Target = Bytes;
    fn deref(&self) -> &Bytes {
        &self.bytes
    }
}

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
impl SharedError {
    /// The error a `FetchCompletion`'s involuntary teardown (`Drop`) publishes: this fetch's own
    /// future was destroyed without reaching an ending -- a panic mid-read, or the whole runtime
    /// shutting down with it still parked (restructure R2, 2026-07-27). A panic becomes ordinary
    /// error propagation through the contract every caller already handles, instead of a closed
    /// channel nobody can interpret -- see `FetchCompletion`'s own `Drop` doc comment.
    fn torn_down(idx: u64) -> Self {
        SharedError(Arc::new(anyhow::anyhow!(
            "block {idx}'s fetch was torn down before completing (a panic mid-read, or the \
             runtime shut down while it was still in flight)"
        )))
    }
}

/// **Bytes one fetch has read so far, in the cheapest representation for how many reads produced
/// them** (batch 15 (2026-07-30), finding #3; claim corrected by batch 17 (2026-07-31), finding
/// #3). **Not "every case copies nothing"** -- `push` has an explicit copy fallback below, and
/// `BytesMut::extend_from_slice` may reallocate and move whatever it already holds, which is the
/// allocator's business and not something this type can promise. What it does guarantee:
///
/// - a fresh fetch starts `Empty` and allocates nothing at all -- the old `BytesMut::with_capacity
///   (block_size)` reserved a megabyte before knowing whether a single read would answer;
/// - the ordinary miss (one read delivers the block) ends as `One`, holding the source's own
///   buffer. The old loop `extend_from_slice`'d it into a fresh allocation, so EVERY cache miss
///   paid a full block-sized memcpy on top of the read -- the cost the reviewer measured at
///   `source.rs`'s own already-allocated `Bytes`;
/// - a second read promotes `One` to `Many` through `Bytes::try_into_mut`, which hands back the
///   existing allocation WHEN this fetch holds the only reference. Checking the accumulator out
///   (`Entry::CheckedOut`) rather than cloning it removes the CACHE as a second holder, which is
///   what the quadratic behaviour turned on -- it does not make uniqueness certain, because the
///   bytes may have other holders this module never sees. The `Err` arm below is not hypothetical: a `Bytes` the SOURCE still retains a
///   slice of (`MockSource` does exactly that) or a `'static` one has no unique ownership to claim,
///   so a first-read answer commonly lands there. That copies once, when a second read arrives --
///   not once per pass, which is the property that matters;
/// - a RESUMED generation is handed the accumulator itself -- a move, not a clone (`Acc` is
///   deliberately not `Clone`) -- so cancelling mid-fetch any number of times adds no copy of the
///   prefix. Seeding a fresh buffer from the cached prefix instead cost one copy of that prefix
///   PER GENERATION: 1+2+...+n, which at a 1 MiB block size is roughly 512 GiB for a source
///   cancelled once per byte;
/// - and growth is geometric, so assembling an N-byte block costs O(N) copying in total. That --
///   amortized linear, plus the two zero-copy paths above -- is the whole of the guarantee;
///   individual appends may still move, and
///   `the_accumulator_costs_no_copy_on_a_miss_and_grows_geometrically` asserts the bound rather
///   than any address.
#[derive(Debug)]
enum Acc {
    Empty,
    One(Bytes),
    Many(bytes::BytesMut),
}
impl Acc {
    fn len(&self) -> usize {
        match self {
            Acc::Empty => 0,
            Acc::One(b) => b.len(),
            Acc::Many(m) => m.len(),
        }
    }
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Appends one read's answer. The `One -> Many` promotion is where `try_into_mut` earns its
    /// keep: it hands back the existing allocation when this is the only reference to it. Checking
    /// the accumulator out (`Entry::CheckedOut`) removes the CACHE as a second holder, which is
    /// what makes that reachable for a RESUMED accumulator -- it does not guarantee uniqueness,
    /// because the bytes may have holders this module never sees. The `Err` arm below is
    /// consequently a real path, not a hypothetical one: a source that retains a slice of what it
    /// returned (`MockSource` does) or a `'static` answer leaves nothing to claim. It copies once,
    /// when a second read arrives -- not once per pass, which is the property that matters.
    fn push(&mut self, more: Bytes) {
        let mut buf = match std::mem::replace(self, Acc::Empty) {
            Acc::Empty => {
                *self = Acc::One(more);
                return;
            }
            // zero-copy when this is the only reference. Checking out removes the CACHE as a
            // holder, which is what a resumed accumulator needs -- but not a guarantee even there:
            // those bytes may still be a source-retained slice or a `'static`, with nothing to
            // claim uniquely. The copy is then
            // real, and paid once per fetch rather than once per pass.
            Acc::One(b) => b
                .try_into_mut()
                .unwrap_or_else(|b| bytes::BytesMut::from(&b[..])),
            Acc::Many(m) => m,
        };
        buf.extend_from_slice(&more);
        *self = Acc::Many(buf);
    }
    /// Test-only: the allocation the accumulator is currently sitting in. This exists so
    /// `the_accumulator_costs_no_copy_on_a_miss_and_grows_geometrically` can witness the GROWTH
    /// POLICY -- capacity changing a logarithmic number of times is what bounds total copying --
    /// rather than reason about addresses, which belong to the allocator and not to this type.
    #[cfg(test)]
    fn capacity(&self) -> usize {
        match self {
            Acc::Empty => 0,
            Acc::One(b) => b.len(),
            Acc::Many(m) => m.capacity(),
        }
    }
    fn into_bytes(self) -> Bytes {
        match self {
            Acc::Empty => Bytes::new(),
            Acc::One(b) => b,
            Acc::Many(m) => m.freeze(),
        }
    }
}
/// **What one LRU slot holds** (batch 15 (2026-07-30), findings #2, #3 and #5). Batch 14 stored
/// only finished blocks and kept a fetch's own partial progress in a local on its stack, to be
/// hand-delivered to the cache at each of three exits -- and two of the three got it wrong: the
/// abandon path stored it in a second critical section AFTER deregistering (so a replacement could
/// register in the gap and re-read from zero), and the error path did not store it at all (so a
/// source that failed once redid every successful pass). Making the accumulator the CACHE's, with
/// an explicit state for "a fetch has it", removes the choice: there is no exit that can forget it,
/// and resuming is a hand-back rather than a copy.
#[derive(Debug)]
enum Entry {
    /// A finished block -- the ONLY thing `get` ever returns, and the only state a consumer can
    /// observe. "Resolved" is `Inner::is_resolved`'s own judgement, made once by the fetch that
    /// produced it rather than re-derived at every read.
    Resolved(Block),
    /// Bytes read by a fetch that did not finish -- an abandoned one, or one whose source errored
    /// partway. The cache owns them between generations; the next fetch for this index resumes
    /// from them.
    Partial(Acc),
    /// The accumulator is checked out by the fetch registered for this index right now. Both
    /// transitions (`in_flight` registration and this) happen under the same lock, so this state
    /// ALWAYS has a live registration to join -- which is what makes it unobservable: a caller
    /// finding it never reads it, it joins the fetch that holds it and waits for the answer.
    CheckedOut,
}
impl Entry {
    /// The finished block, if this slot holds one.
    fn resolved(&self) -> Option<&Block> {
        match self {
            Entry::Resolved(b) => Some(b),
            _ => None,
        }
    }
}
/// A block-aligned, scan-resistant cache over a `BlockSource`.
pub struct BlockCache {
    inner: Arc<Inner>,
}
/// The cache's actual shared state, `Arc`-wrapped separately from `BlockCache` itself so a
/// detached fetch task (see `get`'s own doc comment on the miss path) can hold an owned,
/// `'static` handle to it without `BlockCache`'s own public shape changing at all -- every
/// existing caller still holds a plain `Arc<BlockCache>` exactly as before construction,
/// `block`/`warm` still take `&self`. Batch 4 (2026-07-24), finding #11's own restructure.
struct Inner {
    source: Arc<dyn BlockSource>,
    block_size: usize,
    state: Mutex<State>,
    // found in PR #44 pass 8 (U-cache): a positive "this call coalesced" signal.
    // `in_flight_len` alone cannot serve this role -- it counts distinct in-flight
    // BLOCKS, not waiters per block, so it cannot distinguish "one fetcher, zero
    // waiters" from "one fetcher, many waiters," exactly the ambiguity a coalescing
    // test must not paper over by inferring from `in_flight_len` staying put.
    // Incremented+published from `get()`'s own waiter-discovery branch, already
    // under the same lock that serializes every `in_flight` mutation -- see
    // `coalesced_events`'s own doc comment for the test-facing accessor.
    coalesced: AtomicU64,
    coalesced_tx: tokio::sync::watch::Sender<u64>,
}
impl Inner {
    /// The ordinary lock: panics on poison, matching this crate's existing discipline elsewhere
    /// (an UNEXPECTED poison here means some critical section panicked for a reason nobody has
    /// reasoned about -- e.g. `in_flight::InFlight::release_waiter`'s own underflow `.expect`
    /// firing -- and failing loud on every subsequent use is the right default until something
    /// has explicitly decided it is safe to keep going, which is exactly
    /// `lock_state_tolerating_poison`'s own, narrower job below).
    fn lock_state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }
    /// The one call site that must not panic on poison (restructure R2, 2026-07-27):
    /// `FetchCompletion::drop`'s involuntary-teardown arm runs DURING unwinding (a panic
    /// mid-read, or a task destroyed by runtime shutdown) -- if the state mutex happens to
    /// already be poisoned for ANY reason, a plain `.lock().unwrap()` here would panic a SECOND
    /// time while the first panic is still unwinding, which Rust turns into an immediate process
    /// ABORT, not a catchable error. Today's `InFlightGuard::drop` has exactly this hazard;
    /// closing it is part of this increment.
    ///
    /// Recovering also HEALS the mutex (`clear_poison`), not merely this one acquisition:
    /// poisoning is otherwise harmless here (the worst a panicked critical section leaves behind
    /// is a lost LRU update -- one future cache miss -- never corrupted `in_flight` state, since
    /// every mutation through this module is a short, synchronous map/counter update), so once
    /// one caller has decided the guarded data is trustworthy enough to read, every OTHER
    /// caller -- including the ordinary, panic-on-poison `lock_state` above -- should not have to
    /// independently re-discover the same poisoned mutex and panic on it forever after a single,
    /// already-recovered-from hiccup. Found while testing this exact method (a first draft called
    /// `into_inner()` without `clear_poison()`, which survived the one acquisition it was written
    /// for but left the ordinary lock panicking on every later call for the rest of the process's
    /// life -- a resilience regression, not the abort hazard this method exists for, but real).
    fn lock_state_tolerating_poison(&self) -> std::sync::MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                self.state.clear_poison();
                poisoned.into_inner()
            }
        }
    }
    /// **Publishes one refill outcome and hands back what the cache now AUTHORITATIVELY holds**
    /// (batch 13 (2026-07-29), finding #1). The one write path for a refilled entry, and the reason
    /// two uncoalesced refills cannot corrupt each other: batch 12 had each caller `put` its own
    /// caller-local snapshot, so a refill that got 6 bytes could overwrite one that had already got
    /// 7, and the EOF certificate the 7-byte read had earned then applied to the 6 -- fabricating an
    /// end of data one byte early, and with it `$`/`\b` matches the real file does not contain.
    ///
    /// An entry only ever moves FORWARD here: longer bytes, or the same bytes plus a certificate.
    /// The caller continues its loop from the value this returns, not from its own snapshot, so a
    /// racing pair converges on the longer answer rather than ping-ponging. Returning the held
    /// value is sound because a `BlockSource` is FIXED (`source.rs`): both refills read the same
    /// offsets of the same data, so the longer answer contains the shorter one.
    #[cfg(test)]
    fn absorb(&self, idx: u64, candidate: Block) -> Block {
        let mut st = self.lock_state();
        Self::absorb_into(&mut st, idx, candidate)
    }
    /// `absorb`'s body, for the caller that already holds the lock (`FetchCompletion::publish` does
    /// its deregistration and its write in ONE critical section, and must not drop the lock between
    /// them).
    fn absorb_into(st: &mut State, idx: u64, candidate: Block) -> Block {
        let held = st
            .protected
            .peek(&idx)
            .or_else(|| st.probation.peek(&idx))
            .and_then(Entry::resolved)
            .cloned();
        match held {
            // a racing refill already got strictly further: its answer supersedes this one.
            Some(held) if held.bytes.len() > candidate.bytes.len() => held,
            // same bytes, and the entry already carries the certificate: nothing to add, and
            // writing `candidate` over it would DROP a fact (an uncertified extension that raced a
            // certification of the identical length).
            Some(held) if held.bytes.len() == candidate.bytes.len() && held.ends_data => held,
            // this answer is the furthest anyone has got: it becomes the entry. Note what is NOT
            // preserved -- a certificate held for a SHORTER length. Bytes read past a certified end
            // disprove that certificate (the source answered empty at an offset it later answered
            // bytes at), and between a stale certificate and directly observed bytes, dropping the
            // certificate is the safe direction: losing an end claim costs one extra read, keeping
            // a wrong one fabricates EOF.
            _ => {
                Self::overwrite_preserving_recency(st, idx, Entry::Resolved(candidate.clone()));
                candidate
            }
        }
    }
    /// Hands a fetch's unfinished bytes back to the cache (batch 15 (2026-07-30)). Called by every
    /// ending that is not a successful publish, inside that ending's own critical section. Empty
    /// progress writes nothing: a fetch that read nothing has nothing to resume from, and inserting
    /// an empty entry would cost an eviction for no information.
    fn store_partial(st: &mut State, idx: u64, acc: Acc, added: bool) {
        // **an evicted slot is not resurrected by a generation that read nothing** (batch 16
        // (2026-07-31), finding #3). `check_out` leaves `Entry::CheckedOut` behind; if churn evicts
        // that marker while the fetch is in flight and the fetch then ends having added no bytes,
        // putting the unchanged accumulator back would reinstate an entry the cache had already
        // decided to drop -- and evict something newer to make room for it. Handing back what this
        // generation was GIVEN is only worth an eviction if this generation improved it.
        if !added
            && !matches!(
                st.protected.peek(&idx).or_else(|| st.probation.peek(&idx)),
                Some(Entry::CheckedOut)
            )
        {
            return;
        }
        if acc.is_empty() {
            // still clear a checkout marker, so a later caller does not join a fetch that has ended
            // (`Entry::CheckedOut`'s own doc comment: the state is only sound while a registration
            // is live, and this runs inside the same critical section that ended it).
            if matches!(st.protected.peek(&idx), Some(Entry::CheckedOut)) {
                st.protected.pop(&idx);
            } else if matches!(st.probation.peek(&idx), Some(Entry::CheckedOut)) {
                st.probation.pop(&idx);
            }
            return;
        }
        // never over a RESOLVED entry: a finished block is strictly better than the partial that
        // was on its way to becoming one (and, after an eviction and refetch, may be a later
        // generation's answer entirely).
        if st
            .protected
            .peek(&idx)
            .or_else(|| st.probation.peek(&idx))
            .is_some_and(|e| e.resolved().is_some())
        {
            return;
        }
        Self::overwrite_preserving_recency(st, idx, Entry::Partial(acc));
    }
    /// Takes the cache's own unfinished bytes for `idx` and marks the slot as checked out (batch
    /// 15 (2026-07-30), finding #3). The fetch about to be registered gets the accumulator ITSELF,
    /// not a copy of it, which is what makes resuming free at any number of generations; the
    /// `CheckedOut` marker is what keeps the emptied slot from being mistaken for a finished block
    /// in the meantime. Both this and the registration happen under one lock, so the marker never
    /// outlives the fetch that justifies it.
    ///
    /// A RESOLVED entry is left alone and yields `Acc::Empty`: a fetch registered against one is
    /// re-fetching a block that has since been superseded, and must not consume it.
    fn check_out(st: &mut State, idx: u64) -> Acc {
        let slot = st
            .protected
            .peek_mut(&idx)
            .or_else(|| st.probation.peek_mut(&idx));
        match slot {
            Some(e @ Entry::Partial(_)) => match std::mem::replace(e, Entry::CheckedOut) {
                Entry::Partial(acc) => acc,
                _ => unreachable!("just matched Partial under this same borrow"),
            },
            _ => Acc::Empty,
        }
    }
    /// Writes an entry's bytes WITHOUT touching either segment's recency (batch 13 (2026-07-29),
    /// finding #6). `LruCache::put` on an EXISTING key moves it to most-recently-used, which batch
    /// 12's refill did unconditionally -- so a `warm()`-driven refill (prefetch, the background
    /// index scan) refreshed a protected block's recency and could evict the interactive working
    /// set, contradicting the consumer-truthful promotion rule `docs/block_cache.md` states and
    /// `get_raw` already enforces with `peek`. A refill is not a consumer touch at all: whichever
    /// touch triggered it already had its promotion decided upstream, so this write must be
    /// invisible to eviction order.
    ///
    /// A block evicted between the read and this write is simply re-inserted probationary, exactly
    /// as an ordinary miss would have left it.
    fn overwrite_preserving_recency(st: &mut State, idx: u64, block: Entry) {
        if let Some(slot) = st.protected.peek_mut(&idx) {
            *slot = block;
        } else if let Some(slot) = st.probation.peek_mut(&idx) {
            *slot = block;
        } else {
            st.probation.put(idx, block);
        }
    }
    /// **Whether this block needs nothing more from the source** -- the single definition of
    /// "done", consulted by the requester (`BlockCache::get`, deciding whether a cache hit is an
    /// answer) and by the producer (`BlockCache::fetch`, deciding whether to read again). Batch 13
    /// had the same four conditions written out at one site only, which is why an entry left short
    /// by an abandoned fetch could be handed straight to a consumer.
    ///
    /// Full, empty (nothing exists at or past this block's own start), certified by the source
    /// itself, or short only because the file's own CLAIMED size ends inside this block -- that
    /// last one has nothing beyond it to fetch, though nobody has asked the source about it, which
    /// is why it does not carry a certificate.
    fn is_resolved(&self, idx: u64, len: usize, ends_data: bool) -> bool {
        let bs = self.block_size;
        if len == 0 || len >= bs || ends_data {
            return true;
        }
        let block_start = idx * bs as u64;
        let content_end = block_start + len as u64;
        content_end
            >= block_start
                .saturating_add(bs as u64)
                .min(self.source.size())
    }
}
/// Everything the cache remembers, and **nothing keyed by block index outside the two LRUs**
/// (batch 13 (2026-07-29), findings #2 and #5). Batch 12 kept a refill's own findings in two side
/// tables here -- `confirmed_final: HashSet<u64>` and `refill_attempts: HashMap<u64, u32>` -- both
/// keyed by index, neither bounded by the LRUs that hold the bytes they described. Between them
/// they produced three of that batch's six review findings, all of the same shape: state about a
/// cache ENTRY that outlived the entry, applied to a later generation of it, and grew with file
/// size behind two bounded caches. Both are gone. What a refill learns is now part of the entry
/// (`Block::ends_data`) and what it spends is now nothing at all (the completion pass keeps no
/// counter -- see its own doc comment).
struct State {
    // first-touch blocks; a full-file scan lives and dies here.
    probation: lru::LruCache<u64, Entry>,
    // re-referenced blocks; the interactive working set.
    protected: lru::LruCache<u64, Entry>,
    // the in-flight registry (restructure R2, 2026-07-27) -- see `in_flight`'s own module doc.
    in_flight: in_flight::InFlight,
}

/// One in-flight block fetch, as the cache's shared state sees it. Note what is NOT here: a
/// `watch::Receiver`. Before restructure R2 (2026-07-27), `in_flight` stored a receiver clone,
/// which made `Sender::receiver_count()` permanently >= 1 (>= 2 during a read, once the fetch's
/// own guard subscribed a second one) -- "is anyone still waiting?" had no answer available
/// anywhere in the process (batch 5, finding #4's own design landmine). Nothing in this design
/// consults a receiver count at all; the waiter count living inside `in_flight::Registration`,
/// behind the same mutex as everything else here, is the only definition of live interest that
/// exists.
struct FetchSlot {
    outcome: tokio::sync::watch::Sender<Option<Result<Block, SharedError>>>,
    /// Bumped whenever the waiter count reaches zero (`WaiterTicket::drop`, AFTER the cache lock
    /// is released -- unit D's reviewed "no wake-into-contention" property, kept). The fetch
    /// task's phase-1 select races `.changed()` on this against admission; the actual decision is
    /// re-derived under the lock every time (`FetchCompletion::try_abandon`), so a stale or
    /// coalesced signal here can never cause a wrong abandon -- only ever prompt a re-check.
    waiterless: tokio::sync::watch::Sender<u64>,
}

/// The in-flight registry (restructure R2, 2026-07-27, structural-cache.md §3.2). Deliberately
/// its own module with no PUBLIC removal: the only way to end a registration is through the
/// `FetchCompletion` `register` hands to the fetch task, and every mutating method here is
/// `pub(super)` -- reachable from `cache`'s own `get`/`WaiterTicket`/`FetchCompletion`, never from
/// outside this file, let alone outside the crate. A requester only ever holds a `WaiterTicket`,
/// which cannot reach any of these directly -- unit D's finding-#11 win (coalescing survives
/// supersession) moved out of a doc-comment argument and into these signatures.
mod in_flight {
    use super::{Arc, FetchCompletion, FetchSlot, Inner, WaiterTicket};
    use std::collections::HashMap;

    pub(super) struct InFlight(HashMap<u64, Registration>);
    struct Registration {
        slot: Arc<FetchSlot>,
        waiters: usize,
    }
    impl InFlight {
        pub(super) fn new() -> Self {
            Self(HashMap::new())
        }
        // test-only: leak detection (`BlockCache::in_flight_len`) and the white-box race-
        // argument tests both need this; nothing in production ever queries a bare count.
        #[cfg(test)]
        pub(super) fn len(&self) -> usize {
            self.0.len()
        }
        /// Joins an existing fetch, if one is registered for `idx`: bumps the waiter count under
        /// the caller's own `&mut` (i.e. the cache lock, structurally) and hands back a ticket
        /// subscribed to the SAME slot every other waiter -- including the initiator -- holds.
        pub(super) fn join(&mut self, idx: u64, inner: &Arc<Inner>) -> Option<WaiterTicket> {
            let reg = self.0.get_mut(&idx)?;
            reg.waiters += 1;
            Some(WaiterTicket {
                inner: inner.clone(),
                idx,
                rx: reg.slot.outcome.subscribe(),
                slot: reg.slot.clone(),
                arrival: super::Arrival::Joiner,
            })
        }
        /// Registers a brand new fetch: hands back the initiator's ticket (the SAME type every
        /// joiner gets -- symmetry is the type, not a comment), the single completion token the
        /// fetch task this call spawns will own for its whole life, AND that fetch task's own
        /// `waiterless` receiver -- minted here, under this same lock, rather than left for the
        /// fetch task to subscribe to itself once it starts running.
        ///
        /// That ordering is load-bearing, not stylistic: `watch::Sender::subscribe` seeds a fresh
        /// receiver with whatever value is CURRENT at the moment of the call. If the fetch task
        /// subscribed on its own first poll instead, a waiter dropping
        /// (and bumping `waiterless`) in the window between this call returning and that task
        /// actually being scheduled would already be reflected in the channel BEFORE the
        /// subscribe -- seeding the receiver as already-caught-up, so its own `changed()` would
        /// never fire for a bump that, from its perspective, already happened. There is no
        /// self-healing retry for this: `release_waiter` only bumps on a genuine 1->0 transition,
        /// and with zero waiters left no further transition can ever occur -- the fetch would
        /// commit to (and publish) a read nobody asked for, silently reintroducing finding #4
        /// through exactly the scheduling gap this restructure exists to close. Minting the
        /// receiver here instead makes the ordering a capability rather than a discipline: the
        /// earliest any `WaiterTicket` for this registration can possibly be dropped is AFTER
        /// this whole call returns (the caller cannot drop what it does not have yet), so a
        /// receiver created before that return can never miss a bump.
        pub(super) fn register(
            &mut self,
            idx: u64,
            inner: &Arc<Inner>,
            acc: super::Acc,
        ) -> (
            WaiterTicket,
            FetchCompletion,
            tokio::sync::watch::Receiver<u64>,
        ) {
            let (outcome_tx, outcome_rx) = tokio::sync::watch::channel(None);
            let (waiterless_tx, waiterless_rx) = tokio::sync::watch::channel(0u64);
            let slot = Arc::new(FetchSlot {
                outcome: outcome_tx,
                waiterless: waiterless_tx,
            });
            self.0.insert(
                idx,
                Registration {
                    slot: slot.clone(),
                    waiters: 1,
                },
            );
            let ticket = WaiterTicket {
                inner: inner.clone(),
                idx,
                rx: outcome_rx,
                slot: slot.clone(),
                arrival: super::Arrival::Initiator,
            };
            let completion = FetchCompletion {
                inner: inner.clone(),
                idx,
                slot,
                live: true,
                checked_out_len: acc.len(),
                acc,
            };
            (ticket, completion, waiterless_rx)
        }
        /// How many live waiters this exact registration has -- `None` if `idx` is unregistered,
        /// or if a DIFFERENT registration (identity-checked by `Arc::ptr_eq` on the slot) has
        /// since taken its place. Callers hold the lock across this AND whatever they do with the
        /// answer -- see `FetchCompletion::try_abandon`'s own doc comment for why that continuity
        /// is the entire race argument.
        pub(super) fn waiters(&self, idx: u64, slot: &Arc<FetchSlot>) -> Option<usize> {
            self.0
                .get(&idx)
                .filter(|reg| Arc::ptr_eq(&reg.slot, slot))
                .map(|reg| reg.waiters)
        }
        /// Ends a registration -- identity-checked against `slot`, so a stale token (one whose
        /// registration was already replaced by a fresher fetch) can never clear the wrong entry.
        /// Reachable only from `FetchCompletion`'s own consuming methods (`publish`,
        /// `try_abandon`, `Drop`), all defined in the parent module: nothing else in this file
        /// ever holds both a `&mut State` and a `FetchCompletion` to pass here.
        pub(super) fn end(&mut self, idx: u64, slot: &Arc<FetchSlot>) {
            if self
                .0
                .get(&idx)
                .is_some_and(|cur| Arc::ptr_eq(&cur.slot, slot))
            {
                self.0.remove(&idx);
            }
        }
        /// Releases one waiter's interest -- `WaiterTicket::drop`'s only call site. Returns
        /// whether the count reached zero (the caller publishes `waiterless` from that, but only
        /// AFTER this lock is released -- see `WaiterTicket::drop`). A stale ticket (its
        /// registration already replaced) is a silent no-op: nothing to release, nothing to
        /// report.
        pub(super) fn release_waiter(&mut self, idx: u64, slot: &Arc<FetchSlot>) -> bool {
            let Some(reg) = self.0.get_mut(&idx) else {
                return false;
            };
            if !Arc::ptr_eq(&reg.slot, slot) {
                return false;
            }
            reg.waiters = reg
                .waiters
                .checked_sub(1)
                .expect("a WaiterTicket released more waiter interest than it ever joined");
            reg.waiters == 0
        }
    }
}

/// Which of the two symmetric callers a `WaiterTicket` belongs to -- replaces the old
/// `is_first_requester: bool` (restructure R2, 2026-07-27). Not cosmetic: `promote &&
/// !is_first_requester` (a negated bool guarding the scan-resistance invariant) becomes
/// `matches!(arrival, Arrival::Joiner)`, which cannot be inverted by accident the way a bare
/// negation can. Pinned in both directions -- the negative case
/// (`Arrival::Initiator`'s own touch must NOT count as the promoting one) by
/// `one_pass_scan_does_not_evict_rereferenced_blocks`; the positive case (`Arrival::Joiner`'s
/// touch MUST count) by `foreground_read_coalescing_with_a_prefetch_still_promotes` and
/// `coalesced_read_promotes_even_when_churn_evicts_first`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Arrival {
    Initiator,
    Joiner,
}

/// RAII proof that one live caller wants this block. Its `Drop` is the ONLY thing that can
/// decrement the waiter count -- so "interest" is exactly "a ticket exists," with no second
/// definition of waiting to drift from the first. A requester holds one of these and NOTHING
/// else; there is no method on this type that reaches `InFlight::end` at all (unit D's finding-#11
/// win: coalescing across supersession is now a capability question, not a discipline one).
#[must_use = "a WaiterTicket IS the interest -- dropping it early tells the fetch nobody wants \
              this block anymore"]
struct WaiterTicket {
    inner: Arc<Inner>,
    idx: u64,
    rx: tokio::sync::watch::Receiver<Option<Result<Block, SharedError>>>,
    slot: Arc<FetchSlot>,
    arrival: Arrival,
}
impl Drop for WaiterTicket {
    fn drop(&mut self) {
        // decrement under the lock, notify outside it: keeps unit D's reviewed "no
        // wake-into-contention" property (a fetch task woken by this signal never contends the
        // very lock this drop just released).
        let now_waiterless = {
            let mut st = self.inner.lock_state();
            st.in_flight.release_waiter(self.idx, &self.slot)
        };
        if now_waiterless {
            self.slot.waiterless.send_modify(|n| *n += 1);
        }
    }
}

/// Whether an abandon attempt actually ended the registration, or a joiner beat it to the lock.
enum Abandon {
    Done,
    StillWanted(FetchCompletion),
}

/// The sole right to END block `idx`'s fetch. Minted once, by `InFlight::register`; moved into
/// the fetch task; consumed by exactly one of `publish`, the private `try_abandon`-driven
/// abandon, or `Drop`. Because deregistration is reachable only through these methods,
/// "deregistered while still reading" is not expressible: this token sits inside the read's own
/// stack frame until the read returns (restructure R2, 2026-07-27, structural-cache.md §3.3).
#[must_use = "a registered fetch must end -- publish, abandon, or be dropped into an error"]
struct FetchCompletion {
    inner: Arc<Inner>,
    idx: u64,
    slot: Arc<FetchSlot>,
    /// **The bytes this fetch has read, owned by the token that must end it** (batch 15
    /// (2026-07-30)). Every ending below writes them back under the SAME lock that deregisters:
    /// `publish` (as a finished block, or as preserved progress beside a propagated error),
    /// `try_abandon`'s `Done` arm, and `Drop`. A fetch cannot end without accounting for what it
    /// read, because the accounting is inside the endings rather than at the call sites that
    /// reach them.
    acc: Acc,
    /// how many bytes `acc` held when this fetch was handed it -- so "did this generation actually
    /// add anything" is a fact rather than a guess (batch 16 (2026-07-31), finding #3).
    checked_out_len: usize,
    // sentinel for "already ended, Drop must be a no-op" -- flipped by every consuming path
    // (`publish`, `try_abandon`'s Done arm) before `self` actually drops, so `Drop`'s own
    // involuntary-teardown arm fires ONLY when neither ran: a panic, or runtime shutdown.
    live: bool,
}
impl FetchCompletion {
    /// Normal ending: deregister and publish, one critical section, one outcome -- exactly
    /// `fetch_and_publish`'s pre-restructure ordering (remove, insert-if-ok, drop the lock, THEN
    /// send), unchanged in shape.
    fn publish(mut self, outcome: Result<Block, SharedError>) {
        // batch 14 (2026-07-30), finding #3: through `absorb`, never a bare `put`. This used to
        // publish its own snapshot unconditionally, so an entry could roll BACKWARD -- a refill
        // extended block 0 to `ABCD`, then a legal concurrent fetch that answered a short `A`
        // replaced it, and every later read had to rediscover the rest. `absorb` only ever moves an
        // entry forward, and what goes out on the channel is the MERGED value, so a waiter can
        // never be handed less than the cache already knows either.
        let outcome = {
            let mut st = self.inner.lock_state();
            st.in_flight.end(self.idx, &self.slot);
            match outcome {
                Ok(block) => Ok(Inner::absorb_into(&mut st, self.idx, block)),
                // batch 15 (2026-07-30), finding #5: **the passes that SUCCEEDED are kept.** This
                // used to publish the error and drop the accumulator, so a source that delivered
                // `A`, failed at `B`, then recovered performed A, B(fail), A, B -- redoing work it
                // had already paid for. The bytes are real regardless of what the next read did, so
                // they go back as resumable progress; the ERROR itself is deliberately not cached
                // (it belongs to this attempt, not to the block), it only travels to the waiters
                // who asked.
                Err(e) => {
                    let added = self.acc.len() > self.checked_out_len;
                    let acc = std::mem::replace(&mut self.acc, Acc::Empty);
                    Inner::store_partial(&mut st, self.idx, acc, added);
                    Err(e)
                }
            }
        };
        self.live = false;
        let _ = self.slot.outcome.send(Some(outcome));
    }
    /// The waiterless ending: the abandon decision and the deregistration are ONE critical
    /// section (one `lock_state()` call spanning both `waiters` and, if zero, `end`) -- which IS
    /// the entire race argument. A joiner's own `InFlight::join` takes the SAME lock to
    /// increment, so it either lands FIRST (this call sees `waiters() >= 1`, returns
    /// `StillWanted` and the joiner's ticket is already live against the still-registered slot),
    /// or lands SECOND (this call has already removed the entry, so the joiner's `join` finds
    /// nothing and the caller mints a genuinely fresh fetch instead). No joiner can ever land in
    /// between and end up waiting on a dead channel. Publishes NOTHING on the `Done` arm: there
    /// is, by construction, nobody left to receive anything.
    fn try_abandon(mut self) -> Abandon {
        let mut st = self.inner.lock_state();
        let waiters = st
            .in_flight
            .waiters(self.idx, &self.slot)
            .expect("this fetch's own registration must still exist until it ends itself");
        if waiters == 0 {
            // batch 15 (2026-07-30), finding #2: deregistering and storing the progress are ONE
            // critical section. Batch 14 dropped the lock here and absorbed afterwards, which left
            // a window where a replacement could register, find no entry, and seed itself from
            // nothing -- re-reading offsets this fetch had already read (a staged probe saw
            // `[0, 0]` where `[0, 1]` was the whole point), directly contradicting the
            // "repeats nothing" guarantee `docs/block_cache.md` states. Nothing can observe the
            // registration as gone until the bytes it produced are already there to be resumed
            // from.
            st.in_flight.end(self.idx, &self.slot);
            let added = self.acc.len() > self.checked_out_len;
            let acc = std::mem::replace(&mut self.acc, Acc::Empty);
            Inner::store_partial(&mut st, self.idx, acc, added);
            drop(st);
            self.live = false;
            Abandon::Done
        } else {
            drop(st);
            Abandon::StillWanted(self)
        }
    }
}
impl Drop for FetchCompletion {
    /// The involuntary arm: this task's own future was destroyed without reaching an ending -- a
    /// panic mid-read, or the whole runtime shutting down with the fetch still parked (both
    /// indistinguishable from inside this frame, and both correctly produce the same outcome --
    /// see this method's own provenance note below). Publishes an ERROR instead of leaving a
    /// closed channel behind: a panic becomes ordinary error propagation through the contract
    /// every caller already handles (the sweep's give_up budget, nav's error surfacing), not a
    /// state nothing can interpret. `lock_state_tolerating_poison`, not `lock_state`: this runs
    /// DURING unwinding, and a `.lock().unwrap()` panicking a second time here on an
    /// already-poisoned mutex would abort the process rather than merely fail a task -- the exact
    /// hazard `InFlightGuard::drop` carried before this restructure.
    ///
    /// Runtime shutdown deliberately takes this SAME arm, not a distinguished one: from inside
    /// this frame, a panic and a runtime tearing down mid-`.await` are the same event (the frame
    /// is destroyed without reaching `publish` or `try_abandon`), so treating them differently
    /// would mean distinguishing the indistinguishable. The outcome is correct for both: a live
    /// waiter gets a loud error instead of a silent hang; if waiters are being torn down too,
    /// `watch::Sender::send` with no receivers left is a documented no-op, not an error this path
    /// needs to handle specially.
    fn drop(&mut self) {
        if !self.live {
            return;
        }
        let mut st = self.inner.lock_state_tolerating_poison();
        st.in_flight.end(self.idx, &self.slot);
        // the THIRD ending, accounting for its bytes exactly like the other two (batch 15
        // (2026-07-30)): a panic mid-read destroys this frame, not the reads that already
        // succeeded, and dropping them would make the next attempt redo them -- the same waste
        // finding #5 names on the error path, arriving by the one route nobody takes deliberately.
        // In the same critical section as the deregistration, for finding #2's reason.
        let added = self.acc.len() > self.checked_out_len;
        let acc = std::mem::replace(&mut self.acc, Acc::Empty);
        Inner::store_partial(&mut st, self.idx, acc, added);
        drop(st);
        let _ = self
            .slot
            .outcome
            .send(Some(Err(SharedError::torn_down(self.idx))));
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
        let (coalesced_tx, _) = tokio::sync::watch::channel(0);
        Self {
            inner: Arc::new(Inner {
                source,
                block_size: block_size.max(1),
                state: Mutex::new(State {
                    probation: lru::LruCache::new(prob),
                    protected: lru::LruCache::new(prot),
                    in_flight: in_flight::InFlight::new(),
                }),
                coalesced: AtomicU64::new(0),
                coalesced_tx,
            }),
        }
    }
    /// Total size of the underlying source in bytes.
    pub fn size(&self) -> u64 {
        self.inner.source.size()
    }
    /// The fixed block size in bytes.
    pub fn block_size(&self) -> usize {
        self.inner.block_size
    }
    /// Returns the bytes of block `idx` (short at EOF, empty past EOF), reading
    /// through the source on a miss. Concurrent misses for the same block share
    /// exactly one FETCH -- one registration, one detached reader -- unconditionally, including
    /// across supersession (see `get`'s own doc comment). Not necessarily one physical read:
    /// completing a short block takes as many as the source makes it take (batch 20
    /// (2026-07-31) -- the old wording predates multi-read completion and promised something this
    /// cache has not done since batch 13). A probationary hit counts as
    /// a re-reference and promotes the block.
    pub async fn block(&self, idx: u64) -> anyhow::Result<Block> {
        self.get(idx, true).await
    }
    /// Like `block`, but a probationary hit does NOT promote: prefetch warming
    /// must not count as a re-reference, or repeated fills would push
    /// never-viewed blocks into the protected segment and evict the
    /// interactive working set.
    pub(crate) async fn warm(&self, idx: u64) -> anyhow::Result<Block> {
        self.get(idx, false).await
    }
    /// **The one path by which this cache obtains bytes for a block** (batch 14 (2026-07-30)) --
    /// hit, fresh fetch, or completion of a short one, all the same registration and the same
    /// detached publisher. Batch 12 gave the completion its own hand-rolled read path
    /// (the deleted `complete_short_block`, a loop in the REQUESTER's own future), and batch 14's
    /// review found
    /// three defects in it that the fetch path had never had, because the fetch path solves them
    /// structurally rather than by care:
    ///
    /// - **A committed read survived cancellation only on one of the two paths.** A refill's own
    ///   `ticket.read(..).await` sat in the requester's frame, so aborting the requester discarded a
    ///   read the source may already have started (`source.rs`: past `read` -- SPENDING the ticket,
    ///   not `admit` resolving -- a caller must treat the read as committed, and for `PreadSource`
    ///   the permit really does ride inside `spawn_blocking` where no dropped future can call it
    ///   back; batch 23 (2026-08-01) fixes both halves of that sentence). Two aborted refills
    ///   held both permits of a bounded source, blocked a third request, and cost three identical
    ///   physical reads -- the exact duplication `docs/block_cache.md`'s coalescing section promises
    ///   is impossible.
    /// - **Completions did not coalesce.** `in_flight` is keyed by block index and a completion is
    ///   about a block index, so there was never a reason for it to be outside; N callers finding
    ///   the same short block each ran their own refill.
    /// - **Two writers, one monotone and one not.** `FetchCompletion::publish` put its snapshot
    ///   directly, so a legal concurrent prefix fetch could roll a refilled entry BACKWARD (block 0
    ///   extended to `ABCD`, then replaced with `A`). Now every write to an entry -- fetch,
    ///   completion, abandoned partial -- goes through `absorb`.
    ///
    /// The requester's side is what is left: find it resolved and take it, or join/register and
    /// await one `Block`.
    async fn get(&self, idx: u64, promote: bool) -> anyhow::Result<Block> {
        // see `touched`'s own derivation inside the critical section below: declared out here only
        // so the waiter path can read it (always assigned there before any use).
        #[allow(unused_assignments)]
        let mut touched = false;
        let mut ticket = {
            let mut st = self.inner.lock_state();
            // consumer-truthful promotion (see the module doc) extends to
            // protected recency, not just the promotion decision: a
            // non-promoting caller (warm(): prefetch, the background
            // index scan) touching an already-protected block is still
            // machinery, not a real re-reference, so it must peek rather
            // than refresh the block to most-recently-used.
            let in_protected = if promote {
                st.protected.get(&idx).is_some()
            } else {
                st.protected.peek(&idx).is_some()
            };
            // **Did this request already spend its one consumer touch?** (batch 15 (2026-07-30),
            // finding #4.) Whatever the lookup finds -- finished block, unfinished progress, or the
            // marker saying a fetch holds it -- the promotion/recency effect of THIS request has
            // now happened. Batch 14 let an unfinished hit fall through to the waiter path, which
            // counted the same request's single touch a SECOND time after publication: a joiner
            // that should have left protected order `[1, 0]` made it `[0, 1]`, pushing a block the
            // consumer touched once ahead of one it had genuinely re-referenced.
            touched = in_protected || st.probation.peek(&idx).is_some();
            let hit = if in_protected {
                // already protected: the `get`/`peek` above applied this caller's own recency
                // effect (or deliberately did not, for `warm()`), so only read it back out here.
                st.protected.peek(&idx).and_then(Entry::resolved).cloned()
            } else if promote {
                Self::promote_entry(&mut st, idx)
            } else {
                st.probation.get(&idx).and_then(Entry::resolved).cloned()
            };
            // **A hit is only an answer if it is RESOLVED.** An entry can hold unfinished progress
            // -- an abandoned fetch hands back the bytes it had rather than throwing the reads
            // away -- and handing that to a consumer is precisely the hole batch 12 exists to
            // close. The touch above still counted; what it did not do is answer the question.
            match hit {
                Some(b) => {
                    return Ok(b);
                }
                None => {
                    if let Some(ticket) = st.in_flight.join(idx, &self.inner) {
                        // this call found the block already being fetched: it is about to become
                        // a coalescing WAITER (see `coalesced`'s own doc comment on `BlockCache`
                        // for why this positive signal exists at all). Fired here, still under
                        // `st`'s own lock, so concurrent discoveries -- on this block or any
                        // other -- can never race this increment against each other. The FIRST
                        // requester (the one that registers just below) never fires this -- it
                        // is not "finding" an already-in-flight fetch, it is starting one.
                        let n = self.inner.coalesced.fetch_add(1, Ordering::Relaxed) + 1;
                        Self::publish_high_water_mark(&self.inner.coalesced_tx, n);
                        ticket
                    } else {
                        // **check the accumulator out** (batch 15 (2026-07-30), finding #3): the
                        // fetch takes the cache's own unfinished bytes rather than copying them,
                        // and the slot records that it has them. Both happen under this lock,
                        // together with the registration, so `Entry::CheckedOut` can never be
                        // observed without a live fetch to join.
                        let acc = Inner::check_out(&mut st, idx);
                        let (ticket, completion, waiterless) =
                            st.in_flight.register(idx, &self.inner, acc);
                        // spawned while still under `st`'s own lock (spawning itself has no
                        // `.await` point, so this holds a std Mutex no longer than any other
                        // critical section here): registering and starting the fetch happen
                        // atomically, so no racing caller can ever observe one without the other.
                        // `waiterless` -- minted by `register` itself, not subscribed later inside
                        // `fetch` -- is what keeps the lost-wakeup window closed; see `register`'s
                        // own doc comment.
                        //
                        // `partial` seeds the fetch: an unfinished entry is resumed from where it
                        // stopped, never re-read from the block's own start.
                        tokio::spawn(Self::fetch(self.inner.clone(), idx, completion, waiterless));
                        ticket
                    }
                }
            }
        };
        loop {
            let published = ticket.rx.borrow().clone();
            if let Some(res) = published {
                return match res {
                    Ok(block) => {
                        // only a caller that JOINED an already in-flight fetch -- a
                        // genuine second-or-later toucher while the first read was still
                        // resolving -- counts as a re-reference. The initiator's own
                        // touch is what LANDS the block in probation in the first place
                        // (`FetchCompletion::publish`'s own write), so it must
                        // not ALSO count as the second, promoting touch, or every block
                        // would promote on its very first touch and scan resistance
                        // would be gone.
                        if promote && matches!(ticket.arrival, Arrival::Joiner) && !touched {
                            // a promoting caller that coalesced with an
                            // in-flight (often prefetch) read must leave
                            // the same state as arriving after the fill:
                            // the interactive touch counts toward
                            // promotion.
                            let mut st = self.inner.lock_state();
                            // and it returns what the cache HOLDS, not this fetch's own published
                            // value: `publish` merges through `absorb`, so the entry is the
                            // authoritative one whenever both exist. Both lookups are
                            // recency-affecting on purpose -- this IS a real consumer's promoting
                            // touch, which is the whole point of the branch.
                            if let Some(held) = Self::promote_entry(&mut st, idx) {
                                return Ok(held);
                            }
                            if let Some(held) =
                                st.protected.get(&idx).and_then(Entry::resolved).cloned()
                            {
                                return Ok(held);
                            }
                            // evicted between publish and wake-up: this
                            // is still fill + display — the same two
                            // touches that promote in every other
                            // interleaving — so the outcome must not
                            // depend on churn timing.
                            Self::insert_protected(&mut st, idx, Entry::Resolved(block.clone()));
                            return Ok(block);
                        }
                        Ok(block)
                    }
                    Err(e) => Err(anyhow::Error::new(e)),
                };
            }
            // `ticket` itself holds `slot: Arc<FetchSlot>`, which owns the outcome `Sender` --
            // for as long as THIS call has not returned, `ticket` is still alive in this very
            // frame, so the sender cannot have dropped and this `.changed()` cannot observe a
            // close. Named precisely for that reason, not "FetchCompletion always publishes
            // before ending": if the publish invariant itself ever broke -- e.g. a future
            // ending that neither published nor dropped -- the observable would be this call
            // hanging forever, never this `.expect` firing.
            ticket.rx.changed().await.expect(
                "a live WaiterTicket's own Arc<FetchSlot> keeps the outcome Sender alive -- this \
                 receiver cannot observe a close while `ticket` is still held in this frame; if \
                 this ever fires, something dropped `ticket` early or the Arc accounting itself \
                 has a hole",
            );
        }
    }
    /// The fetch task itself (restructure R2, 2026-07-27, structural-cache.md §3.4; extended to
    /// COMPLETION by batch 14 (2026-07-30)): owns the only `FetchCompletion` for block `idx`,
    /// spawned by `get`'s own miss path and owned by nobody else -- see `get`'s own doc comment for
    /// why that is the whole point.
    ///
    /// **It resolves the block before publishing anything.** A source may legally answer short
    /// (`source.rs`: "up to `len`"), and a block-indexed cache memoizing that answer turns a
    /// transient shortfall into a permanent hole nothing downstream can tell from the end of the
    /// file. So this loops: read from wherever the bytes stop, until the block is full, or the
    /// source answers EMPTY (certifying that its real data ends exactly there --
    /// `Block::ends_data`), or the file's own claimed size ends inside the block. Bounded by the
    /// hole's own width, since every pass adds at least one byte.
    ///
    /// Three properties come from the loop living HERE rather than in the requester (batch 14):
    /// - **A committed read is never wasted.** This task is not any requester's future, so aborting
    ///   a search cannot discard a read the source has already committed to, strand its permit, or
    ///   force a duplicate.
    /// - **Completions coalesce** exactly as fresh fetches always have: one registration per block.
    /// - **It is linear, not quadratic.** One `Acc` accumulates across passes -- the source's own
    ///   buffer while a single read is all there is, promoted to one growing `BytesMut` when a
    ///   second arrives -- and is frozen once. Batch 13 rebuilt the whole prefix every pass to republish it for durability --
    ///   `block_size` reads over a 1 MiB block copied on the order of 512 GiB. Durability now comes
    ///   from the abandon path instead: nothing is republished while the fetch is alive, and if it
    ///   dies with nobody waiting, it hands over what it has.
    ///
    /// `waiterless` is a PARAMETER, not something this task subscribes to itself: it is minted by
    /// `in_flight::InFlight::register`, under the cache lock, before this task is even spawned --
    /// see that method's own doc comment for why subscribing here, inside the task's own first
    /// poll, is a genuine lost-wakeup hazard rather than a stylistic preference. This does NOT
    /// reopen unit D's reviewed "no wake-into-contention" property: that property is about
    /// `WaiterTicket::drop`'s own SEND, which still happens strictly after its lock guard drops
    /// (unchanged) -- moving where the RECEIVER is created changes nothing about where the
    /// notification is published, only about when this task starts listening for it.
    ///
    /// Two phases with opposite cancellation semantics -- the root-cause boundary every prior
    /// defect in this file (the pass-8 pread stacking, finding #11, finding #4, finding #9) turned
    /// out to share, now a value (`ReadTicket`, restructure R1) with a `select!` built around it --
    /// and, since batch 14, one pair of phases PER PASS rather than once for the whole fetch:
    /// - **Phase 1, before commitment.** A `biased` select races `waiterless.changed()` against the
    ///   PINNED `admit` future. Losing the abandon race (a joiner arrives and re-joins before this
    ///   task's own re-check runs) is free and resumable: `StillWanted` keeps the SAME pinned
    ///   `admit`, so the fetch neither restarts nor loses its FIFO position in the source's own
    ///   queue -- only phase 1 ever races anything, because retrying phase 1 costs nothing and
    ///   retrying phase 2 would cost a duplicate read. A multi-pass completion therefore stays
    ///   abandonable at every pass boundary, which is the right granularity: a dribbling source
    ///   cannot pin this task to a block nobody wants anymore.
    /// - **Phase 2, after commitment.** The ticket is spent; no select exists; `completion` sits in
    ///   this frame across the read. From here that read WILL happen and its bytes WILL be kept --
    ///   published to whoever is still interested, or absorbed into the cache on the way out.
    async fn fetch(
        inner: Arc<Inner>,
        idx: u64,
        mut completion: FetchCompletion,
        mut waiterless: tokio::sync::watch::Receiver<u64>,
    ) {
        let bs = inner.block_size;
        let block_start = idx * bs as u64;
        // the accumulator lives on `completion` (batch 15 (2026-07-30)): it arrived from the cache
        // -- possibly already holding an earlier generation's bytes, at no copy -- and every ending
        // writes it back. Nothing here has to remember to hand it over.
        let mut ends_data = false;
        // a seeded resume never re-reads what the abandoned generation already got.
        let seeded = !completion.acc.is_empty();
        let mut passes = 0u32;
        loop {
            // `seeded || passes > 0`: with neither, `acc` is empty because nothing has been read
            // yet, which `is_resolved` would otherwise read as the wholly-empty past-EOF block.
            if (seeded || passes > 0) && inner.is_resolved(idx, completion.acc.len(), ends_data) {
                break;
            }
            let at = block_start + completion.acc.len() as u64;
            let want = bs - completion.acc.len();
            let admit = inner.source.admit();
            tokio::pin!(admit);
            let ticket = loop {
                tokio::select! {
                    biased;
                    _ = waiterless.changed() => match completion.try_abandon() {
                        // nobody is waiting anymore, so there is nothing to publish -- and
                        // nothing to hand over here either: `try_abandon` stored the accumulator
                        // inside the same critical section that deregistered (batch 15
                        // (2026-07-30), finding #2), so a replacement registering the instant this
                        // returns already finds the bytes to resume from.
                        Abandon::Done => return,
                        Abandon::StillWanted(c) => completion = c,
                    },
                    t = &mut admit => break t,
                }
            };
            // **THE COMMIT POINT** (batch 16 (2026-07-31), finding #2). Admission has resolved;
            // the ticket is not yet spent, and spending it is the moment the read stops being
            // cancellable (`source.rs`: past `read` the caller must treat the read as committed --
            // a rule for the caller, not something the trait extracts from every source). So the decision is re-derived HERE, under the
            // lock, rather than left to the phase-1 select's own race:
            // `WaiterTicket::drop` updates the authoritative count under the lock but sends its
            // notification after releasing it, and admission resolving inside that gap used to send
            // this task straight into a physical read with zero waiters -- a staged probe recorded
            // exactly one. `try_abandon` asks the count itself, not the notification, so this is an
            // answer rather than a race; and an UNSPENT ticket dropped here costs nothing, which is
            // the property `ReadTicket` documents and phase 1 already relies on.
            match completion.try_abandon() {
                Abandon::Done => {
                    drop(ticket);
                    return;
                }
                Abandon::StillWanted(c) => completion = c,
            }
            let more = match ticket.read(at, want).await {
                Ok(more) => more,
                Err(e) => {
                    completion.publish(Err(SharedError(Arc::new(
                        e.context(format!("read block {idx} at {at}")),
                    ))));
                    return;
                }
            };
            if more.is_empty() {
                // the source itself says there is nothing at `at` (`source.rs`: "an empty buffer
                // when `offset >= size`"). On the very first read of an untouched block that means
                // the whole block is past EOF; anywhere else it certifies that the real data ends
                // exactly where these bytes do -- and that certificate is minted HERE, for THESE
                // bytes, which is the only place in this crate that can mint one.
                ends_data = !completion.acc.is_empty();
                break;
            }
            // clamped: a source answering with MORE than the `want` it was asked for is
            // non-conforming, and letting a block grow past `block_size` would break the
            // block-local offset arithmetic every consumer does (`meter::classify`'s own
            // `at - block_start`).
            completion.acc.push(more.slice(..more.len().min(want)));
            passes += 1;
            // cooperative: a source that answers from memory completes every `.await` in this loop
            // without ever yielding, so a dribbling one would otherwise run `block_size` passes
            // inside a single poll and starve every other task on this worker.
            if passes.is_multiple_of(64) {
                tokio::task::yield_now().await;
            }
        }
        let bytes = std::mem::replace(&mut completion.acc, Acc::Empty).into_bytes();
        completion.publish(Ok(Block { bytes, ends_data }));
    }
    fn insert_protected(st: &mut State, idx: u64, block: Entry) {
        if let Some((k, v)) = st.protected.push(idx, block)
            && k != idx
        {
            st.probation.put(k, v);
        }
    }
    /// Moves a probationary entry into the protected segment and returns it;
    /// `None` when the block is not probationary.
    fn promote_entry(st: &mut State, idx: u64) -> Option<Block> {
        let e = st.probation.pop(&idx)?;
        let resolved = e.resolved().cloned();
        // second touch promotes -- whatever the slot holds. An unfinished entry promotes too: the
        // consumer really did reference this block twice, and which generation of the fetch happens
        // to be in flight is machinery, not the consumer's doing.
        Self::insert_protected(st, idx, e);
        resolved
    }
    /// `send_if_modified`, monotonic high-water-mark publish, no-op with no receivers --
    /// the identical contract `source.rs`'s own `publish_high_water_mark` documents at
    /// length (also reimplemented locally in `prefetch.rs`'s own `FillEventGuard::publish`);
    /// kept local here too rather than imported, per the same P7-C layering principle:
    /// this is the cache's own coalescing signal, not the block-source's.
    fn publish_high_water_mark(sender: &tokio::sync::watch::Sender<u64>, value: u64) {
        sender.send_if_modified(|current| {
            if value > *current {
                *current = value;
                true
            } else {
                false
            }
        });
    }
    /// Number of in-flight registrations; leak detection in tests.
    #[cfg(test)]
    pub(crate) fn in_flight_len(&self) -> usize {
        self.inner.lock_state().in_flight.len()
    }
    /// A receiver a test can `wait_for`/`changed()` on to observe how many `get()` calls
    /// have coalesced onto an already in-flight fetch -- see `coalesced`'s own doc comment
    /// on `BlockCache` for why this exists (a positive alternative to inferring coalescing
    /// from `in_flight_len` staying put, which cannot tell "no waiters" apart from "many").
    #[cfg(test)]
    pub(crate) fn coalesced_events(&self) -> tokio::sync::watch::Receiver<u64> {
        self.inner.coalesced_tx.subscribe()
    }
    /// Test-only view of the protected segment's population.
    #[cfg(test)]
    pub(crate) fn protected_len(&self) -> usize {
        self.inner.lock_state().protected.len()
    }
    /// Test-only count of every block this cache remembers ANYTHING about (batch 13 (2026-07-29),
    /// finding #5). The number exists to be compared against the two LRUs' own capacity: batch 12
    /// kept refill findings in side maps keyed by block index, which grew with file size behind
    /// those bounded caches, and the structural answer is that there is now nothing to count except
    /// the entries themselves -- `refill_state_never_outgrows_the_bounded_segments` is what holds
    /// that claim to a number rather than a comment.
    #[cfg(test)]
    pub(crate) fn tracked_blocks(&self) -> usize {
        let st = self.inner.lock_state();
        st.probation.len() + st.protected.len()
    }
    /// Test-only view of the protected segment's key order, most-recently-used
    /// first (`lru::LruCache::iter`'s order).
    #[cfg(test)]
    pub(crate) fn protected_keys(&self) -> Vec<u64> {
        self.inner
            .lock_state()
            .protected
            .iter()
            .map(|(&k, _)| k)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{MockSource, wait_for_count};
    fn cache(
        data: &'static [u8],
        block_size: usize,
        capacity_bytes: usize,
    ) -> (Arc<MockSource>, BlockCache) {
        let src = Arc::new(MockSource::new(Bytes::from_static(data)));
        let c = BlockCache::new(src.clone(), block_size, capacity_bytes);
        (src, c)
    }
    // found in PR #44 round 17 (a codex P2, sweep): several tests below used to spawn a
    // background fetcher and then sleep a guessed duration (5-10ms), hoping that was enough
    // real time for it to have registered itself in `in_flight` before the test's own next
    // step -- a correct implementation can fail that race under a loaded/parallel executor,
    // the same "tests prove events, not scheduler timing" rule this workspace already states
    // elsewhere (AGENTS.md; round 16's identical fix to `prefetch.rs`). `in_flight_len()` is
    // already a real, synchronous fact (`BlockCache`'s own `std::sync::Mutex`-guarded map, not
    // an async signal) -- polled here via `yield_now` (which costs no real wall-clock time
    // itself, only a cooperative reschedule) rather than inferred from elapsed time, bounded by
    // a generous 5s timeout purely as a hang backstop, matching this crate's own `wait_for`
    // idiom (ress-core/src/status.rs).
    async fn wait_for_in_flight_len_at_least(c: &BlockCache, n: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while c.in_flight_len() < n {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "in_flight_len never reached {n} (stuck at {})",
                c.in_flight_len()
            )
        });
    }
    /// **Waits for a SPENT ticket, not merely a registration** (batch 19 (2026-07-31)).
    /// `wait_for_in_flight_len_at_least` proves only that `get` registered the fetch -- which it
    /// does under the lock, BEFORE the detached fetch task has been polled even once (this file's
    /// own `probe_p1_requester_dropped_before_the_fetch_task_first_poll` constructs exactly that
    /// state). A test that aborts on that signal alone may therefore be exercising waiterless
    /// ABANDONMENT -- the fetch dying before it ever read -- while claiming to exercise a
    /// cancellation of a committed read. The two have opposite expected outcomes, so the test
    /// passing tells you nothing about which one ran.
    ///
    /// `MockSource::started_events` publishes per read that has actually ENTERED (a ticket spent,
    /// before the gate's own park check), which is the fact those tests need: past it the read is
    /// committed and no cancellation can call it back.
    async fn wait_for_read_started(src: &MockSource, n: u64) {
        let mut started = src.started_events();
        wait_for_count(&mut started, |c| c >= n).await;
    }
    // the `at_most` sibling (batch 4 (2026-07-24), finding #11): waits for a registration to
    // CLEAR (e.g. after a waiterless abandon, or an abnormal `FetchCompletion::drop` cleanup)
    // rather than appear -- same polled-via-`yield_now`, bounded-by-a-diagnostic-ceiling idiom
    // as its sibling above, not a sleep.
    async fn wait_for_in_flight_len_at_most(c: &BlockCache, n: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while c.in_flight_len() > n {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "in_flight_len never settled to {n} (stuck at {})",
                c.in_flight_len()
            )
        });
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
        let src = Arc::new(MockSource::new(Bytes::from_static(b"0123456789")).with_gate());
        src.arm_gate();
        let c = Arc::new(BlockCache::new(src.clone(), 4, 64));
        let mut coalesced = c.coalesced_events();
        let mut joins = Vec::new();
        for _ in 0..8 {
            let c = c.clone();
            joins.push(tokio::spawn(async move { c.block(0).await.unwrap() }));
        }
        // exactly one of the 8 becomes the fetcher (parked on the gate below); the other 7
        // must have genuinely JOINED as coalescing waiters -- not merely been spawned -- before
        // the gate ever opens, or a not-yet-scheduled 8th task could slip in after release and
        // hit a freshly-cached block instead of ever exercising concurrent-miss coalescing.
        wait_for_count(&mut coalesced, |n| n >= 7).await;
        src.open_gate();
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
        let src = Arc::new(MockSource::new(Bytes::from_static(b"0123456789abcdef")).with_gate());
        src.arm_gate();
        let c = Arc::new(BlockCache::new(src.clone(), 4, 16));
        let warm = tokio::spawn({
            let c = c.clone();
            async move { c.warm(0).await }
        });
        // waits for the fill to have genuinely REGISTERED (not merely spawned) as the fetcher --
        // and, since nothing can progress past the gate, be genuinely PARKED there -- before the
        // foreground read below; see `wait_for_in_flight_len_at_least`'s own doc comment.
        wait_for_in_flight_len_at_least(&c, 1).await;
        let mut coalesced = c.coalesced_events();
        let interactive = tokio::spawn({
            let c = c.clone();
            async move { c.block(0).await }
        });
        // proves the interactive read genuinely JOINED the in-flight fill as a coalescing
        // waiter -- not a maybe-already-done cache hit -- before the gate ever opens.
        wait_for_count(&mut coalesced, |n| n >= 1).await;
        src.open_gate();
        let b = interactive.await.unwrap().unwrap();
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
        let src = Arc::new(MockSource::new(Bytes::from_static(b"0123456789abcdef")).with_gate());
        src.arm_gate();
        let c = Arc::new(BlockCache::new(src.clone(), 4, 16));
        let w0 = tokio::spawn({
            let c = c.clone();
            async move { c.warm(0).await }
        });
        let w1 = tokio::spawn({
            let c = c.clone();
            async move { c.warm(1).await }
        });
        // waits for BOTH fills to have registered -- and, since nothing can progress past the
        // gate, be genuinely PARKED there -- before the foreground read below; see
        // `wait_for_in_flight_len_at_least`'s own doc comment, and its sibling use just above.
        wait_for_in_flight_len_at_least(&c, 2).await;
        let mut coalesced = c.coalesced_events();
        let interactive = tokio::spawn({
            let c = c.clone();
            async move { c.block(0).await }
        });
        // proves the interactive read genuinely JOINED w0's in-flight fill as a coalescing
        // waiter -- not a maybe-already-done cache hit -- before the gate ever opens.
        wait_for_count(&mut coalesced, |n| n >= 1).await;
        src.open_gate();
        let b = interactive.await.unwrap().unwrap();
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
            async fn admit(&self) -> crate::source::ReadTicket {
                crate::source::ReadTicket::from_fn(|_offset, _len| {
                    Box::pin(async move { Err(anyhow::anyhow!("boom")) })
                })
            }
        }
        let c = BlockCache::new(Arc::new(Failing), 4, 16);
        let err = c.block(0).await.unwrap_err();
        assert!(format!("{err:#}").contains("boom"));
    }
    #[tokio::test]
    async fn aborting_the_first_requester_does_not_wedge_the_block() {
        // aborting the requester that triggered a fetch must leave the block fetchable.
        //
        // REVISED, batch 4 (2026-07-24), finding #11: under the detached-publisher restructure
        // (this file's own module-level doc comment on `get`), the mechanism changed even though
        // the black-box claim did not. Pre-restructure, aborting the fetcher tore down the
        // physical read WITH it, so the second caller below only stayed unwedged because a
        // cleanup route (`InFlightGuard::drop`, or a coalesced waiter's own self-heal-on-closed-
        // channel) cleared the stale registration and let the second caller become a fresh
        // fetcher. Post-restructure, the physical read is a DETACHED task nobody but itself owns
        // -- aborting the first requester's own future only cancels that requester's OWN wait, not
        // the fetch it spawned, which keeps running and stays registered. The second caller below
        // instead COALESCES onto that still-live detached fetch, gets its bytes once the gate
        // opens, and never needed a wedge-avoiding cleanup route at all. Still deliberately
        // black-box (no `in_flight_len` assertion here): `aborting_the_only_requester_leaves_the_
        // detached_fetch_registered`, below, is the one that pins the registration's own fate
        // directly.
        //
        // found in PR #44 pass 7's structural pass (codex P2, a 3rd re-review, the shared root of
        // 3 findings at once, this one an audit find rather than one of the 3 codex flagged
        // directly): a gate, armed immediately -- not a fixed latency, which can complete on its
        // own before `first.abort()` below, independent of this test's own scheduling, letting
        // the SECOND `c.block(0)` call below find the block already cached (a hit, not the
        // coalesce-onto-a-live-fetch this test exists to prove) rather than genuinely racing an
        // abort against a still-in-flight read. Opened again right after the abort: this test's
        // own claim is bounded by the 2s timeout on the second call, not a positive event proof,
        // so there is no reason to keep anything parked past that point, and the second call
        // needs the gate open to complete at all.
        let src = Arc::new(MockSource::new(Bytes::from_static(b"0123456789")).with_gate());
        src.arm_gate();
        let c = Arc::new(BlockCache::new(src.clone(), 4, 64));
        let first = tokio::spawn({
            let c = c.clone();
            async move { c.block(0).await }
        });
        // waits for the fetch to have genuinely STARTED READING before aborting it -- batch 19
        // (2026-07-31). Registration alone is not that fact: `get` registers under the lock before
        // the detached fetch task is polled at all, so aborting on it can exercise waiterless
        // abandonment instead of the mid-read cancel this test is about, and the two have opposite
        // outcomes. See `wait_for_read_started`'s own doc comment.
        wait_for_read_started(&src, 1).await;
        first.abort();
        src.open_gate();
        let bytes = tokio::time::timeout(std::time::Duration::from_secs(2), c.block(0))
            .await
            .expect("second caller wedged after the first requester was aborted")
            .unwrap();
        assert_eq!(&bytes[..], b"0123");
    }
    #[tokio::test]
    async fn aborting_the_only_requester_leaves_the_detached_fetch_registered() {
        // REVERSED, batch 4 (2026-07-24), finding #11 (this test was RENAMED from the deleted
        // `aborted_fetcher_does_not_leak_its_registration`, which asserted the OPPOSITE outcome -- `in_flight_len() ==
        // 0` -- true under the pre-restructure design, where the fetcher's own future WAS the
        // physical read, so aborting it tore the read down and its `InFlightGuard` cleaned up the
        // registration). Under the detached-publisher restructure (`get`'s own module-level doc
        // comment, above), the registration belongs to the detached fetch, not to whichever
        // requester happened to trigger it -- so aborting the ONLY requester must leave it
        // INTACT, not clean it up: the fetch is still genuinely in flight (parked on the gate
        // below) and will publish into the cache once it completes, exactly the coalescing target
        // `aborting_the_first_requester_does_not_wedge_the_block` (above) relies on. The
        // registration only ever disappears via `FetchCompletion`'s own three endings (restructure
        // R2, 2026-07-27): the normal publish path; a waiterless abandon (unreachable for THIS
        // fetch specifically, since it is already past admission by the time the abort below
        // fires -- see `k_distinct_block_fetches_die_unstarted_once_every_requester_is_gone` for
        // that path); or an abnormal teardown of the DETACHED task itself (`Drop`'s own
        // involuntary arm -- see `a_panicking_detached_fetch_does_not_leak_its_registration`,
        // below). Never merely because a requester walked away.
        //
        // RED-verified against the fix: this assertion (`in_flight_len() == 1`) fails against the
        // pre-restructure code with `left: 0, right: 1` -- the exact mirror of this test's own
        // pre-flip failure against the restructured code (`left: 1, right: 0`), confirming the
        // two versions genuinely discriminate the two designs rather than both happening to pass.
        //
        // found in PR #44 pass 7's structural pass (codex P2, a 3rd re-review, an audit find):
        // a gate, armed immediately -- not a fixed latency, which can complete on its own before
        // `first.abort()` below fires, independent of this test's own scheduling. Never opened:
        // nothing needs the fetch to actually complete here, only that the abort leaves its
        // registration exactly as it found it.
        let src = Arc::new(MockSource::new(Bytes::from_static(b"0123456789")).with_gate());
        src.arm_gate();
        let c = Arc::new(BlockCache::new(src.clone(), 4, 64));
        let first = tokio::spawn({
            let c = c.clone();
            async move { c.block(0).await }
        });
        // batch 19 (2026-07-31): a SPENT-TICKET fact, not merely a registration -- see
        // `wait_for_read_started`'s own doc comment. This test's claim is about what an abort
        // leaves behind once the read is committed, and registration alone happens before the
        // fetch task has been polled at all.
        wait_for_read_started(&src, 1).await;
        assert_eq!(c.in_flight_len(), 1, "fetch should be registered");
        first.abort();
        let _ = first.await;
        assert_eq!(
            c.in_flight_len(),
            1,
            "the detached fetch's own registration must survive its first requester's abort"
        );
    }
    #[tokio::test]
    async fn superseded_requester_does_not_stack_a_second_physical_read() {
        // GREEN, batch 4 (2026-07-24), finding #11: supersedes the RED fixture that used to live
        // here (the deleted `red_superseded_requester_stacks_a_second_physical_read`, which pinned
        // the opposite, TODAY-broken outcome, per its own doc
        // comment). Confirmed against the pre-restructure code before deleting: that RED version
        // passed there (started_events reached 2), and this GREEN version's own `wait_for_count`
        // below times out there instead of ever reaching 1 a second time -- the two are genuine
        // mirror images of each other, not merely differently-worded.
        let src = Arc::new(MockSource::new(Bytes::from_static(b"0123456789")).with_gate());
        src.arm_gate();
        let c = Arc::new(BlockCache::new(src.clone(), 4, 64));
        let mut started = src.started_events();
        let mut coalesced = c.coalesced_events();
        let first = tokio::spawn({
            let c = c.clone();
            async move { c.block(0).await }
        });
        // the first requester is genuinely, provably parked mid-read (past the gate's own
        // started-event, before returning) before it gets superseded below.
        wait_for_count(&mut started, |n| n >= 1).await;
        first.abort();
        let _ = first.await;
        // supersession: a replacement request for the SAME block, issued while the first read
        // is still genuinely physically in flight (the detached fetch the first requester
        // spawned survives its own abort -- `get`'s own module-level doc comment).
        let second = tokio::spawn({
            let c = c.clone();
            async move { c.block(0).await }
        });
        // positive proof the replacement genuinely JOINED the still-live detached fetch as a
        // coalescing waiter -- not a maybe-already-done cache hit -- before the gate ever opens.
        wait_for_count(&mut coalesced, |n| n >= 1).await;
        src.open_gate();
        let bytes = second.await.unwrap().unwrap();
        assert_eq!(&bytes[..], b"0123");
        assert_eq!(
            *started.borrow(),
            1,
            "the replacement must coalesce onto the SAME physical read, not start a second one"
        );
    }
    #[tokio::test]
    async fn repeated_supersession_never_stacks_more_than_one_read_per_block() {
        // "what must hold" #2 (batch 4, finding #11): N supersede-and-replace rounds against one
        // gated block must never exceed one in-flight read for that block, and permits (here,
        // the cache's own `in_flight` registration -- `BoundedBlockingReads`'s own permit-level
        // proof lives in `source.rs`, a layer below the cache and already covered there) return
        // to baseline once the fetch is allowed to finish.
        let src = Arc::new(MockSource::new(Bytes::from_static(b"0123456789")).with_gate());
        let c = Arc::new(BlockCache::new(src.clone(), 4, 64));
        src.arm_gate();
        let started = src.started_events();
        let mut coalesced = c.coalesced_events();
        // round 0 STARTS the fetch. Waiting for the read to have genuinely entered is what makes
        // every later round a supersession of something real (batch 20 (2026-07-31)).
        let first = tokio::spawn({
            let c = c.clone();
            async move { c.block(0).await }
        });
        wait_for_read_started(&src, 1).await;
        first.abort();
        let _ = first.await;
        // rounds 1..=4 are the REPLACEMENTS, and each must be shown to have JOINED the still-live
        // fetch before it is aborted. `wait_for_in_flight_len_at_least` could not show that: after
        // round 0 the registration is already there, so it returned immediately and the abort could
        // land before the replacement was ever polled -- the reviewer demonstrated the gap by
        // swapping these requests for `pending()`, which passed. `coalesced_events` is the positive
        // fact instead (a caller that FOUND an in-flight fetch and became a waiter on it), counted
        // cumulatively so each round proves its own join rather than re-observing an earlier one.
        for round in 1..=4u64 {
            let req = tokio::spawn({
                let c = c.clone();
                async move { c.block(0).await }
            });
            wait_for_count(&mut coalesced, |n| n >= round).await;
            assert_eq!(
                *started.borrow(),
                1,
                "round {round}: still exactly the one physical read across every supersession"
            );
            req.abort();
            let _ = req.await;
        }
        assert_eq!(
            *started.borrow(),
            1,
            "5 supersession rounds against the same gated block must still share one physical read"
        );
        src.open_gate();
        let bytes = tokio::time::timeout(std::time::Duration::from_secs(2), c.block(0))
            .await
            .expect("settling after 5 rounds of supersession must not wedge the block")
            .unwrap();
        assert_eq!(&bytes[..], b"0123");
        assert_eq!(
            *started.borrow(),
            1,
            "still exactly one physical read after everything settles"
        );
        assert_eq!(
            c.in_flight_len(),
            0,
            "the registration must return to baseline once the fetch has published"
        );
    }
    #[tokio::test]
    async fn abandoned_requester_read_still_completes_and_populates_the_cache() {
        // "what must hold" #3 (batch 4, finding #11): a superseded scan's abandoned read is no
        // longer wasted work -- it completes and lands in the cache, so a LATER, independent
        // request for that block is a hit (no new physical read), not merely "does not wedge"
        // (`aborting_the_first_requester_does_not_wedge_the_block`, above, already covers that
        // weaker claim).
        let src = Arc::new(MockSource::new(Bytes::from_static(b"0123456789")).with_gate());
        src.arm_gate();
        let c = Arc::new(BlockCache::new(src.clone(), 4, 64));
        let mut started = src.started_events();
        let req = tokio::spawn({
            let c = c.clone();
            async move { c.block(0).await }
        });
        wait_for_count(&mut started, |n| n >= 1).await;
        req.abort();
        let _ = req.await;
        // nobody is waiting on the block at all now; the detached fetch is still parked on the
        // gate, unaffected by the abort above.
        src.open_gate();
        // resolves correctly whether this call lands as a cache hit (the detached publish beat
        // it) or as a coalescing waiter on the still-finishing fetch (this call beat the
        // publish) -- coalescing is exactly what makes the distinction not matter here.
        let bytes = tokio::time::timeout(std::time::Duration::from_secs(2), c.block(0))
            .await
            .expect("a later request for the abandoned block must not wedge")
            .unwrap();
        assert_eq!(&bytes[..], b"0123");
        assert_eq!(
            *started.borrow(),
            1,
            "the later request must be served by the abandoned read, not a fresh one"
        );
    }
    #[tokio::test]
    async fn a_failed_detached_fetch_deregisters_so_a_later_retry_reads_again() {
        // "what must hold" #5 (batch 4, finding #11): errors are not cached (unchanged across
        // restructure R2 too -- `FetchCompletion::publish`'s own `Err` arm never calls
        // `probation.put`), and `in_flight` is cleared either way, so a later request after a
        // failure is a genuinely fresh physical read, never permanently poisoned by one bad
        // read. `read_errors_propagate_with_context` (above) already covers the error's own
        // chain; this test's own, narrower job is the RETRY the sweep's give_up/backoff
        // machinery depends on (search v1's own retry budget, `crate::search`).
        struct FailsOnce {
            failed_once: Arc<AtomicU64>,
            data: Bytes,
        }
        #[async_trait::async_trait]
        impl BlockSource for FailsOnce {
            fn size(&self) -> u64 {
                self.data.len() as u64
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let failed_once = self.failed_once.clone();
                let data = self.data.clone();
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        if failed_once.fetch_add(1, Ordering::Relaxed) == 0 {
                            return Err(anyhow::anyhow!("boom"));
                        }
                        let start = offset.min(data.len() as u64) as usize;
                        let end = offset.saturating_add(len as u64).min(data.len() as u64) as usize;
                        Ok(data.slice(start..end))
                    })
                })
            }
        }
        let src = Arc::new(FailsOnce {
            failed_once: Arc::new(AtomicU64::new(0)),
            data: Bytes::from_static(b"0123456789"),
        });
        let c = BlockCache::new(src, 4, 64);
        let err = c.block(0).await.unwrap_err();
        assert!(format!("{err:#}").contains("boom"));
        assert_eq!(
            c.in_flight_len(),
            0,
            "a failed fetch must deregister, not wedge the block"
        );
        let bytes = c.block(0).await.unwrap();
        assert_eq!(
            &bytes[..],
            b"0123",
            "the retry after a failure must issue a fresh read"
        );
    }
    #[tokio::test]
    async fn supersession_during_a_failing_read_still_delivers_the_error_and_retries_fresh() {
        // adopted from the fix-round-1 review (batch 4, finding #11): the one error-path shape
        // the original pass did not cover -- supersession racing a FAILING read, not a
        // succeeding one. Requester A registers and parks mid-read on a gated, always-failing
        // source; A is aborted; replacement B coalesces onto the SAME still-live detached fetch
        // (a positive `coalesced_events` wait, not inferred); the gate opens; B receives the
        // error with its chain intact; `in_flight` drains to 0; and a LATER `block(0)` call
        // issues a genuinely fresh physical read (errors are not cached, so the block is never
        // permanently poisoned by the first failure) -- exactly 2 physical reads total, never
        // stacked into more by the supersession itself.
        struct FailingGated {
            gate: tokio::sync::watch::Sender<bool>,
            reads: Arc<AtomicU64>,
        }
        #[async_trait::async_trait]
        impl BlockSource for FailingGated {
            fn size(&self) -> u64 {
                8
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let reads = self.reads.clone();
                let gate = self.gate.clone();
                crate::source::ReadTicket::from_fn(move |_offset, _len| {
                    Box::pin(async move {
                        reads.fetch_add(1, Ordering::Relaxed);
                        let mut rx = gate.subscribe();
                        // matches `crate::source::DIAGNOSTIC_CEILING`, the same shared bound
                        // `concurrent_waiters_all_observe_the_same_error`'s own `FailingSlow`
                        // (below) uses -- a diagnostic backstop, not a coordination oracle.
                        tokio::time::timeout(
                            crate::source::DIAGNOSTIC_CEILING,
                            rx.wait_for(|open| *open),
                        )
                        .await
                        .expect("FailingGated's own gate parked longer than the diagnostic ceiling")
                        .expect("the sender lives alongside this receiver, in the same test");
                        Err(anyhow::anyhow!("boom"))
                    })
                })
            }
        }
        let (gate_tx, _) = tokio::sync::watch::channel(false);
        let reads = Arc::new(AtomicU64::new(0));
        let src = Arc::new(FailingGated {
            gate: gate_tx.clone(),
            reads: reads.clone(),
        });
        let c = Arc::new(BlockCache::new(src.clone(), 4, 64));
        let a = tokio::spawn({
            let c = c.clone();
            async move { c.block(0).await }
        });
        // `reads` is incremented as the read ENTERS, so this is the same spent-ticket fact
        // `wait_for_read_started` waits for, published by this fixture's own counter rather than
        // by `MockSource`'s watch (batch 19 (2026-07-31)).
        wait_for_counter(&reads, 1).await;
        a.abort();
        let _ = a.await;
        // supersession: a replacement request for the SAME block, issued while the (still-live,
        // still gated) failing fetch A started is genuinely in flight.
        let mut coalesced = c.coalesced_events();
        let b = tokio::spawn({
            let c = c.clone();
            async move { c.block(0).await }
        });
        // positive proof B genuinely joined the still-live detached fetch as a coalescing
        // waiter -- not a fresh fetcher of its own -- before the gate ever opens.
        wait_for_count(&mut coalesced, |n| n >= 1).await;
        let _ = gate_tx.send(true);
        let err = b.await.unwrap().unwrap_err();
        assert!(
            format!("{err:#}").contains("boom"),
            "B must receive the error, chain intact"
        );
        assert_eq!(
            src.reads.load(Ordering::Relaxed),
            1,
            "the supersession itself must not have stacked a second physical read"
        );
        assert_eq!(
            c.in_flight_len(),
            0,
            "a failed fetch must deregister, not wedge the block, even across supersession"
        );
        // a later, independent request: errors are not cached, so this must be a genuinely
        // fresh physical read, not a hit reusing the first failure.
        let err2 = c.block(0).await.unwrap_err();
        assert!(format!("{err2:#}").contains("boom"));
        assert_eq!(
            src.reads.load(Ordering::Relaxed),
            2,
            "the later retry must issue a fresh read -- exactly 2 physical reads total"
        );
    }
    #[tokio::test]
    async fn a_panicking_detached_fetch_does_not_leak_its_registration() {
        // `FetchCompletion::drop`'s one and only reason to fire in normal operation (restructure
        // R2, 2026-07-27, superseding `InFlightGuard`'s narrower pre-R2 role): the detached fetch
        // cannot be aborted by a requester walking away anymore (nobody but the fetch itself
        // owns its own future), but an ABNORMAL teardown -- a panic mid-read, or the whole
        // runtime shutting down with it still in flight -- must still leave `in_flight` clean,
        // not leaked forever, via `Drop`'s own involuntary-teardown arm. `MockSource`'s own gate
        // safety-bound panic (source.rs, `arm_gate`'s own doc comment) is a ready-made "this
        // task's own future was torn down by a panic mid-await" -- reused here rather than a
        // bespoke panicking `BlockSource`.
        //
        // the requester itself is aborted right after registering, before the panic ever fires:
        // this test's own job is the registration's cleanup in isolation (black-box, no
        // assertion on what a live waiter would have observed) -- what a STILL-LIVE waiter
        // receives from that same `Drop` arm (an error, not a hang or a respawn loop) is
        // `a_repeatedly_panicking_source_surfaces_one_error_not_a_respawn_storm`'s own job,
        // below, not this one's.
        let src = Arc::new(
            MockSource::new(Bytes::from_static(b"0123456789"))
                .with_gate()
                .with_gate_safety_bound(std::time::Duration::from_millis(50)),
        );
        src.arm_gate();
        let c = Arc::new(BlockCache::new(src.clone(), 4, 64));
        let req = tokio::spawn({
            let c = c.clone();
            async move { c.block(0).await }
        });
        wait_for_read_started(&src, 1).await;
        req.abort();
        let _ = req.await;
        // the detached fetch is still running, parked on the gate, heading for its own
        // safety-bound panic in <=50ms; nobody is left to observe its outcome either way.
        let mut gate_timeouts = src.gate_timeout_events();
        wait_for_count(&mut gate_timeouts, |n| n >= 1).await;
        // positive proof the panic did NOT counterfeit a normal-looking state: the registration
        // must actually clear, not merely "look empty because nothing rechecked it" -- polled
        // via `yield_now`, matching `wait_for_in_flight_len_at_least`'s own idiom, giving the
        // panicking task's own unwind (running `FetchCompletion::drop`) a chance to complete.
        wait_for_in_flight_len_at_most(&c, 0).await;
    }
    #[tokio::test]
    async fn a_panicking_detached_fetch_survives_an_already_poisoned_state_mutex() {
        // "the poison test" (restr-R2-brief.md's acceptance #3): `FetchCompletion::drop`'s
        // involuntary-teardown arm runs DURING unwinding, so if the state mutex happens to
        // ALREADY be poisoned (for any unrelated reason) when that arm fires, a plain
        // `.lock().unwrap()` there would panic a SECOND time while THIS panic is still
        // unwinding -- which Rust turns into an immediate process ABORT, not a catchable error,
        // regardless of how long ago or by whom the mutex was poisoned. This is exactly
        // `InFlightGuard::drop`'s still-latent pre-R2 hazard (it used a bare `.lock().unwrap()`
        // too, never exercised because nothing poisoned the mutex before it fired) --
        // constructed here so that hazard would have tripped, via an entirely independent panic
        // that poisons the mutex first.
        //
        // Ordering matters and is deliberate: registration and the requester's own abort happen
        // FIRST, while the mutex is still healthy (both use the ordinary, panic-on-poison
        // `lock_state`, on purpose -- see that method's own doc comment for why an UNEXPECTED
        // poison anywhere else in this file should still fail loud). Only once the fetch has
        // genuinely committed to its read (uncapped admission resolves on the very first poll,
        // with no yield point in between -- the same reasoning
        // `a_panicking_detached_fetch_does_not_leak_its_registration` already relies on) is the
        // mutex poisoned, from an entirely independent task, simulating some OTHER bug having
        // poisoned it moments earlier -- unrelated to, and not caused by, anything this fetch
        // itself does.
        let src = Arc::new(
            MockSource::new(Bytes::from_static(b"0123456789"))
                .with_gate()
                .with_gate_safety_bound(std::time::Duration::from_millis(50)),
        );
        src.arm_gate();
        let c = Arc::new(BlockCache::new(src.clone(), 4, 64));
        let req = tokio::spawn({
            let c = c.clone();
            async move { c.block(0).await }
        });
        wait_for_read_started(&src, 1).await;
        req.abort();
        let _ = req.await;

        // NOW poison the mutex -- after the requester's own abort (and its `WaiterTicket::drop`)
        // already ran cleanly against the healthy mutex, and before the fetch's own gate-timeout
        // panic (bounded at 50ms) has had a chance to fire.
        let poisoner_inner = c.inner.clone();
        let poisoned = tokio::spawn(async move {
            let _st = poisoner_inner.lock_state();
            panic!("deliberately poisoning the state mutex for this test");
        })
        .await;
        assert!(
            poisoned.is_err(),
            "the poisoning task must itself have panicked"
        );
        assert!(
            c.inner.state.is_poisoned(),
            "the state mutex must actually be poisoned before the real assertion below means \
             anything"
        );

        // the detached fetch is still running, parked on the gate, heading for its own
        // safety-bound panic in <=50ms -- with the mutex already poisoned. With a
        // non-poison-tolerant `Drop`, this would abort the WHOLE PROCESS here, not merely fail
        // this one task -- reaching any assertion below at all is itself part of the proof.
        let mut gate_timeouts = src.gate_timeout_events();
        wait_for_count(&mut gate_timeouts, |n| n >= 1).await;
        // if this line runs at all, the process did not abort. `FetchCompletion::drop` ran as
        // part of the SAME synchronous unwind that already fired the gate-timeout event above
        // (both are destructors on the same panicking call chain), so by now it has already
        // used `lock_state_tolerating_poison` -- which also HEALS the mutex -- meaning the
        // ordinary, panic-on-poison `lock_state` below must work normally again, not panic a
        // third time.
        assert!(
            !c.inner.state.is_poisoned(),
            "FetchCompletion::drop's own recovery must clear the poison, not just survive it"
        );
        wait_for_in_flight_len_at_most(&c, 0).await;
        assert_eq!(
            c.in_flight_len(),
            0,
            "the registration must clear correctly despite the poison -- a poisoned \
             std::sync::Mutex does not corrupt the data it guards, only marks that a panic once \
             held the lock"
        );

        // the cache must stay fully usable afterward, for an entirely unrelated block.
        src.open_gate();
        let bytes = c.block(1).await.unwrap();
        assert_eq!(&bytes[..], b"4567");
    }
    #[tokio::test]
    async fn k_distinct_block_fetches_die_unstarted_once_every_requester_is_gone() {
        // restructure R2 acceptance #1 / batch 5 unit I's RED 1 (finding #4), GREEN: supersedes
        // `red_distinct_block_fetches_stay_registered_after_every_requester_drops`, which pinned
        // TODAY's-then broken opposite outcome against the pre-restructure code (confirmed
        // failing there: `in_flight_len()` stuck at 5, `started_events` stuck at 1, forever --
        // deleted once this fix landed, per the same delete-the-RED-fixture pattern batch 4 unit
        // D's own `red_superseded_requester_stacks_a_second_physical_read` used). K distinct-
        // block requests through a 1-permit admission-capped, gated source; drop every
        // requester. Now: the K-1 unadmitted fetches die unstarted (their `admit` future was
        // never anything but PENDING, so dropping it -- via `try_abandon`'s `Abandon::Done` --
        // consumes nothing and starts no read); the one admitted survivor keeps its read
        // entered and, once released, still publishes (D's rule, unweakened) and returns the
        // permit.
        const K: u64 = 5;
        let src = Arc::new(
            MockSource::new(Bytes::from_static(&[0u8; 4096]))
                .with_admission_cap(1)
                .with_gate(),
        );
        src.arm_gate();
        let c = Arc::new(BlockCache::new(src.clone(), 4, 1 << 20));
        let mut started = src.started_events();
        let mut reqs = Vec::new();
        for i in 0..K {
            let cc = c.clone();
            reqs.push(tokio::spawn(async move { cc.block(i).await }));
        }
        // exactly one gets admitted (the cap is 1) and genuinely enters its read, parking on
        // the gate; the other K-1 register but never get past `admit` -- both facts positively
        // observed, not inferred.
        wait_for_count(&mut started, |n| n >= 1).await;
        wait_for_in_flight_len_at_least(&c, K as usize).await;
        for r in reqs {
            r.abort();
            let _ = r.await;
        }
        // the K-1 unadmitted fetches must die unstarted: the registry drains to 1 (only the
        // admitted survivor remains), and no additional read is ever entered -- zero reads
        // entered for the K-1, exactly as the brief's own evidence contract asks.
        wait_for_in_flight_len_at_most(&c, 1).await;
        assert_eq!(
            c.in_flight_len(),
            1,
            "only the admitted survivor may remain registered after the mass-drop"
        );
        assert_eq!(
            *started.borrow(),
            1,
            "an unadmitted fetch must never enter a read once its last waiter is gone"
        );
        // the survivor's own permit is still held (its read has not returned yet) -- the cap's
        // one permit is not "at baseline" while a legitimately still-running read holds it.
        assert_eq!(
            src.admission_available_permits(),
            0,
            "the survivor's own admission permit is still legitimately held"
        );
        // D's rule, unweakened: the survivor's read already entered, so it still publishes once
        // released, even though every one of its own requesters is long gone; and the permit
        // returns to baseline once that read returns, not merely once it was admitted.
        src.open_gate();
        wait_for_in_flight_len_at_most(&c, 0).await;
        assert_eq!(
            c.in_flight_len(),
            0,
            "the survivor must deregister once it publishes"
        );
        assert_eq!(
            src.admission_available_permits(),
            1,
            "the permit returns to baseline once the survivor's read actually returns"
        );
    }
    #[tokio::test]
    async fn a_repeatedly_panicking_source_surfaces_one_error_not_a_respawn_storm() {
        // restructure R2 acceptance #2 / batch 5 unit I's RED 2 (finding #9), GREEN: supersedes
        // `red_a_panicking_source_hangs_the_caller_in_a_respawn_loop`, which pinned TODAY's-then
        // broken hang against the pre-restructure code (confirmed hanging there: the bounded
        // `tokio::time::timeout` elapsed, `attempts` kept growing past 1 -- deleted once this
        // fix landed). `FetchCompletion::drop` is the involuntary-teardown arm now: a panic mid-
        // read unwinds through `fetch`'s own frame, dropping `completion` before it ever reaches
        // `publish`, so `Drop` publishes an ERROR instead of leaving a closed channel behind --
        // the waiter's own blind self-heal-and-retry no longer exists in `get` at all, so there
        // is no loop left to spin.
        struct AlwaysPanics {
            attempts: Arc<AtomicU64>,
        }
        #[async_trait::async_trait]
        impl BlockSource for AlwaysPanics {
            fn size(&self) -> u64 {
                64
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let attempts = self.attempts.clone();
                crate::source::ReadTicket::from_fn(move |_offset, _len| {
                    Box::pin(async move {
                        attempts.fetch_add(1, Ordering::Relaxed);
                        panic!("this source always panics on read");
                    })
                })
            }
        }
        let attempts = Arc::new(AtomicU64::new(0));
        let src = Arc::new(AlwaysPanics {
            attempts: attempts.clone(),
        });
        let c = BlockCache::new(src, 4, 64);
        // must resolve -- not hang -- within a diagnostic ceiling; a correct implementation
        // resolves in microseconds (panic, unwind, publish), so any reasonable bound proves the
        // qualitative "surfaced an answer" claim, not a tuned threshold.
        let err = tokio::time::timeout(crate::source::DIAGNOSTIC_CEILING, c.block(0))
            .await
            .expect(
                "must not hang -- a panicking source must surface an error, not respawn forever",
            )
            .expect_err("a panicking source's read must surface as an error, not a value");
        assert!(
            format!("{err:#}").contains("torn down"),
            "expected the involuntary-teardown error, got: {err:#}"
        );
        assert_eq!(
            attempts.load(Ordering::Relaxed),
            1,
            "exactly one attempt for the first request -- no blind respawn loop"
        );
        // the NEXT request must make a genuinely fresh attempt, not find a dead registration.
        let err2 = tokio::time::timeout(crate::source::DIAGNOSTIC_CEILING, c.block(0))
            .await
            .expect("must not hang on the second request either")
            .expect_err("the second request must also surface an error");
        assert!(format!("{err2:#}").contains("torn down"));
        assert_eq!(
            attempts.load(Ordering::Relaxed),
            2,
            "the next request must make a fresh attempt, not coalesce onto a dead entry"
        );
        assert_eq!(
            c.in_flight_len(),
            0,
            "the registration must not be left behind after the involuntary teardown"
        );
    }
    #[tokio::test]
    async fn concurrent_waiters_all_observe_the_same_error() {
        // an armed gate holds the one real read open until this test has positively confirmed
        // coalescing below, then releases it -- unlike a fixed sleep (this test's own pre-
        // U-failingslow shape), which can complete and clear the in-flight registration on its
        // own timeline, independent of when this test gets around to observing it. Measured
        // directly (U-failingslow): removing the sleep entirely dropped coalesced_events from
        // 3 (every run) to 0 (every run) -- the sleep was genuine coordination, not simulated
        // realism, `with_latency`'s own shape reincarnated inline rather than through that
        // (deleted) method. Unlike `MockSource`'s own general-purpose gate, this one-shot mock
        // only ever has ONE real read to gate at all -- the other 3 callers coalesce as
        // waiters and never call `admit` themselves -- so a minimal, always-armed local
        // gate is enough; no install/arm two-step needed.
        struct FailingSlow {
            gate: tokio::sync::watch::Sender<bool>,
        }
        #[async_trait::async_trait]
        impl BlockSource for FailingSlow {
            fn size(&self) -> u64 {
                8
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let gate = self.gate.clone();
                crate::source::ReadTicket::from_fn(move |_offset, _len| {
                    Box::pin(async move {
                        let mut rx = gate.subscribe();
                        // `crate::source::DIAGNOSTIC_CEILING`, not a second independent literal
                        // (U-guard sweep, pass 8): the exact constant-drift risk that constant's
                        // own doc comment already exists to prevent -- see it for why this must
                        // stay one shared value, not a bound that happens to match today.
                        tokio::time::timeout(
                            crate::source::DIAGNOSTIC_CEILING,
                            rx.wait_for(|open| *open),
                        )
                        .await
                        .expect(
                            "FailingSlow's own gate parked longer than 5s -- forgotten release?",
                        )
                        .expect("the sender lives alongside this receiver, in the same test");
                        Err(anyhow::anyhow!("boom"))
                    })
                })
            }
        }
        let (gate_tx, _) = tokio::sync::watch::channel(false);
        let c = Arc::new(BlockCache::new(
            Arc::new(FailingSlow {
                gate: gate_tx.clone(),
            }),
            4,
            16,
        ));
        let mut coalesced = c.coalesced_events();
        let mut joins = Vec::new();
        for _ in 0..4 {
            let c = c.clone();
            joins.push(tokio::spawn(async move { c.block(0).await }));
        }
        // exactly one of the 4 becomes the fetcher (parked on the gate above); the other 3
        // must have genuinely JOINED as coalescing waiters -- not merely been spawned --
        // before the gate ever opens (the identical shape
        // `concurrent_misses_coalesce_into_one_read`, above, already established).
        wait_for_count(&mut coalesced, |n| n >= 3).await;
        let _ = gate_tx.send(true);
        for j in joins {
            let err = j.await.unwrap().unwrap_err();
            assert!(format!("{err:#}").contains("boom"));
            assert!(
                err.chain().count() >= 2,
                "waiter lost the error chain: {err:#}"
            );
        }
        // the discriminator this test's own name actually claims (U-failingslow): the two
        // assertions above pass identically whether coalescing happened or not -- 4
        // independent reads of the same always-failing source produce 4 qualitatively
        // identical-looking wrapped errors. RED-verified in both directions: (a) breaking
        // coalescing via never-match keys (each of the 4 requesting a distinct block index,
        // so none can coalesce) makes the assertion below fail loud (0 != 3), exactly as
        // intended; (b) with coalescing broken the SAME way but this assertion removed, the
        // two assertions above still pass every time -- proof they alone cannot tell four
        // independent-but-similar-looking errors apart from one shared one. Both reverted
        // after confirming. This is what actually proves "concurrent waiters observed the
        // SAME error," not four separate ones that merely look alike.
        assert_eq!(
            *coalesced.borrow(),
            3,
            "expected exactly 3 of the 4 concurrent callers to coalesce onto the one real fetch"
        );
    }
    // The race argument, white-box (restr-R2-brief.md's acceptance #5): the abandon/join race
    // rests on an argument about a MUTEX, not a type -- "the waiter count is read and the entry
    // removed in one critical section, and joins increment in the same one" -- so a real
    // concurrent repro would only ever exercise ONE of the two possible lock orderings per run,
    // whichever the scheduler happens to pick, exactly the kind of scheduler-dependent proof
    // this crate's own testing discipline (AGENTS.md) refuses to rely on. Driving
    // `in_flight`'s own methods directly, in a CHOSEN order, is the faithful, deterministic way
    // to pin BOTH orderings the mutex can produce in reality -- not a shortcut around the race,
    // but the same race stated as two cases instead of left to chance.
    #[tokio::test]
    async fn a_joiner_landing_before_the_abandon_check_gets_still_wanted() {
        let src = Arc::new(MockSource::new(Bytes::from_static(b"01234567")));
        let c = BlockCache::new(src, 4, 64);
        let inner = c.inner.clone();
        let (initiator, completion, _waiterless) = {
            let mut st = inner.lock_state();
            st.in_flight.register(0, &inner, Acc::Empty)
        };
        // the sole waiter leaves: waiters 1 -> 0.
        drop(initiator);
        // ORDERING A: a joiner's own `join` (which takes the SAME lock to increment) lands
        // BEFORE `try_abandon`'s check below -- the exact interleaving that must yield
        // `StillWanted`, never a joiner left holding a ticket to a registration that is about to
        // vanish.
        let joiner = {
            let mut st = inner.lock_state();
            st.in_flight.join(0, &inner)
        };
        assert!(
            joiner.is_some(),
            "a joiner arriving while the registration is still live must join it"
        );
        match completion.try_abandon() {
            Abandon::StillWanted(c) => {
                // end it cleanly so this test does not itself leak a registration; the joiner's
                // own ticket then correctly observes the publish.
                c.publish(Ok(Block::uncertified(Bytes::from_static(b"0123"))));
            }
            Abandon::Done => panic!(
                "a live joiner must prevent abandonment -- the count was incremented under the \
                 SAME lock this check reads"
            ),
        }
        let joiner = joiner.unwrap();
        assert_eq!(&joiner.rx.borrow().clone().unwrap().unwrap()[..], b"0123");
        drop(joiner);
        assert_eq!(inner.lock_state().in_flight.len(), 0);
    }
    #[tokio::test]
    async fn a_joiner_landing_after_the_abandon_check_gets_a_genuinely_fresh_registration() {
        let src = Arc::new(MockSource::new(Bytes::from_static(b"01234567")));
        let c = BlockCache::new(src, 4, 64);
        let inner = c.inner.clone();
        let (initiator, completion, _waiterless) = {
            let mut st = inner.lock_state();
            st.in_flight.register(1, &inner, Acc::Empty)
        };
        drop(initiator);
        // ORDERING B: `try_abandon`'s check-and-remove (one lock acquisition covering both)
        // completes in full BEFORE any joiner arrives.
        match completion.try_abandon() {
            Abandon::Done => {}
            Abandon::StillWanted(_) => panic!("zero waiters must abandon"),
        }
        assert_eq!(
            inner.lock_state().in_flight.len(),
            0,
            "the dead registration must be gone"
        );
        // a joiner arriving AFTER must find NOTHING -- never a ticket to a registration whose
        // fetch task has already returned without publishing anything.
        let late_joiner = {
            let mut st = inner.lock_state();
            st.in_flight.join(1, &inner)
        };
        assert!(
            late_joiner.is_none(),
            "a joiner arriving after the abandon must find no registration to join -- the \
             caller must mint a genuinely fresh fetch instead, never wait on a dead channel"
        );
    }
    // The MUTATION half of the race argument: break the one-critical-section property
    // deliberately (split `try_abandon`'s check-and-remove across TWO separate lock
    // acquisitions, with a joiner's own `join` landing in the gap between them -- exactly the
    // "decide-then-deregister in two sections" shape the brief names) and show a joiner ends up
    // holding a ticket to a registration that has ALREADY been removed, so its outcome channel
    // will never receive a publish. This is reverted immediately after being observed; see this
    // test's own report entry for the exact failure this produced.
    #[tokio::test]
    async fn mutation_splitting_the_abandon_check_from_the_removal_strands_a_joiner() {
        let src = Arc::new(MockSource::new(Bytes::from_static(b"01234567")));
        let c = BlockCache::new(src, 4, 64);
        let inner = c.inner.clone();
        let (initiator, mut completion, _waiterless) = {
            let mut st = inner.lock_state();
            st.in_flight.register(2, &inner, Acc::Empty)
        };
        drop(initiator);
        // the BUGGY shape: read `waiters()` under one lock acquisition, release it, let a
        // joiner land in the gap, THEN remove under a SECOND, fresh lock acquisition -- as
        // opposed to `try_abandon`'s real, one-critical-section implementation (`cache::tests`
        // is a descendant of `cache`, so it can reach `completion.slot` directly, same as
        // `try_abandon`'s own body does).
        let waiters_seen_stale = {
            let st = inner.lock_state();
            st.in_flight.waiters(2, &completion.slot)
        };
        assert_eq!(
            waiters_seen_stale,
            Some(0),
            "the stale read must see zero, as real code would at this exact point"
        );
        // a joiner lands in the gap the split created.
        let stranded_joiner = {
            let mut st = inner.lock_state();
            st.in_flight.join(2, &inner)
        };
        assert!(
            stranded_joiner.is_some(),
            "the joiner must still find the registration live -- the removal hasn't happened yet"
        );
        // the stale decision (computed before the joiner arrived) is now acted on anyway --
        // matching `try_abandon`'s real `Done` bookkeeping exactly (`end`, then disarm `Drop`),
        // just with the removal happening under a SEPARATE lock acquisition from the read that
        // decided it, which is the entire mutation.
        {
            let mut st = inner.lock_state();
            st.in_flight.end(2, &completion.slot);
        }
        completion.live = false;
        drop(completion); // now a no-op Drop, exactly like the real `Done` arm's own teardown.
        assert_eq!(
            inner.lock_state().in_flight.len(),
            0,
            "the split removed the registration despite the joiner"
        );
        // the consequence: the stranded joiner's own outcome channel will NEVER receive a
        // value -- nothing will ever publish to this registration again. A bounded wait proves
        // the channel is dead, not merely slow.
        let mut joiner = stranded_joiner.unwrap();
        let outcome =
            tokio::time::timeout(crate::source::DIAGNOSTIC_CEILING, joiner.rx.changed()).await;
        assert!(
            outcome.is_err(),
            "MUTATION CONFIRMED: a joiner stranded by the split now waits on a channel that will \
             never publish -- exactly the joiner-on-a-dead-channel hazard the one-critical-\
             section property exists to prevent"
        );
    }
    #[tokio::test]
    async fn still_wanted_resumption_never_recreates_the_pinned_admit_future() {
        // StillWanted resumption (restr-R2-brief.md's own battery item): losing the abandon
        // race must be free and resumable -- `completion = c` keeps the SAME `FetchCompletion`,
        // and `admit` is `tokio::pin!`-ed ONCE, outside the phase-1 loop, never reassigned
        // anywhere in `fetch`'s own body. Proved here by counting `admit` INVOCATIONS (not
        // completions) on a source whose `admit` never resolves at all -- so this fetch stays
        // in phase 1 for the whole test, letting it be churned repeatedly while directly
        // observing whether `BlockSource::admit` is ever called a second time.
        struct NeverAdmits {
            admit_calls: Arc<AtomicU64>,
        }
        #[async_trait::async_trait]
        impl BlockSource for NeverAdmits {
            fn size(&self) -> u64 {
                64
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                self.admit_calls.fetch_add(1, Ordering::Relaxed);
                std::future::pending::<()>().await;
                unreachable!("this source's admit never resolves")
            }
        }
        let admit_calls = Arc::new(AtomicU64::new(0));
        let src = Arc::new(NeverAdmits {
            admit_calls: admit_calls.clone(),
        });
        let c = BlockCache::new(src, 4, 64);
        let inner = c.inner.clone();
        let (mut waiter, completion, waiterless) = {
            let mut st = inner.lock_state();
            st.in_flight.register(0, &inner, Acc::Empty)
        };
        tokio::spawn(BlockCache::fetch(inner.clone(), 0, completion, waiterless));
        // wait for the fetch task to have genuinely called `admit` once (positively observed,
        // not inferred from a yield count).
        tokio::time::timeout(crate::source::DIAGNOSTIC_CEILING, async {
            while admit_calls.load(Ordering::Relaxed) < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the fetch task never called admit at all");

        // 5 rounds of churn: drop the current waiter and immediately (no `.await` in between,
        // so the fetch task cannot possibly be polled between these two lines) join a fresh
        // one for the SAME block -- the waiter count goes 1 -> 0 -> 1 entirely within this
        // synchronous span, so whichever ordering the fetch task's own lock acquisition landed
        // in, it must see a live waiter and return `StillWanted`, never `Done`.
        for round in 0..5u32 {
            drop(waiter);
            waiter = {
                let mut st = inner.lock_state();
                st.in_flight
                    .join(0, &inner)
                    .expect("the registration must still be live for this same-turn rejoin")
            };
            // give the fetch task a bounded chance to react to the waiterless bump and resume.
            tokio::time::timeout(crate::source::DIAGNOSTIC_CEILING, tokio::task::yield_now())
                .await
                .unwrap();
            assert_eq!(
                inner.lock_state().in_flight.len(),
                1,
                "round {round}: the registration must survive every churn that keeps a waiter \
                 alive at the moment of the check"
            );
            assert_eq!(
                admit_calls.load(Ordering::Relaxed),
                1,
                "round {round}: StillWanted must reuse the SAME pinned admit future -- admit \
                 must never be invoked a second time"
            );
        }
        // finally, let it end for real: drop the last waiter and never rejoin.
        drop(waiter);
        wait_for_in_flight_len_at_most(&c, 0).await;
        assert_eq!(
            admit_calls.load(Ordering::Relaxed),
            1,
            "even after finally abandoning for real, admit was still only ever called once"
        );
    }
    #[tokio::test]
    async fn the_identity_check_prevents_a_stale_slot_from_touching_a_fresher_registration() {
        // the identity check on `end`/`release_waiter` (restr-R2-brief.md's own battery item):
        // a token whose registration has already been replaced by a fresher fetch (same block
        // index, different `FetchSlot`) must never be able to touch the fresher one.
        let src = Arc::new(MockSource::new(Bytes::from_static(b"01234567")));
        let c = BlockCache::new(src, 4, 64);
        let inner = c.inner.clone();
        // F: register, then end it normally (publish) -- its own slot is now stale relative to
        // whatever comes next for the same block index.
        let (initiator, completion, _waiterless) = {
            let mut st = inner.lock_state();
            st.in_flight.register(0, &inner, Acc::Empty)
        };
        let stale_slot = completion.slot.clone();
        drop(initiator);
        completion.publish(Ok(Block::uncertified(Bytes::from_static(b"0000"))));
        assert_eq!(inner.lock_state().in_flight.len(), 0);
        // F': a FRESH registration for the SAME block index.
        let (fresh_initiator, fresh_completion, _fresh_waiterless) = {
            let mut st = inner.lock_state();
            st.in_flight.register(0, &inner, Acc::Empty)
        };
        assert_eq!(inner.lock_state().in_flight.len(), 1);
        // using the STALE slot (F's, not F''s) must be a complete no-op against the CURRENT
        // registry -- both the removal and the waiter-release paths.
        {
            let mut st = inner.lock_state();
            st.in_flight.end(0, &stale_slot);
        }
        assert_eq!(
            inner.lock_state().in_flight.len(),
            1,
            "a stale slot must never remove a fresher registration for the same index"
        );
        let released = {
            let mut st = inner.lock_state();
            st.in_flight.release_waiter(0, &stale_slot)
        };
        assert!(
            !released,
            "a stale slot's release must be a silent no-op, never touching the fresh \
             registration's own waiter count"
        );
        assert_eq!(
            inner
                .lock_state()
                .in_flight
                .waiters(0, &fresh_completion.slot),
            Some(1),
            "the fresh registration's own waiter count must be untouched by the stale slot"
        );
        // clean up.
        drop(fresh_initiator);
        fresh_completion.publish(Ok(Block::uncertified(Bytes::from_static(b"0000"))));
    }

    // ============ The lost-wakeup window, the biased arm's tie, and the strand detector ========
    // Deterministic constructions for three properties real concurrent scheduling cannot be
    // relied on to exercise on demand: a requester dropped before the fetch task's own first
    // poll (the exact shape a losing `select!` arm produces), a genuine same-poll tie between
    // `waiterless.changed()` and `admit` (constructed with no `.await` between the two events
    // that create it, so the fetch task cannot observe them separately), and a positive,
    // large-N strand detector driving the abandon/join race against real `try_abandon` from a
    // different worker thread than the fetch task itself.

    /// Polls `fut` exactly once, without ever yielding to the runtime. Returns true if it
    /// completed. This is the shape `tokio::select!` produces for a losing arm.
    async fn poll_once_and_drop<F: std::future::Future>(fut: F) {
        tokio::pin!(fut);
        tokio::select! {
            biased;
            _ = &mut fut => {},
            _ = std::future::ready(()) => {},
        }
    }

    #[tokio::test]
    async fn probe_p1_requester_dropped_before_the_fetch_task_first_poll() {
        // The requester registers and is dropped inside ONE poll of the enclosing task -- so
        // `WaiterTicket::drop`'s waiterless bump lands BEFORE the spawned fetch task has run its
        // first line. `register` mints the fetch task's own `waiterless` receiver at
        // registration time, seeded before anything could exist to bump it -- if a late
        // subscribe (a `watch::Sender::subscribe` call inside the task's own first poll,
        // marking the CURRENT version as already-seen) were ever reintroduced, the bump would
        // be silently lost, and the fetch would never learn nobody wants the block, committing
        // to a read anyway.
        let src = Arc::new(MockSource::new(Bytes::from_static(&[7u8; 4096])).with_gate());
        src.arm_gate();
        let c = Arc::new(BlockCache::new(src.clone(), 4, 1 << 20));
        let mut started = src.started_events();
        poll_once_and_drop(c.block(1)).await;
        assert_eq!(
            c.in_flight_len(),
            1,
            "the fetch registered on that single poll"
        );
        // POSITIVE, discriminating signal: a read ENTERED for a block whose only requester is
        // already gone. A correct implementation never fires this event at all.
        // ACCEPTED-RESIDUAL: a bounded absence window (AGENTS.md closed-campaign policy,
        // 2026-07-22) -- there is no positive "a read was never entered" signal to wait for, so
        // this can only give a hypothetically-broken implementation a few seconds to prove
        // itself wrong before concluding it did not.
        let entered = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            wait_for_count(&mut started, |n| n >= 1),
        )
        .await;
        assert!(
            entered.is_err(),
            "LOST WAKEUP: a physical read was entered for a block whose only requester was \
             dropped before the fetch task's first poll (started_events reached {:?}); \
             in_flight_len = {}",
            entered.ok(),
            c.in_flight_len()
        );
    }

    #[tokio::test]
    async fn probe_p1_production_shaped_unbiased_select_hits_it_about_half_the_time() {
        // No `biased`, no contrivance: `analyzer.rs`'s driver races a fresh-request signal
        // against a step that reads through the cache. When the fresh request is ALREADY
        // pending, tokio's randomized `select!` polls the cache-read arm first about half the
        // time -- registering the fetch, then dropping it in the same poll. Counts how many
        // rounds leak an unwanted read.
        const ROUNDS: u64 = 40;
        let mut leaked = 0u64;
        for round in 0..ROUNDS {
            let src = Arc::new(MockSource::new(Bytes::from_static(&[7u8; 4096])).with_gate());
            src.arm_gate();
            let c = Arc::new(BlockCache::new(src.clone(), 4, 1 << 20));
            let started = src.started_events();
            let (tx, mut rx) = tokio::sync::watch::channel(0u64);
            tx.send_replace(1); // the fresher request is already waiting: this arm is ready now
            tokio::select! {
                _ = rx.changed() => {},
                r = c.block(round) => { let _ = r; },
            }
            // ACCEPTED-RESIDUAL: a bounded absence window (AGENTS.md closed-campaign policy,
            // 2026-07-22) -- there is no positive "this read will never start" signal, so a
            // fixed settle window is what lets any spawned fetch task run to the point where
            // it would enter its read, if it were going to at all.
            for _ in 0..50 {
                tokio::task::yield_now().await;
            }
            if *started.borrow() > 0 {
                leaked += 1;
            }
        }
        assert_eq!(
            leaked, 0,
            "LOST WAKEUP reachable from the production driver shape: {leaked}/{ROUNDS} rounds \
             entered a physical read for a block whose requester was already gone"
        );
    }

    // A source whose admission is opened by the test, synchronously, on demand -- shared by the
    // biased-arm tie probe and the strand detector below.
    struct GatedAdmit {
        open: tokio::sync::watch::Receiver<bool>,
        admit_entered: tokio::sync::watch::Sender<u64>,
        admits: Arc<AtomicU64>,
        reads: Arc<AtomicU64>,
    }
    #[async_trait::async_trait]
    impl BlockSource for GatedAdmit {
        fn size(&self) -> u64 {
            4096
        }
        async fn admit(&self) -> crate::source::ReadTicket {
            let n = self.admits.fetch_add(1, Ordering::Relaxed) + 1;
            self.admit_entered.send_replace(n);
            let mut open = self.open.clone();
            while !*open.borrow_and_update() {
                open.changed().await.expect("the test holds the sender");
            }
            let reads = self.reads.clone();
            crate::source::ReadTicket::from_fn(move |_offset, len| {
                Box::pin(async move {
                    reads.fetch_add(1, Ordering::Relaxed);
                    Ok(Bytes::from(vec![0u8; len]))
                })
            })
        }
    }

    #[tokio::test]
    async fn probe_biased_arm_a_genuine_tie_is_constructible() {
        // A real tie (both `select!` arms ready on the same poll) IS constructible without any
        // scheduler-timing dependence. Park the fetch task inside `admit`, then -- with NO
        // await in between, so the fetch task cannot be polled in between -- drop the last
        // requester (bumping `waiterless`) AND open admission. On the fetch task's very next
        // poll BOTH arms are ready. `biased` decides which wins, and the two outcomes differ
        // observably: abandon (no read) vs. commit (a physical read for a block nobody wants).
        const ROUNDS: u32 = 24;
        let mut reads_total = 0u64;
        for _ in 0..ROUNDS {
            let (open_tx, open_rx) = tokio::sync::watch::channel(false);
            let (entered_tx, mut entered_rx) = tokio::sync::watch::channel(0u64);
            let admits = Arc::new(AtomicU64::new(0));
            let reads = Arc::new(AtomicU64::new(0));
            let src = Arc::new(GatedAdmit {
                open: open_rx,
                admit_entered: entered_tx,
                admits: admits.clone(),
                reads: reads.clone(),
            });
            let c = Arc::new(BlockCache::new(src, 4, 1 << 20));
            let mut req = Box::pin(c.block(0));
            // one poll: registers + spawns the fetch task, then pends.
            tokio::select! {
                biased;
                _ = &mut req => unreachable!("admission is closed"),
                _ = std::future::ready(()) => {},
            }
            // let the fetch task subscribe to `waiterless` and park inside `admit`.
            wait_for_count(&mut entered_rx, |n| n >= 1).await;
            // --- the tie, constructed synchronously: no `.await` between these two lines, so
            // the fetch task cannot observe either event separately.
            drop(req); //           arm 1 becomes ready (waiterless bumps past the seen version)
            open_tx.send_replace(true); // arm 2 becomes ready (admit resolves on its next poll)
            // ACCEPTED-RESIDUAL: a bounded absence window (AGENTS.md closed-campaign policy,
            // 2026-07-22) -- there is no positive "no read was entered" signal for the abandon
            // arm winning the tie, so a fixed settle window is what lets the fetch task's own
            // resolution of the tie run to completion before this round's own read count is
            // read.
            for _ in 0..50 {
                tokio::task::yield_now().await;
            }
            reads_total += reads.load(Ordering::Relaxed);
        }
        assert_eq!(
            reads_total, 0,
            "on a genuine tie the abandon arm must win: {reads_total} physical reads were \
             entered across {ROUNDS} constructed ties for blocks nobody wanted"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn probe_join_leave_churn_never_strands_a_waiter() {
        // The abandon/join race under real concurrent scheduling: hold the source's ONE
        // admission permit with a gated read on block 0, so block 1's fetch is pinned in
        // phase 1 for the whole test -- the abandon window held open by construction, not by
        // timing. Then churn requesters for block 1 from 8 workers. Nothing may panic
        // (`try_abandon`'s own registration `expect`), no requester may be left waiting on a
        // dead channel, and block 1 must never stack more than one physical read.
        let src = Arc::new(
            MockSource::new(Bytes::from_static(&[3u8; 65536]))
                .with_admission_cap(1)
                .with_gate(),
        );
        src.arm_gate();
        let c = Arc::new(BlockCache::new(src.clone(), 64, 1 << 20));
        let mut started = src.started_events();
        let hog = {
            let c = c.clone();
            tokio::spawn(async move { c.block(0).await })
        };
        wait_for_count(&mut started, |n| n >= 1).await;

        let mut workers = Vec::new();
        for _ in 0..8 {
            let c = c.clone();
            workers.push(tokio::spawn(async move {
                for _ in 0..300 {
                    let h = {
                        let c = c.clone();
                        tokio::spawn(async move { c.block(1).await })
                    };
                    tokio::task::yield_now().await;
                    h.abort();
                    let _ = h.await;
                }
            }));
        }
        for w in workers {
            w.await.expect("a churn worker panicked");
        }
        // block 1 was never admitted (the cap's one permit is held by block 0's gated read),
        // so no read for it can have started.
        assert_eq!(
            *started.borrow(),
            1,
            "only block 0's read may have entered while the single permit is held"
        );
        // A genuinely fresh requester must still get an answer -- not hang on a dead channel.
        src.open_gate();
        let bytes = tokio::time::timeout(crate::source::DIAGNOSTIC_CEILING, c.block(1))
            .await
            .expect("a post-churn requester hung -- it is waiting on a dead outcome channel")
            .expect("a post-churn requester got an error");
        assert_eq!(bytes.len(), 64);
        let _ = hog.await;
        wait_for_in_flight_len_at_most(&c, 0).await;
        assert_eq!(
            c.in_flight_len(),
            0,
            "the registry must drain after the churn"
        );
        assert!(
            src.read_count() <= 2,
            "at most one physical read per block: got {} reads",
            src.read_count()
        );
    }

    #[tokio::test]
    async fn probe_a_joiner_arriving_during_stillwanted_resumption_is_never_stranded() {
        // The interleaving the one-critical-section argument exists for, driven end to end:
        // a fetch parked in phase 1, its waiter count driven 1 -> 0 -> 1 repeatedly with NO
        // await between the drop and the rejoin, so the fetch task's own lock acquisition can
        // land on either side of the joiner every round. Every rejoined waiter must end up
        // receiving the eventual published value.
        let src = Arc::new(
            MockSource::new(Bytes::from_static(&[9u8; 4096]))
                .with_admission_cap(1)
                .with_gate(),
        );
        src.arm_gate();
        let c = Arc::new(BlockCache::new(src.clone(), 4, 1 << 20));
        let mut started = src.started_events();
        let hog = {
            let c = c.clone();
            tokio::spawn(async move { c.block(0).await })
        };
        wait_for_count(&mut started, |n| n >= 1).await;
        let inner = c.inner.clone();
        // register block 1's fetch by hand so the churn can drive its ticket directly.
        let (mut waiter, completion, waiterless) = {
            let mut st = inner.lock_state();
            st.in_flight.register(1, &inner, Acc::Empty)
        };
        tokio::spawn(BlockCache::fetch(inner.clone(), 1, completion, waiterless));
        for round in 0..200u32 {
            drop(waiter);
            waiter = {
                let mut st = inner.lock_state();
                match st.in_flight.join(1, &inner) {
                    Some(t) => t,
                    None => panic!(
                        "round {round}: the registration was abandoned despite the rejoin \
                         landing in the same synchronous span as the drop -- a joiner would \
                         have been stranded on a dead channel"
                    ),
                }
            };
            tokio::task::yield_now().await;
        }
        // the surviving waiter must receive the real published value once admission frees up.
        src.open_gate();
        let _ = hog.await;
        let got = tokio::time::timeout(crate::source::DIAGNOSTIC_CEILING, async {
            loop {
                if let Some(v) = waiter.rx.borrow().clone() {
                    return v;
                }
                waiter
                    .rx
                    .changed()
                    .await
                    .expect("channel closed with no value");
            }
        })
        .await
        .expect("the surviving joiner was stranded -- nothing ever published");
        assert!(
            got.is_ok(),
            "the surviving joiner received an error: {got:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn probe_strand_detector_one_critical_section_is_load_bearing() {
        // A POSITIVE strand detector for the abandon/join race, run against real production
        // code (not a hand-built replica): the fetch is pinned in phase 1 forever (admission
        // never opens), while this task drives its waiter count 1 -> 0 -> 1 as fast as it can
        // from a DIFFERENT worker thread than the fetch task. After every successful `join`
        // -- which incremented the count under the lock -- the registration MUST still be
        // ours. If `try_abandon` ever read `waiters == 0` in one critical section and removed
        // the entry in a later one, our join lands in the gap and we are left holding a live
        // ticket on a deregistered slot: a joiner on a dead channel, detected directly rather
        // than as a hang. Closes the analyst's own "mutex argument, not proof" honesty gap --
        // the one-critical-section property now has a positive test against real, unmutated
        // production code, not only the hand-built replica
        // (`mutation_splitting_the_abandon_check_from_the_removal_strands_a_joiner`, above).
        let (_open_tx, open_rx) = tokio::sync::watch::channel(false);
        let (entered_tx, mut entered_rx) = tokio::sync::watch::channel(0u64);
        let src = Arc::new(GatedAdmit {
            open: open_rx,
            admit_entered: entered_tx,
            admits: Arc::new(AtomicU64::new(0)),
            reads: Arc::new(AtomicU64::new(0)),
        });
        let c = Arc::new(BlockCache::new(src, 4, 1 << 20));
        let inner = c.inner.clone();
        let (mut t, completion, waiterless) = {
            let mut st = inner.lock_state();
            st.in_flight.register(1, &inner, Acc::Empty)
        };
        tokio::spawn(BlockCache::fetch(inner.clone(), 1, completion, waiterless));
        // the fetch task is genuinely parked inside `admit`, past its waiterless subscribe.
        wait_for_count(&mut entered_rx, |n| n >= 1).await;

        let mut stranded = 0u64;
        let mut rejoins = 0u64;
        let mut fresh = 0u64;
        for _ in 0..30_000u32 {
            drop(t);
            let joined = {
                let mut st = inner.lock_state();
                st.in_flight.join(1, &inner)
            };
            match joined {
                Some(nt) => {
                    rejoins += 1;
                    t = nt;
                    let still_ours = {
                        let st = inner.lock_state();
                        st.in_flight.waiters(1, &t.slot).is_some()
                    };
                    if !still_ours {
                        stranded += 1;
                        break;
                    }
                }
                None => {
                    // legitimate outcome: the fetch abandoned before this rejoin landed, so a
                    // genuinely fresh fetch is required -- exactly the design's other branch.
                    fresh += 1;
                    let (nt, nc, nw) = {
                        let mut st = inner.lock_state();
                        st.in_flight.register(1, &inner, Acc::Empty)
                    };
                    tokio::spawn(BlockCache::fetch(inner.clone(), 1, nc, nw));
                    t = nt;
                }
            }
        }
        assert_eq!(
            stranded, 0,
            "a joiner was stranded on a deregistered slot after a successful join \
             ({rejoins} rejoins, {fresh} fresh registrations)"
        );
        assert!(
            fresh > 0 || rejoins > 0,
            "the churn never actually raced the fetch task"
        );
    }
    // ---- batch 13 (2026-07-29): the refill restructure's own referees ----
    //
    // Polled on a synchronous atomic, not slept on: `admits` is incremented before the source parks,
    // so "a refill is parked in admission" is a real fact a test can wait for -- the same rule
    // `wait_for_in_flight_len_at_least` above follows (tests prove events, not scheduler timing).
    // Bounded by the shared diagnostic ceiling purely as a hang backstop.
    async fn wait_for_counter(admits: &Arc<AtomicU64>, n: u64) {
        tokio::time::timeout(crate::source::DIAGNOSTIC_CEILING, async {
            while admits
                .load(Ordering::Relaxed)
                .max(admits.load(Ordering::SeqCst))
                < n
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "admissions never reached {n} (stuck at {})",
                admits.load(Ordering::SeqCst)
            )
        });
    }
    /// A conforming source that answers ONE byte per read and parks every admission on `gate`, so a
    /// test can hold a refill at its commitment point and abort it there. `admits` counts entries
    /// into `admit` (before the park), which is what makes the park observable without a sleep.
    struct DribblesWhenAdmitted {
        admits: Arc<AtomicU64>,
        /// tickets actually SPENT -- the count that separates "an attempt was consumed" from "a read
        /// happened", which is the entire subject of finding #2.
        reads: Arc<AtomicU64>,
        gate: Arc<tokio::sync::Semaphore>,
        size: u64,
    }
    #[async_trait::async_trait]
    impl BlockSource for DribblesWhenAdmitted {
        fn size(&self) -> u64 {
            self.size
        }
        async fn admit(&self) -> crate::source::ReadTicket {
            self.admits.fetch_add(1, Ordering::SeqCst);
            // parks HERE -- the commitment point, exactly where the reviewer's aborted searches
            // sat. A dropped `admit` future has started nothing (`source.rs`).
            self.gate
                .acquire()
                .await
                .expect("the gate is never closed")
                .forget();
            let size = self.size;
            let reads = self.reads.clone();
            crate::source::ReadTicket::from_fn(move |offset, _len| {
                Box::pin(async move {
                    reads.fetch_add(1, Ordering::SeqCst);
                    if offset >= size {
                        return Ok(Bytes::new());
                    }
                    Ok(Bytes::from_static(b"x"))
                })
            })
        }
    }
    /// A conforming source whose first answer for each block delivers a single byte and whose
    /// answers at any other offset deliver everything asked for. Stateless by construction: an
    /// initial block fetch always starts block-ALIGNED and a refill never does, so every block costs
    /// exactly one short answer plus one refill that completes it. The shape whose per-block
    /// side-table entries used to accumulate one per block, forever.
    struct ShortFirstAnswerPerBlock {
        data: Bytes,
        block_size: u64,
        /// what `size()` reports -- larger than `data` for a fixture that needs the source's own
        /// EMPTY answer to certify the end, equal to it for one whose file simply ends inside a
        /// block (which needs no certificate: there is nothing beyond to fetch).
        claimed: u64,
        reads: Arc<AtomicU64>,
    }
    #[async_trait::async_trait]
    impl BlockSource for ShortFirstAnswerPerBlock {
        fn size(&self) -> u64 {
            self.claimed
        }
        async fn admit(&self) -> crate::source::ReadTicket {
            let data = self.data.clone();
            let bs = self.block_size;
            let reads = self.reads.clone();
            crate::source::ReadTicket::from_fn(move |offset, len| {
                Box::pin(async move {
                    reads.fetch_add(1, Ordering::SeqCst);
                    if offset >= data.len() as u64 {
                        return Ok(Bytes::new());
                    }
                    let start = offset as usize;
                    let want = if offset % bs == 0 { 1 } else { len };
                    let end = (start + want).min(data.len());
                    Ok(data.slice(start..end))
                })
            })
        }
    }
    /// A source that records the offset of every read it performs and can be made to fail once at a
    /// chosen offset, delivering one byte at a time otherwise. The read log is the whole point:
    /// "did this repeat work it had already done" is a question about offsets, not about results.
    struct LogsEveryRead {
        data: Bytes,
        offsets: Arc<Mutex<Vec<u64>>>,
        fail_once_at: Option<u64>,
        failed: Arc<AtomicU64>,
        gate: Arc<tokio::sync::Semaphore>,
        admits: Arc<AtomicU64>,
    }
    #[async_trait::async_trait]
    impl BlockSource for LogsEveryRead {
        fn size(&self) -> u64 {
            self.data.len() as u64
        }
        async fn admit(&self) -> crate::source::ReadTicket {
            self.admits.fetch_add(1, Ordering::SeqCst);
            self.gate
                .acquire()
                .await
                .expect("the gate is never closed")
                .forget();
            let data = self.data.clone();
            let offsets = self.offsets.clone();
            let fail_once_at = self.fail_once_at;
            let failed = self.failed.clone();
            crate::source::ReadTicket::from_fn(move |offset, _len| {
                Box::pin(async move {
                    offsets.lock().unwrap().push(offset);
                    if fail_once_at == Some(offset) && failed.fetch_add(1, Ordering::SeqCst) == 0 {
                        anyhow::bail!("injected failure at {offset}");
                    }
                    if offset >= data.len() as u64 {
                        return Ok(Bytes::new());
                    }
                    Ok(data.slice(offset as usize..offset as usize + 1))
                })
            })
        }
    }
    /// batch 15 (2026-07-30), finding #2. **A waiterless hand-off is atomic: nothing can observe the
    /// registration as gone before the bytes it produced are there to resume from.**
    ///
    /// The defect: `try_abandon` deregistered, dropped the lock, and absorbed afterwards. A
    /// replacement registering in that gap found no entry and seeded itself from nothing, re-reading
    /// what the abandoned fetch had already read -- offsets `[0, 0]` where the whole guarantee is
    /// `[0, 1]`, and `docs/block_cache.md` says "repeats nothing" in those words.
    ///
    /// **What this pins, exactly.** The guarantee (a replacement resumes) end-to-end, and it is a
    /// real regression net. It does NOT distinguish the one-critical-section fix from the two-lock
    /// arrangement it replaced: there is no `.await` between those two acquisitions, so nothing can
    /// interleave on this runtime, and only a second OS thread landing precisely in that window
    /// would tell them apart. Unlike `mutation_splitting_the_abandon_check_from_the_removal_strands
    /// _a_joiner` below -- whose own race has a positive signal (a stranded joiner is observable) --
    /// this one has none: "the replacement re-read offset 0" IS the failure, so a race test could
    /// never report that the race it needed had actually happened. The fix is structural rather
    /// than pinned: the store is inside the critical section that deregisters, so the window does
    /// not exist to be raced.
    #[tokio::test]
    async fn a_waiterless_handoff_stores_progress_before_it_can_be_replaced() {
        let offsets = Arc::new(Mutex::new(Vec::new()));
        let admits = Arc::new(AtomicU64::new(0));
        let gate = Arc::new(tokio::sync::Semaphore::new(1));
        let c = Arc::new(BlockCache::new(
            Arc::new(LogsEveryRead {
                data: Bytes::from_static(b"AB"),
                offsets: offsets.clone(),
                fail_once_at: None,
                failed: Arc::new(AtomicU64::new(0)),
                gate: gate.clone(),
                admits: admits.clone(),
            }),
            2,
            1 << 20,
        ));
        // one permit: the fetch reads offset 0, then parks in admission for its second pass.
        let h = {
            let c = c.clone();
            tokio::spawn(async move { c.block(0).await.map(|b| b.to_vec()) })
        };
        wait_for_counter(&admits, 2).await;
        h.abort();
        let _ = h.await;
        // the abandon must already have stored the `A`; this fresh request resumes at 1.
        gate.add_permits(8);
        assert_eq!(&c.block(0).await.unwrap()[..], b"AB");
        assert_eq!(
            &offsets.lock().unwrap()[..],
            &[0, 1],
            "a replacement must resume from where the abandoned fetch stopped -- a second read at \
             0 means it registered while the progress was still in flight between two critical \
             sections"
        );
    }
    /// batch 15 (2026-07-30), finding #5. **A read error keeps the passes that succeeded.**
    ///
    /// The defect: the error path published immediately without storing the accumulator, so a source
    /// that delivered `A`, failed at `B`, then recovered performed A, B(fail), A, B -- redoing a
    /// read it had already paid for. The error itself is still propagated, and still not cached.
    #[tokio::test]
    async fn a_read_error_keeps_the_passes_that_already_succeeded() {
        let offsets = Arc::new(Mutex::new(Vec::new()));
        let c = Arc::new(BlockCache::new(
            Arc::new(LogsEveryRead {
                data: Bytes::from_static(b"AB"),
                offsets: offsets.clone(),
                fail_once_at: Some(1),
                failed: Arc::new(AtomicU64::new(0)),
                gate: Arc::new(tokio::sync::Semaphore::new(usize::MAX >> 4)),
                admits: Arc::new(AtomicU64::new(0)),
            }),
            2,
            1 << 20,
        ));
        let err = c.block(0).await.expect_err("the source fails at offset 1");
        assert!(
            format!("{err:#}").contains("injected failure"),
            "the error still reaches the caller: {err:#}"
        );
        // and the retry resumes rather than restarting.
        assert_eq!(&c.block(0).await.unwrap()[..], b"AB");
        assert_eq!(
            &offsets.lock().unwrap()[..],
            &[0, 1, 1],
            "read 0 delivered `A`, read 1 failed, and the retry picks up AT 1 -- `[0, 1, 0, 1]` \
             means the successful pass was thrown away with the error"
        );
    }
    /// batch 15 (2026-07-30), finding #4. **One request, one consumer touch.**
    ///
    /// The defect: a request that found an UNFINISHED entry promoted (or refreshed) it on the way
    /// in, then joined the fetch and promoted AGAIN after publication -- the same request's single
    /// touch counted twice. Staged the way the reviewer staged it: the request touches block 0 and
    /// then waits; block 1 is genuinely re-referenced while it waits, so block 1 becomes the more
    /// recently used of the two; completing block 0 must not reorder them.
    ///
    /// Block 1's touches during the wait are cache HITS by construction -- the source's admission
    /// queue is FIFO, so any permit added to let block 1 through would go to the parked fetch
    /// instead.
    #[tokio::test]
    async fn a_partial_block_joiner_counts_its_touch_once() {
        /// Parks in ADMISSION (so an abandon is possible -- once the ticket is SPENT the caller
        /// must treat the read as committed; admission resolving is one step short of that, with a
        /// final waiter check in between), dribbles block 0 one byte at a time, and answers any
        /// other block in full so it costs a single permit.
        struct DribblesBlockZero {
            admits: Arc<AtomicU64>,
            gate: Arc<tokio::sync::Semaphore>,
        }
        #[async_trait::async_trait]
        impl BlockSource for DribblesBlockZero {
            fn size(&self) -> u64 {
                16
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                self.admits.fetch_add(1, Ordering::SeqCst);
                self.gate
                    .acquire()
                    .await
                    .expect("the gate is never closed")
                    .forget();
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        if offset >= 16 {
                            return Ok(Bytes::new());
                        }
                        if offset < 4 {
                            return Ok(Bytes::from_static(b"x"));
                        }
                        Ok(Bytes::from(vec![b'y'; len.min(16 - offset as usize)]))
                    })
                })
            }
        }
        let admits = Arc::new(AtomicU64::new(0));
        let gate = Arc::new(tokio::sync::Semaphore::new(1));
        // 8 blocks of capacity (probation 2, protected 6), so nothing below is an eviction test.
        let c = Arc::new(BlockCache::new(
            Arc::new(DribblesBlockZero {
                admits: admits.clone(),
                gate: gate.clone(),
            }),
            4,
            32,
        ));
        // leave block 0 unfinished in probation: one byte read, then its only requester aborts.
        let h = {
            let c = c.clone();
            tokio::spawn(async move { c.block(0).await.map(|b| b.len()) })
        };
        wait_for_counter(&admits, 2).await;
        h.abort();
        let _ = h.await;
        // block 1, once, into probation -- so its touches during the wait below need no source read.
        gate.add_permits(1);
        assert_eq!(c.block(1).await.unwrap().len(), 4);
        // the request that starts the completion -- an INITIATOR, which never counted twice.
        let initiator = {
            let c = c.clone();
            tokio::spawn(async move { c.block(0).await.map(|b| b.len()) })
        };
        wait_for_counter(&admits, 4).await;
        // THE REQUEST UNDER TEST: a JOINER. It finds block 0 (already protected, so its lookup
        // refreshes recency -- that is its one touch), finds the fetch already in flight, and waits.
        let mut coalesced = c.coalesced_events();
        let joiner = {
            let c = c.clone();
            tokio::spawn(async move { c.block(0).await.map(|b| b.len()) })
        };
        wait_for_count(&mut coalesced, |n| n >= 1).await;
        assert_eq!(
            c.protected_keys(),
            vec![0],
            "precondition: both requests' touches landed on block 0 before either began waiting"
        );
        // while it waits, block 1 is genuinely re-referenced (a hit, no source read), so it is now
        // the more recently used of the two.
        assert_eq!(c.block(1).await.unwrap().len(), 4);
        assert_eq!(c.protected_keys(), vec![1, 0], "precondition");
        gate.add_permits(64);
        assert_eq!(initiator.await.unwrap().unwrap(), 4, "block 0 completes");
        assert_eq!(
            joiner.await.unwrap().unwrap(),
            4,
            "and the joiner gets the same block"
        );
        assert_eq!(
            c.protected_keys(),
            vec![1, 0],
            "and completing it must not count that same request's touch a SECOND time -- `[0, 1]` \
             here puts a block touched once ahead of one the consumer really did re-reference"
        );
    }
    /// batch 15 (2026-07-30), finding #3, restated by batch 16 (2026-07-31), finding #4. **What
    /// assembling a block actually costs.**
    ///
    /// The first version of this test asserted pointer identity across `Acc`'s own `One -> Many`
    /// promotion, which is not a guarantee anything can make: `BytesMut::extend_from_slice` may
    /// reallocate, `Acc::push` has an explicit copy fallback for a `Bytes` it does not uniquely
    /// own, and whether a reallocation lands at the same address is the allocator's business -- the
    /// assertion duly failed under a valid always-moving one. Two claims survive that are really
    /// guarantees, and they are what this asserts:
    ///
    /// 1. **An ordinary miss does not copy the payload.** The block a consumer receives IS the
    ///    buffer the source returned -- pointer identity witnesses that no payload copy happened,
    ///    which is a narrower claim than "no allocation" and the one that matters here.
    /// 2. **Assembly is amortized linear**, which is the honest form of "the dribbling source is
    ///    not quadratic": the accumulator grows geometrically, so the total copying to build a
    ///    block of N bytes is O(N) rather than O(N^2), whatever any individual reallocation does.
    ///
    /// What part 2 does NOT detect: a rebuild into a buffer of the SAME capacity would change no
    /// capacity and pass. It witnesses the growth POLICY (geometric, hence O(N) total), which is
    /// the property the quadratic bug violated; it is not a copy counter, and `Acc` deliberately
    /// has no instrumentation to be one.
    ///
    /// The third claim -- no PER-GENERATION prefix copy -- is structural rather than measurable:
    /// `Acc` is not `Clone`, `check_out` hands it over with `mem::replace`, and the observable
    /// consequence (a resumed generation re-reads nothing) is pinned by
    /// `a_waiterless_handoff_stores_progress_before_it_can_be_replaced`.
    #[tokio::test]
    async fn the_accumulator_costs_no_copy_on_a_miss_and_grows_geometrically() {
        // 1. an ordinary miss: one read answers the whole block, and the bytes handed out are the
        //    source's own allocation (`MockSource` slices its data, so this involves no allocation
        //    to be moved).
        let data = Bytes::from_static(b"0123456789ABCDEF");
        let src = Arc::new(MockSource::new(data.clone()));
        let c = BlockCache::new(src, 16, 1 << 20);
        let got = c.block(0).await.unwrap();
        assert_eq!(&got[..], &data[..]);
        assert_eq!(
            got.as_ptr(),
            data.as_ptr(),
            "the block a consumer receives must BE the buffer the source returned -- a different \
             pointer means every cache miss paid a full block-sized memcpy on top of the read"
        );
        // 2. the dribbling case: 4096 one-byte passes, the shape that was quadratic. Counting
        //    CAPACITY CHANGES witnesses the growth policy directly: geometric growth can only
        //    change capacity O(log N) times, and it is geometric growth that bounds total copying
        //    at O(N). What it detects is a rebuild that GROWS -- the mutation this was verified
        //    against (rebuild into `with_capacity(len)`) reports ~N changes. A rebuild into a
        //    buffer of the SAME capacity would be invisible here; see this test's own doc comment,
        //    which says so rather than letting the assertion imply more than it checks.
        let mut acc = Acc::Empty;
        let mut capacity = acc.capacity();
        let mut grew = 0;
        for _ in 0..4096 {
            acc.push(Bytes::from_static(b"x"));
            if acc.capacity() != capacity {
                capacity = acc.capacity();
                grew += 1;
            }
        }
        assert_eq!(acc.len(), 4096, "every pass landed");
        assert!(
            grew <= 32,
            "4096 one-byte passes must reallocate a logarithmic number of times, not a linear \
             one -- {grew} growth events means the accumulator is being rebuilt per pass, which is \
             the 1+2+...+n behaviour this design exists to avoid"
        );
    }
    /// batch 16 (2026-07-31), finding #2. **A read never starts once the fetch has OBSERVED zero
    /// waiters.** (Stated precisely by batch 18 (2026-07-31): the recheck is a linearization point,
    /// not a promise about the whole window. A waiter dropping after the recheck observes it, while
    /// the ticket is being spent, does not stop the read -- and cannot, since that is the boundary
    /// past which the source owns the work.)
    ///
    /// The defect: `WaiterTicket::drop` updates the authoritative waiter count under the lock and
    /// sends its notification AFTER releasing it (deliberately -- unit D's "no wake-into-contention"
    /// property). Admission resolving inside that gap left the fetch task's phase-1 select with
    /// nothing to report, so it went straight on to spend the ticket: one physical read for a block
    /// nobody was waiting for.
    ///
    /// Constructed rather than raced, in this file's own established style (see the deterministic
    /// constructions below): the state the race produces is "waiter count is zero AND this task's
    /// `waiterless` receiver has seen no change", and a receiver subscribed AFTER the bump is
    /// seeded exactly that way. That is the same state, reached on purpose instead of by luck --
    /// which matters because the real window has no `.await` in it and so cannot be hit on a
    /// single-threaded runtime at all.
    #[tokio::test]
    async fn admission_winning_the_abandon_race_still_starts_no_read() {
        let (src, c) = cache(b"0123456789", 4, 64);
        let inner = c.inner.clone();
        let (ticket, completion, _minted_rx) = {
            let mut st = inner.lock_state();
            st.in_flight.register(0, &inner, Acc::Empty)
        };
        drop(ticket);
        assert_eq!(
            *completion.slot.waiterless.borrow(),
            1,
            "precondition: the count really did reach zero and the bump was sent"
        );
        // subscribed AFTER that bump, so it is seeded as already caught up and its `changed()` can
        // never fire -- the fetch task sees no signal at all while the count is zero, which is
        // exactly what the losing side of the real race observes. (Subscribing BEFORE the drop
        // would build the ordinary abandon path instead, which phase 1 has always handled.)
        let stale_rx = completion.slot.waiterless.subscribe();
        assert!(
            !stale_rx.has_changed().unwrap(),
            "precondition: this receiver reports nothing to act on"
        );
        tokio::spawn(BlockCache::fetch(inner.clone(), 0, completion, stale_rx))
            .await
            .unwrap();
        assert_eq!(
            src.read_count(),
            0,
            "the commit point re-derives the decision from the COUNT after admission, so a fetch \
             nobody is waiting for spends no ticket -- an unspent ticket costs nothing, which is \
             the whole reason phase 1 is allowed to abandon at all"
        );
        assert_eq!(
            c.in_flight_len(),
            0,
            "and it ended its registration on the way out"
        );
    }
    /// batch 16 (2026-07-31), finding #3. **A generation that read nothing does not resurrect an
    /// entry the cache has already evicted.**
    ///
    /// The defect: `check_out` leaves `Entry::CheckedOut` in the slot. If churn evicts that marker
    /// while the fetch is in flight, and the fetch then ends having added no bytes of its own,
    /// handing the unchanged accumulator back reinstated an entry the cache had decided to drop --
    /// and evicted something newer to make room for it. Giving back what this generation was handed
    /// is only worth an eviction if this generation improved on it.
    #[tokio::test]
    async fn a_zero_work_abandon_does_not_resurrect_an_evicted_entry() {
        let (_, c) = cache(b"0123456789", 4, 64);
        let mut st = c.inner.lock_state();
        // an unfinished entry, then checked out by a fetch.
        let mut partial = Acc::Empty;
        partial.push(Bytes::from_static(b"ab"));
        Inner::store_partial(&mut st, 0, partial, true);
        let taken = Inner::check_out(&mut st, 0);
        assert_eq!(taken.len(), 2, "precondition: the fetch holds the bytes");
        assert!(
            matches!(st.probation.peek(&0), Some(Entry::CheckedOut)),
            "precondition: the slot is marked"
        );
        // churn evicts the marker while the fetch is still in flight.
        st.probation.pop(&0);
        assert!(st.probation.peek(&0).is_none() && st.protected.peek(&0).is_none());
        // the fetch abandons having read nothing of its own.
        Inner::store_partial(&mut st, 0, taken, false);
        assert!(
            st.probation.peek(&0).is_none() && st.protected.peek(&0).is_none(),
            "an evicted slot must stay evicted when the generation that held its bytes added \
             nothing -- reinstating it costs a live entry its place for no new information"
        );
        // and the same hand-back IS kept while the marker is still there, which is the case it
        // exists for: the entry is the cache's, on loan.
        let mut partial = Acc::Empty;
        partial.push(Bytes::from_static(b"cd"));
        Inner::store_partial(&mut st, 1, partial, true);
        let taken = Inner::check_out(&mut st, 1);
        Inner::store_partial(&mut st, 1, taken, false);
        assert!(
            matches!(st.probation.peek(&1), Some(Entry::Partial(_))),
            "a marked slot gets its bytes back even after zero work -- they were never the \
             fetch's to drop"
        );
    }
    /// batch 15 (2026-07-30), finding #3's structural half. **Checking the accumulator out leaves a
    /// marker, never a slot that looks like a finished block.**
    ///
    /// `Entry::CheckedOut` is what makes the take safe: an emptied slot that could be read as a
    /// zero-length block would be indistinguishable from "this block is past EOF". The state is
    /// only ever entered under the same lock that registers the fetch, so a caller finding it has
    /// something to join.
    #[tokio::test]
    async fn checking_out_an_accumulator_leaves_a_marker_not_an_empty_block() {
        let (_, c) = cache(b"0123456789", 4, 64);
        {
            let mut st = c.inner.lock_state();
            Inner::store_partial(
                &mut st,
                7,
                {
                    let mut a = Acc::Empty;
                    a.push(Bytes::from_static(b"ab"));
                    a
                },
                true,
            );
            assert!(
                matches!(st.probation.peek(&7), Some(Entry::Partial(_))),
                "precondition: unfinished progress is in the cache"
            );
            let taken = Inner::check_out(&mut st, 7);
            assert_eq!(taken.len(), 2, "the fetch receives the bytes themselves");
            assert!(
                matches!(st.probation.peek(&7), Some(Entry::CheckedOut)),
                "and the slot says so -- an emptied `Partial` would read as a zero-length block, \
                 which `Inner::is_resolved` treats as past EOF"
            );
            assert!(
                st.probation.peek(&7).and_then(Entry::resolved).is_none(),
                "and it is never mistakable for an answer"
            );
        }
        // the ordinary path still works around it.
        assert_eq!(&c.block(1).await.unwrap()[..], b"4567");
    }
    /// batch 14 (2026-07-30), finding #4's positive law. **Completing a short block coalesces
    /// exactly as a fresh fetch does: one read path per block, always.**
    ///
    /// Batch 13's `racing_refills_converge_instead_of_overwriting_each_other` is deleted and
    /// replaced by this: its whole premise -- two callers independently refilling the same block,
    /// converging by a monotone merge -- was a consequence of the completion living OUTSIDE
    /// `in_flight`, and the race it staged can no longer be constructed at all. Batch 14
    /// moved it in, so the race that test staged can no longer be constructed: the second caller
    /// joins the first's registration instead of starting a second read path. The property that
    /// mattered survives in a stronger form and is asserted here directly.
    #[tokio::test]
    async fn concurrent_callers_share_one_completion_of_a_short_block() {
        let reads = Arc::new(AtomicU64::new(0));
        let c = Arc::new(BlockCache::new(
            Arc::new(ShortFirstAnswerPerBlock {
                data: Bytes::from_static(b"\nabczef"),
                block_size: 16,
                claimed: 16,
                reads: reads.clone(),
            }),
            16,
            1 << 20,
        ));
        let mut coalesced = c.coalesced_events();
        let (a, b) = tokio::join!(
            {
                let c = c.clone();
                tokio::spawn(async move { c.block(0).await.map(|b| b.to_vec()) })
            },
            {
                let c = c.clone();
                tokio::spawn(async move { c.block(0).await.map(|b| b.to_vec()) })
            },
        );
        let (a, b) = (a.unwrap().unwrap(), b.unwrap().unwrap());
        assert_eq!(&a[..], b"\nabczef", "first caller");
        assert_eq!(&b[..], b"\nabczef", "second caller");
        assert_eq!(
            reads.load(Ordering::SeqCst),
            3,
            "ONE read path: the short first answer, the completion that gathers the rest, and the \
             read past 7 that certifies the end. Two independent completions would double the \
             last two"
        );
        wait_for_count(&mut coalesced, |n| n >= 1).await;
        assert!(
            c.block(0).await.unwrap().ends_data(),
            "and the certificate the shared completion earned is on the entry"
        );
    }
    /// batch 14 (2026-07-30), finding #3. **An entry never moves backward -- not even when a fresh
    /// whole-block fetch answers with a legal short prefix of what the cache already holds.**
    ///
    /// The defect: `FetchCompletion::publish` wrote its own snapshot with a bare `put`, bypassing
    /// `absorb`. A completion could extend block 0 to `ABCD` while a concurrent fresh fetch was
    /// still in flight; that fetch then published the `A` its own first read had returned, and every
    /// later read had to rediscover the rest -- or, with the source no longer willing, failed.
    ///
    /// The two writes are staged directly rather than raced, because the merge is what is under
    /// test and a race would only make the same call less certainly.
    #[tokio::test]
    async fn a_short_fresh_publish_cannot_roll_an_entry_backward() {
        let reads = Arc::new(AtomicU64::new(0));
        let c = BlockCache::new(
            Arc::new(ShortFirstAnswerPerBlock {
                data: Bytes::from_static(b"ABCDEFGH"),
                block_size: 8,
                claimed: 8,
                reads: reads.clone(),
            }),
            8,
            1 << 20,
        );
        assert_eq!(&c.block(0).await.unwrap()[..], b"ABCDEFGH");
        // exactly what a fresh fetch's own first answer looks like for this source -- staged at
        // the boundary that used to write it, since with completion inside `in_flight` there is no
        // longer a way to get two writers for one index through the public API at all. (That is the
        // structural half of the fix; this is the invariant it rests on, still asserted directly
        // because an abandoned fetch's own late partial-absorb can still land after a later fetch
        // has published -- a real overlap, just not one a test can schedule deterministically.)
        let (_ticket, completion, _waiterless) = {
            let mut st = c.inner.lock_state();
            st.in_flight.register(0, &c.inner, Acc::Empty)
        };
        completion.publish(Ok(Block::uncertified(Bytes::from_static(b"A"))));
        assert_eq!(
            &c.block(0).await.unwrap()[..],
            b"ABCDEFGH",
            "publishing a legal short prefix must not shrink the entry -- with a bare `put` here, \
             every later read had to rediscover the rest, or failed outright against a source no \
             longer willing to answer"
        );
        // and the merge is the one path, so the writer is handed back what the cache holds rather
        // than its own snapshot: it cannot publish less than that to its own waiters either.
        let merged = c
            .inner
            .absorb(0, Block::uncertified(Bytes::from_static(b"AB")));
        assert_eq!(&merged[..], b"ABCDEFGH");
    }
    /// batch 14 (2026-07-30), finding #4. **Aborting a requester cannot discard a read the source
    /// has already committed to, strand its permit, or force a duplicate.**
    ///
    /// The defect: the completion loop read inside the REQUESTER's own future. Past `admit` a read
    /// is committed -- past `read` a caller may not count on cancellation preventing it
    /// (`source.rs`) -- so an aborted search left the read running, its result discarded, its permit
    /// held. Two of those filled a two-permit source, blocked a third request, and cost three
    /// identical physical reads, contradicting the coalescing guarantee `docs/block_cache.md`
    /// states. The read now lives in the detached fetch task, which no requester owns.
    #[tokio::test]
    async fn aborting_a_requester_neither_discards_nor_duplicates_a_committed_read() {
        let reads = Arc::new(AtomicU64::new(0));
        let started = Arc::new(AtomicU64::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        struct HoldsTheFirstRead {
            reads: Arc<AtomicU64>,
            started: Arc<AtomicU64>,
            release: Arc<tokio::sync::Semaphore>,
        }
        #[async_trait::async_trait]
        impl BlockSource for HoldsTheFirstRead {
            fn size(&self) -> u64 {
                8
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let reads = self.reads.clone();
                let started = self.started.clone();
                let release = self.release.clone();
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        // COMMITTED from here on: the ticket is spent, so this read happens whatever
                        // becomes of whoever asked for it.
                        started.fetch_add(1, Ordering::SeqCst);
                        release
                            .acquire()
                            .await
                            .expect("the gate is never closed")
                            .forget();
                        reads.fetch_add(1, Ordering::SeqCst);
                        let data: &[u8] = b"ABCDEFGH";
                        if offset >= data.len() as u64 {
                            return Ok(Bytes::new());
                        }
                        let start = offset as usize;
                        Ok(Bytes::from_static(data).slice(start..(start + len).min(data.len())))
                    })
                })
            }
        }
        let c = Arc::new(BlockCache::new(
            Arc::new(HoldsTheFirstRead {
                reads: reads.clone(),
                started: started.clone(),
                release: release.clone(),
            }),
            8,
            1 << 20,
        ));
        let h = {
            let c = c.clone();
            tokio::spawn(async move { c.block(0).await.map(|b| b.to_vec()) })
        };
        // wait until the read is genuinely committed, then abort the only requester.
        wait_for_counter(&started, 1).await;
        h.abort();
        let _ = h.await;
        release.add_permits(8);
        assert_eq!(
            &c.block(0).await.unwrap()[..],
            b"ABCDEFGH",
            "the block resolves normally after its only requester walked away"
        );
        assert_eq!(
            reads.load(Ordering::SeqCst),
            1,
            "and the committed read was KEPT, not discarded and redone: one physical read serves \
             the whole 8-byte block. Owning that read inside the aborted requester's future made \
             it three"
        );
    }
    /// batch 14 (2026-07-30), finding #5. **A dribbling completion yields, so it cannot starve the
    /// executor it runs on.**
    ///
    /// The defect: every `.await` in the completion loop resolves without yielding for a source
    /// that answers from memory, so a legal one-byte source ran `block_size` passes inside a single
    /// poll -- 4,095 reads for a 4 KiB block, and at the default 1 MiB block size a loop no other
    /// task on that worker could interleave with. (The same loop also rebuilt the accumulated prefix
    /// on every pass to republish it; that is now one buffer frozen once, which this test cannot
    /// observe directly but which the single-threaded interleaving below depends on staying cheap.)
    #[tokio::test(flavor = "current_thread")]
    async fn a_dribbling_completion_lets_other_tasks_run() {
        let data: &'static [u8] = Box::leak(vec![b'x'; 4096].into_boxed_slice());
        let reads = Arc::new(AtomicU64::new(0));
        let c = Arc::new(BlockCache::new(
            Arc::new(OneBytePerRead {
                data: Bytes::from_static(data),
                reads: reads.clone(),
            }),
            4096,
            1 << 20,
        ));
        // a plain counting task on the SAME single-threaded runtime: if the completion loop never
        // yields, this cannot advance at all while the block is being filled.
        let ticks = Arc::new(AtomicU64::new(0));
        let spinner = {
            let ticks = ticks.clone();
            tokio::spawn(async move {
                loop {
                    ticks.fetch_add(1, Ordering::SeqCst);
                    tokio::task::yield_now().await;
                }
            })
        };
        let block = c.block(0).await.unwrap();
        spinner.abort();
        assert_eq!(block.len(), 4096, "the block completed");
        assert!(
            ticks.load(Ordering::SeqCst) > 1,
            "the other task must have run DURING the 4096-pass completion, not only before it; \
             ticked {} times",
            ticks.load(Ordering::SeqCst)
        );
    }
    /// A conforming source that answers exactly one byte per read -- `BlockSource`'s "up to `len`"
    /// contract permits it and `size()` is accurate.
    struct OneBytePerRead {
        data: Bytes,
        reads: Arc<AtomicU64>,
    }
    #[async_trait::async_trait]
    impl BlockSource for OneBytePerRead {
        fn size(&self) -> u64 {
            self.data.len() as u64
        }
        async fn admit(&self) -> crate::source::ReadTicket {
            let data = self.data.clone();
            let reads = self.reads.clone();
            crate::source::ReadTicket::from_fn(move |offset, _len| {
                Box::pin(async move {
                    reads.fetch_add(1, Ordering::SeqCst);
                    if offset >= data.len() as u64 {
                        return Ok(Bytes::new());
                    }
                    Ok(data.slice(offset as usize..offset as usize + 1))
                })
            })
        }
    }
    /// batch 13 (2026-07-29), finding #2. **A cancelled refill costs nothing and repeats nothing.**
    ///
    /// The defect: batch 12 spent a per-block LIFETIME attempt counter before `admit`, so a search
    /// aborted while parked in admission burned an attempt without any read happening. Four aborted
    /// searches exhausted a block's whole budget and the fifth answered `Exhausted` over a file
    /// whose tail had never once been re-asked. Retiring the cap removes the counter; this asserts
    /// the property the counter broke.
    ///
    /// Also pins the durability half the uncapped loop rests on. **How that works changed in batch
    /// 15 (2026-07-30) and this comment lagged it (batch 17 (2026-07-31), finding #3):** passes do
    /// NOT publish as they go -- republishing the accumulated prefix on every pass is exactly the
    /// quadratic copying batch 15 removed. Progress is durable because the accumulator belongs to
    /// the cache and every ENDING hands it back, including the abandon this test drives.
    #[tokio::test]
    async fn aborted_refills_neither_consume_progress_nor_block_a_later_one() {
        let admits = Arc::new(AtomicU64::new(0));
        let reads = Arc::new(AtomicU64::new(0));
        // ONE permit: block 0's initial fetch gets through and delivers a single byte; every refill
        // admission after that parks, which is where each aborted attempt below dies.
        let gate = Arc::new(tokio::sync::Semaphore::new(1));
        let c = Arc::new(BlockCache::new(
            Arc::new(DribblesWhenAdmitted {
                admits: admits.clone(),
                reads: reads.clone(),
                gate: gate.clone(),
                size: 8,
            }),
            8,
            1 << 20,
        ));
        let abort_a_parked_refill = |expected_admits: u64| {
            let c = c.clone();
            let admits = admits.clone();
            async move {
                let h = tokio::spawn(async move { c.block(0).await.map(|b| b.len()) });
                wait_for_counter(&admits, expected_admits).await;
                h.abort();
                let _ = h.await;
            }
        };
        // the reported repro: four searches aborted while admission was parked.
        for nth in 0..4 {
            abort_a_parked_refill(2 + nth).await;
        }
        assert_eq!(
            reads.load(Ordering::SeqCst),
            1,
            "the reviewer's own measurement: those four attempts performed ZERO tail reads -- only \
             block 0's original fetch has read anything. Under batch 12 they nonetheless spent the \
             block's whole lifetime budget, and the fifth attempt answered Exhausted"
        );
        // let two refill passes through, then abort a third mid-way: the two bytes they added must
        // SURVIVE the abort, which is what makes an uncapped loop safe (a cancelled refill resumes
        // rather than restarting).
        gate.add_permits(2);
        abort_a_parked_refill(8).await;
        assert_eq!(
            reads.load(Ordering::SeqCst),
            3,
            "one original fetch plus the two admitted refill passes"
        );
        // now let the rest through: the block completes, and every byte was read exactly ONCE across
        // all five aborted attempts -- no pass ever redone.
        gate.add_permits(64);
        assert_eq!(
            &c.block(0).await.unwrap()[..],
            b"xxxxxxxx",
            "the refill budget was never a budget: the block completes on the next real attempt"
        );
        assert_eq!(
            reads.load(Ordering::SeqCst),
            8,
            "8 bytes, 8 reads: the aborted attempts left durable progress behind, so nothing was \
             re-read on the way to a complete block"
        );
    }
    /// batch 13 (2026-07-29), findings #2 and #5. **Refill state is scoped to the cache ENTRY, so it
    /// can neither outgrow the bounded segments nor outlive the bytes it describes.**
    ///
    /// The defect: `confirmed_final: HashSet<u64>` and `refill_attempts: HashMap<u64, u32>` were
    /// keyed by block index and never pruned -- a sequential source whose first answer per block is
    /// short grew both with file size, behind two LRUs whose whole purpose is to be bounded, and
    /// state from an evicted generation then applied to the fresh fetch that replaced it.
    #[tokio::test]
    async fn refill_state_never_outgrows_the_bounded_segments() {
        // 4 blocks of capacity over a 64-block file whose every block answers short first and is
        // then completed by one refill: batch 12 ended a walk like this holding 64 entries in each
        // side map.
        let data: &'static [u8] = Box::leak(vec![b'x'; 64 * 4].into_boxed_slice());
        let reads = Arc::new(AtomicU64::new(0));
        let c = BlockCache::new(
            Arc::new(ShortFirstAnswerPerBlock {
                data: Bytes::from_static(data),
                block_size: 4,
                claimed: (64 * 4) as u64,
                reads: reads.clone(),
            }),
            4,
            16,
        );
        for idx in 0..64u64 {
            assert_eq!(
                c.block(idx).await.unwrap().len(),
                4,
                "every block completes: the first answer is short, the refill closes it"
            );
        }
        assert!(
            c.tracked_blocks() <= 4,
            "the cache must remember nothing about a block it no longer holds; it tracked {} \
             entries across a 64-block walk with 4 blocks of capacity",
            c.tracked_blocks()
        );
        // the other half: an evicted block's certificate is not remembered either. Block 0 is long
        // gone, so re-asking it costs fresh reads rather than replaying a previous generation's
        // findings -- which is the whole reason the state belongs to the entry.
        let before = reads.load(Ordering::SeqCst);
        assert_eq!(c.block(0).await.unwrap().len(), 4);
        assert!(
            reads.load(Ordering::SeqCst) > before,
            "block 0 was evicted, so its refetch must really re-read"
        );
    }
    /// batch 13 (2026-07-29), finding #6. **A refill must not refresh recency -- not even in the
    /// segment the block already sits in.**
    ///
    /// The defect: batch 12's refill wrote with `LruCache::put`, which moves an EXISTING key to
    /// most-recently-used. A `warm()`-driven refill (prefetch, the background index scan) therefore
    /// refreshed a protected block's recency and could evict the interactive working set --
    /// contradicting the consumer-truthful promotion rule `docs/block_cache.md` states and `get_raw`
    /// already enforces with `peek`.
    ///
    /// Reaching a `warm()` refill of an already-PROTECTED block takes the one path that leaves a
    /// short block unresolved in the cache: an aborted refill's own durable partial progress.
    #[tokio::test]
    async fn a_warm_refill_does_not_refresh_protected_recency() {
        let admits = Arc::new(AtomicU64::new(0));
        let gate = Arc::new(tokio::sync::Semaphore::new(1));
        // 4 blocks of capacity: probation 1, protected 3.
        let c = Arc::new(BlockCache::new(
            Arc::new(DribblesWhenAdmitted {
                admits: admits.clone(),
                reads: Arc::new(AtomicU64::new(0)),
                gate: gate.clone(),
                size: 16,
            }),
            4,
            16,
        ));
        // block 0: fetch its single byte, then abort the refill twice. The second call promotes it
        // out of probation BEFORE refilling, so it ends up protected, short and unresolved -- the
        // state a later `warm()` will want to complete.
        let mut expected = 1;
        for _ in 0..2 {
            let h = {
                let c = c.clone();
                tokio::spawn(async move { c.block(0).await.map(|b| b.len()) })
            };
            expected += 1;
            wait_for_counter(&admits, expected).await;
            h.abort();
            let _ = h.await;
        }
        gate.add_permits(256);
        // blocks 1 and 2, twice each, fill the rest of `protected`; block 0 becomes its OLDEST entry.
        for idx in [1u64, 2] {
            let _ = c.block(idx).await.unwrap();
            let _ = c.block(idx).await.unwrap();
        }
        assert_eq!(
            c.protected_keys(),
            vec![2, 1, 0],
            "precondition: block 0 protected and least-recently-used"
        );
        let warmed = c.warm(0).await.unwrap();
        assert_eq!(
            warmed.len(),
            4,
            "the warm DID refill -- otherwise this proves nothing about the write it makes"
        );
        assert_eq!(
            c.protected_keys(),
            vec![2, 1, 0],
            "and that write left recency alone: with `put` here, block 0 would have jumped to \
             most-recently-used and the next fill would evict the interactive working set instead"
        );
    }
}
