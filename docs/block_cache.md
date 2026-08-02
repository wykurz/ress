# Block cache

Every byte the engine reads flows through one shared `BlockCache`
(`ress-core/src/cache.rs`). The viewport, navigation scans, prefetch, and
the background line-index scan all read the same fixed-size,
block-aligned `Bytes` — so a block fetched for any reason serves every
consumer. The background match-summary sweep (`SweepAnalysis`, see
[search](search.md)) shares the same path too — a real analyzer now, not a
future one; syntax highlighting remains the one still-future analyzer
designed to share it.

## Shape

Blocks are keyed by index (`offset / block_size`, 1 MiB by default) and
capacity is counted in whole blocks against a byte budget (256 MiB by
default, `--cache-mib`). The block count is clamped so a tiny block size
against a large capacity cannot eagerly allocate an enormous map.

Values are `Bytes`: refcounted slices. A consumer holding a block's bytes is
unaffected by eviction — eviction drops the cache's reference, nothing
dangles. This is why the design needs no explicit "pin the viewport's
blocks" mechanism: safety comes from refcounting, and retention of the hot
set comes from the eviction policy below.

## Scan-resistant eviction (SLRU)

The cache is split into two LRU segments:

- **probationary** (~¼ of capacity): every first-touch block lands here.
- **protected** (~¾): a block is promoted here when it is *re-referenced*.
  Overflow from protected demotes back into probationary rather than
  evicting outright.

A one-pass streaming scan (the background line-index scan, or simply
paging through a file once) touches each block once, so its blocks live
and die in probation and can never evict the **promoted** working set —
the blocks a consumer has actually come back to. First-touch interactive
blocks compete in probation like everything else until their second
touch; that window is the price of scan resistance, kept small by
promotion happening on the very next real reference. ARC was considered
and rejected — historically patent-encumbered, a poor fit for an MIT
tool — and SLRU delivers the property that matters here with two plain
LRUs.

### Promotion is consumer-truthful

"Re-reference" means a *real consumer* touched the block again — never the
machinery:

- Interactive reads use `block()`, which promotes on a probationary hit.
- Prefetch and the background line-index scan both use `warm()`, which
  fills but **never promotes** or refreshes an already-protected block's
  recency. Without this split, redraws, the natural overlap of successive
  prefetch windows, and a full-file index pass would all re-touch
  probationary blocks and push never-viewed data into the protected
  segment, evicting what the user is actually looking at.
- The outcome must not depend on race timing: an interactive read that
  coalesces with an in-flight prefetch fill promotes just like one that
  arrives after the fill — including when churn evicts the entry in the
  window between the fill publishing and the waiter waking. Whenever a
  prefetch fill precedes or overlaps the display, fill + display lands
  protected. (The one different-looking case is consistent, not an
  exception: if the display itself initiates the fetch and the prefetch
  arrives second as a waiter, the display was the block's *first* touch —
  it lands probationary and the next real reference promotes it.)
- A **refill** is not a touch at all. Completing a short block (below)
  rewrites an entry's bytes in place, and that write must be invisible to
  eviction order — whichever touch triggered it already had its promotion
  decided. `LruCache::put` on an existing key moves it to
  most-recently-used, so batch 12's refill silently refreshed a protected
  block's recency and could evict the interactive working set from a
  `warm()` call; batch 13 (2026-07-29) writes through `peek_mut` instead.

## Completing a short block

`BlockSource` promises only "up to `len`" bytes, so a conforming source may
answer short for reasons unrelated to EOF — and a block-indexed cache
memoising that answer turns a transient shortfall into a permanent hole
that nothing downstream can tell apart from the end of the file. The cache
closes it by asking: the fetch task re-reads from where the bytes stop until
the block is full, or the source answers **empty** — which certifies that its
real data ends exactly there — or the file's own **claimed size** ends inside
the block, which leaves nothing further to fetch but earns no certificate,
since nobody asked the source about it. Those three, plus a wholly empty
block, are `Inner::is_resolved`, the single definition of "done". The third is
not an edge case: it is how every file whose real end is not block-aligned
finishes, and `FillOutcome::Budget`'s own doc comment (`scan.rs`) covers what
the missing certificate costs one layer up.

Five properties make this safe rather than merely helpful:

- **It is the same read path as a fresh fetch** (batch 14 (2026-07-30)).
  Completion is not a second mechanism: it is registered in `in_flight`,
  performed by the detached fetch task, and published once — so it
  coalesces (N callers finding one short block share one read path), it
  survives its requesters (aborting a search cannot discard a read the
  source has already committed to, strand its permit, or force a
  duplicate), and it publishes through the same monotone `absorb` every
  other write uses. Batch 12 gave it a hand-rolled loop in the requester's
  own future and all three of those went wrong.
