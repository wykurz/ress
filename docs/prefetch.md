# Prefetch

The goal: scrolling rarely waits on a cold read. The `Prefetcher`
(`ress-core/src/prefetch.rs`) watches viewport anchors as the user moves,
infers the scroll direction, and warms the next blocks in the shared
[block cache](block_cache.md) — so in the typical case the viewport reaches
already-resident blocks. It is deliberately best-effort, not a guarantee:
fills are background tasks that a fast scroll can outrun, and the bounds
below intentionally skip work rather than let prefetch compete with the
interactive path.

## Direction inference

Direction is inferred from successive **anchor byte offsets**, not block
indices. Most scroll steps stay inside one 1 MiB block; comparing block
indices would leave a reversal undetected until the anchor crossed a block
boundary, making the first read after turning around cold — the common case
a prefetcher exists to prevent. Offset comparison flips the direction on the
first reversed step.

A jump (rather than a step) simply re-targets the window: prefetch fills are
per-block tasks, so there is nothing heavyweight to cancel — outstanding
single-block reads complete, land in the cache, and may still prove useful.

## Bounded and best-effort

Prefetch is designed to stay out of the interactive path's way. Three bounds
cap how much background work can exist at once — they limit contention with
foreground reads (which share the same source and blocking pool) rather than
eliminate it:

- **Concurrency**: fills acquire a small semaphore, so at most a few
  background reads are in flight regardless of depth.
- **Backlog**: on a slow source, redraws can outpace fill completion; new
  fills are skipped while the queued-task backlog exceeds a small multiple
  of the depth. Skipping is cheap — the cache's coalescing and residency
  make any skipped block inexpensive to fetch when actually needed.
- **Cache policy**: fills go through `warm()`, which never promotes — a
  prefetched-but-never-viewed block cannot displace the *promoted* working
  set. First-touch interactive blocks share the probationary segment with
  fills until their second reference, so a tight cache can still see churn
  there (see [block cache](block_cache.md)).

Failures are swallowed by design: a failed fill is retried by whichever
consumer actually needs the block, with the real error surfacing there.

Depth is tunable (`--prefetch-depth`, default 8; 0 disables prefetch
entirely).

## Shutdown

Fills run positioned reads on the blocking thread pool, and a read wedged on
a dead network mount cannot be aborted. The binary therefore bounds runtime
shutdown with a short timeout: quitting abandons stuck background fills
rather than waiting for a filesystem that may never answer.
