# Block cache

Every byte the engine reads flows through one shared `BlockCache`
(`ress-core/src/cache.rs`). The viewport, navigation scans, prefetch, and
the background line-index scan all read the same fixed-size,
block-aligned `Bytes` — so a block fetched for any reason serves every
consumer. Future analyzers (search, syntax highlighting) are designed to
share the same path.

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

## In-flight coalescing

Concurrent requests for the same block share one physical read (exactly one,
as long as the fetch is not cancelled mid-read — see below). The first
requester becomes the fetcher and registers a watch channel; every other
caller clones the receiver and waits for the published result. This makes
duplicate work free by design — overlapping prefetch windows, simultaneous
scans, and viewport reads of the same region all collapse into single reads.

Two properties of the protocol are load-bearing:

- **Cancellation-safe.** A fetcher's future can be dropped mid-read (an
  aborted prefetch, a cancelled jump). The dropped sender wakes the waiters,
  the first one cleans up the stale registration (guarded so a fresh
  fetcher's registration is never clobbered), and retries as the new
  fetcher. No interleaving can wedge a block. The abandoned blocking read
  cannot be interrupted and may still complete in the background, so a
  cancellation can briefly cost a duplicate physical read — the accepted
  price of never wedging.
- **Errors keep their chain.** Read errors are not cached; the fetcher's
  caller and every coalesced waiter receive the full error chain (shared
  behind an `Arc`), so diagnostics from a flaky network mount survive intact
  everywhere they surface.

## Locking

Cache state sits behind a `std::sync::Mutex` with short, synchronous
critical sections; the physical read happens with no lock held. See the
concurrency rules in [architecture.md](architecture.md).