- **Every block leaving the cache is resolved.** The loop has no attempt
  cap (batch 13 (2026-07-29) retired batch 12's), so it is bounded by the
  hole's own width — `block_size` reads at worst, for a source that
  dribbles — and never by a give-up. A hit is only an answer if it is
  resolved; an entry left short by an abandoned fetch is resumed, never
  handed out.
- **Cancellation costs nothing and repeats nothing, for as long as the
  progress stays resident.** A dropped requester does not stop the fetch. If
  *every* requester leaves and one of the fetch's own abandon checks sees
  that (each is a linearization point, not a window-wide promise — the
  waiterless-abandon bullet below is the authority), it abandons at that
  pass boundary and hands the bytes it already paid for to the
  cache, so a later call resumes from them rather than starting over. The
  hand-over happens in the *same* critical section that deregisters, so
  there is no window in which a replacement can register and find nothing
  to resume from — and every ending accounts for its bytes, including the
  two nobody reaches deliberately: a read error keeps the passes that
  succeeded (propagating the error without caching it), and so does a fetch
  destroyed mid-read by a panic. The qualification is not a footnote: progress
  is cached data and cached data is evictable. The `added` rule governs the
  HAND-BACK paths only — an abandon, a read error, a teardown — where a
  generation that read nothing lets the unchanged bytes go rather than
  reinstating an entry the cache had already decided to drop, and the next
  request re-reads from the start. A fetch that RESOLVES publishes either
  way, including one that resolved on its first empty read: publishing is
  the answer, not a hand-back, and an answer is always worth an entry. See
  the accumulator bullet below.
- **The accumulator is the cache's, so assembly stays cheap.** A fetch
  *checks it out* (`Entry::CheckedOut`) rather than copying it, extends it,
  and hands it back; freezing to `Bytes` happens once, when the block
  resolves. Three claims, stated as precisely as they hold:
  - an **ordinary miss copies nothing** — one read answers the block and
    the bytes a consumer receives *are* the buffer the source returned;
  - **no per-generation prefix copy**: resuming after a cancellation moves
    the accumulator, it does not clone it (`Acc` is deliberately not
    `Clone`), so the observable cost of N cancellations is N resumptions
    and no re-reads — *for as long as the entry stays resident*. Progress is
    cached data, and cached data is evictable: a fetch checks its
    accumulator out (`Entry::CheckedOut`), and if churn evicts that marker
    while the fetch is in flight, a pass that added no bytes of its own and
    is **handing back** rather than answering deliberately lets the unchanged
    bytes go rather than reinstating an entry the cache had already decided
    to drop. The added-nothing rule is the hand-back paths' alone, exactly as
    the bullet above scopes it — a pass that RESOLVES publishes and reinstates
    the entry however few bytes it read, including none at all. Re-reading
    after an eviction is the ordinary cost of a cache, not a repeat;
  - **amortized linear assembly**: the accumulator grows geometrically, so
    building an N-byte block costs O(N) copying in total. Individual
    appends may still reallocate and move — that is the allocator's
    business, and no code here can promise otherwise. Seeding a fresh
    buffer from the cached prefix each time, which is what this replaced,
    cost one copy of that prefix *per generation*: 1+2+…+n, roughly
    512 GiB at a 1 MiB block size for a source cancelled once per byte.
- **The certificate belongs to a length, not to an index.** It lives in the
  entry alongside the bytes it describes (`Block::ends_data`), so it cannot
  be read for a different length, cannot outlive the bytes through
  eviction, and cannot be dropped by a shortcut that only passes bytes
  along. And an entry only ever moves *forward*: longer bytes, or the same
  bytes plus a certificate. That merge is what keeps a legal short prefix —
  from a fetch that raced an abandoned one's late hand-over, or from a file
  truncated under the pager — from rolling a completed entry backward.

## In-flight coalescing

Concurrent requests for the same block share exactly one **fetch** —
one registration, one detached reader — unconditionally, including across
supersession (batch 4 (2026-07-24), finding #11). Not necessarily one
physical *read*: completing a short block can take several (see above), and
a conforming source that dribbles can make that `block_size` of them. What
coalescing guarantees is that no second fetch is ever started for a block
one is already working on, which is the property duplicate work turns on. The fetch is a **detached publisher**: the first caller to
find a block neither cached nor already in flight registers it and spawns
the fetch as its own task; every caller, including that first one, is
symmetrically a **waiter** holding a `WaiterTicket` — an RAII proof of
interest whose `Drop` is the only thing that can decrement the registration's
waiter count, and which has no method reaching into the registry at all.
This makes duplicate work free by design — overlapping prefetch windows,
simultaneous scans, and viewport reads of the same region all collapse into
a single fetch, and so does a request replacing one that was already in
flight. (One fetch, not necessarily one read: see above.)

The fetch itself is built around a phase boundary the source makes a value
(`ReadTicket`, restructure R1; see [concurrency](concurrency.md)): the
commitment point is **spending** the ticket, not admission. A ticket that has
been admitted but not spent can still be dropped for nothing — no read has
started and the permit it holds is released on the spot — so the fetch task
takes the decision one last time *after* admission resolves and *before* it
spends (batch 16 (2026-07-31)). That recheck is the cache-level commit point,
and it is a real one: the waiter count is authoritative under the lock while
the notification that a waiter left is sent after the lock releases, so
admission resolving inside that gap would otherwise send a fetch into a read
nobody was waiting for. Once the ticket is spent the read commits,
cancellation or not.
Restructure R2 (2026-07-27) gives the fetch task two phases built on that
boundary, and three ways a registration can end, each stated once as a type
rather than left to a comment:

- **Publish (the normal ending).** The read completed — Ok or Err — and the
  result is sent to every waiter and, on success, landed in probation.
  Deregistering and (on success) landing the bytes in probation happen as
  one critical section; the send to waiters happens deliberately *after*
  that section releases the lock — so a fetch task is never woken into
  contention for the lock it just released, unit D's own reviewed property.
  This happens unconditionally, regardless of how many requesters (if any)
  are still waiting.
- **Waiterless abandon (a normal ending too, not an error).** The fetch
  takes this decision twice per pass — parked in admission, and again the
  instant admission resolves — and each is a **linearization point**: if the
  waiter count is zero *when it looks*, the fetch dies without reading. It is
  not a promise about the whole window, and the difference matters: a waiter
  that drops after the second look, while the ticket is being spent, does not
  stop the read. What the second look buys is that the count is read
  authoritatively (under the lock) rather than inferred from a notification
  that is deliberately sent after the lock releases — closing a gap in which
  admission could win and start a read for a block whose last waiter had
  already gone. On either look it deregisters and publishes
  *nothing* — there is nobody left to receive anything. This is new as of
  restructure R2: before it, an abandoned-but-unadmitted fetch still queued
  and eventually read, wasted work nobody wanted (batch 5, finding #4). The
  decision and the deregistration happen as one critical section
  specifically so a joiner arriving at the same moment can never be left
  waiting on a channel that will never publish — it either joins the still-
  live fetch, or the registration is already gone and it starts a genuinely
  fresh one. Once the ticket is **spent**, this ending is no longer
  reachable for that pass: the read completes and publishes (see the
  "cancellation-proof" bullet below). A multi-pass completion is therefore
  abandonable at every pass boundary and at no point inside one — the right
  granularity, since a dribbling source must not be able to pin a fetch to a
  block nobody wants anymore.
- **Involuntary teardown (the abnormal ending).** If the fetch task's own
  future is destroyed without reaching either ending above — a panic
  mid-read, or the whole runtime shutting down while it is still in
  flight — it deregisters **and publishes an error**. Before restructure
  R2, this case left a closed channel with no value, and a waiter's own
  self-heal-and-retry could spin forever against a source that
  deterministically panics (batch 5, finding #9); that retry path is now
  gone entirely from the waiting side. A panic becomes ordinary error
  propagation through the same contract every caller already handles (the
  sweep's `give_up` budget, nav's error surfacing), not a state nothing can
  interpret.

  Precisely why a *live* waiter can never observe a closed channel with no
  published value: not because publish-or-Drop are the only two endings —
  the waiterless abandon above is a third ending that also publishes
  nothing — but because every live waiter's own `WaiterTicket` holds a
  reference-counted handle to the same slot that owns the outcome sender,
  so the sender cannot be dropped while any `WaiterTicket` for that fetch
  still exists. The waiterless-abandon ending publishes nothing precisely
  *because* it only ever fires once every such reference is already gone —
  there is nobody left with a handle to observe the close either way.

Two properties of the protocol are load-bearing:

- **Cancellation-safe, and cancellation-proof.** A requester's own future
  can be dropped mid-wait (an aborted prefetch, a cancelled jump, a fresh
  search superseding a stale sweep) without disturbing the fetch it may
  have started: the registration lives until the fetch itself ends (one of
  the three ways above), not until any one requester's future ends, so a
  replacement request for the same block joins the still-running fetch
  instead of finding no registration and starting a second physical
  read — the stacking finding #11 fixed is structurally impossible, not
  merely unlikely (there is no method on a requester's own `WaiterTicket`
  that reaches the registry at all). A superseded caller's abandoned
  interest is not wasted work either, provided the read already committed:
  the fetch it started still completes and lands in the cache, so a later
  request for that block can be a hit. If instead every interested party
  leaves before the fetch's own last look at the waiter count — parked in
  admission, or at the recheck the instant admission resolves — it dies
  without reading rather than queuing a read nobody will collect. A
  departure landing after that look, while the ticket is being spent, is
  not noticed and does not stop the read; the look is a linearization
  point, not a promise about the whole window. No
  interleaving can wedge a block, and no interleaving can leave a waiter
  parked on a channel that will never resolve. The one read that genuinely
  cannot be interrupted is the blocking OS read itself, already handed to
  the source; see [concurrency](concurrency.md) for why that one fact — not
  cache-level cancellation — is what the read-concurrency bound exists to
  contain.
- **Errors keep their chain.** Read errors are not cached; the fetch's own
  caller and every coalesced waiter receive the full error chain (shared
  behind an `Arc`), so diagnostics from a flaky network mount survive intact
  everywhere they surface. A failed fetch still deregisters, so the next
  request is a genuinely fresh read, never permanently poisoned by one bad
  one.

## Locking

Cache state sits behind a `std::sync::Mutex` with short, synchronous
critical sections; the physical read happens with no lock held. See the
concurrency rules in [architecture.md](architecture.md).
