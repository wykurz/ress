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
That last clause became true only with batch 4 (2026-07-24)'s finding #11:
before it, a cancelled fill's read could still complete at the OS level
(the same uncancellable-pread reality below), but nothing was left to
write the result into the cache — the async fn carrying that logic had
already been dropped past the point of its own read. See [block
cache](block_cache.md)'s "In-flight coalescing" for why the fetch now
survives and publishes regardless.

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

## Cancellation and shutdown

Every fill is tracked in a `TaskOwner` the `Prefetcher` owns, never detached
— a thin `JoinSet` wrapper kept only for `JoinSet`'s own cancel-on-drop
`Drop` (see `task_owner.rs`'s own doc comment): dropping the `Prefetcher` —
which happens exactly when its `Document` does, since it is a plain field,
never shared — drops that `TaskOwner`, which aborts every handle its
`JoinSet` holds. This is the same cancel-on-drop guarantee
`ScanScheduler` and `StatusWorker` give their own background task (see
[concurrency](concurrency.md)); prefetch joins only that ownership half of
the idiom, since a fill has no answer to publish back through a `watch`
channel the way those two do.

What that abort actually reaches is three layers, not one uniform "aborts
everything" (verified against tokio's own source, not assumed — `tokio::
task::JoinSet::drop`'s doc, `spawn_blocking`'s own cancellation doc):

- A fill still **queued** on the concurrency semaphore — never having
  started a read at all — dies outright.
- A fill **suspended at an ordinary `.await`** (the semaphore acquire
  itself) dies the same way, at that await point.
- A fill whose `cache.warm()` call has already become (or joined) an
  in-flight [block cache](block_cache.md) fetch is a third case, not a
  variant of the first two (fix round 1, F2, batch 4 (2026-07-24), finding
  #11: the boundary moved) — and the boundary is that registration, not
  entry into `PreadSource::admit`'s ticket, spent into its own
  `spawn_blocking` closure: the fetch runs detached from the fill that
  started it, so aborting the fill's own task no longer reaches it,
  regardless of whether the underlying OS read has even begun. Dropping the
  fill drops only its own `WaiterTicket` (restructure R2, 2026-07-27) — one
  fewer thing interested in the block, never a signal reaching the fetch
  itself directly. What happens to the fetch then depends on whether it has
  spent its ticket yet, and either outcome is
  correct, not merely tolerated: if the fetch has already **spent** it
  (admission alone is one step short — it takes a final look at the waiter
  count first), it still completes and publishes into the
  cache regardless of who is left to receive it, so a later consumer of
  that block gets a hit instead of a cold read — unchanged from before this
  restructure. If instead the fill's own drop was the fetch's *last*
  interested party and the read has not yet been admitted, the fetch now
  recognizes that and dies unstarted — deregistering without reading at all
  (batch 5, finding #4) — rather than the pre-R2 behavior of still queuing
  and eventually performing a read nobody was left to want. This is the
  point, not a regression: an abandoned fill that never got as far as an
  actual OS read now costs nothing instead of one wasted read. Between
  admission resolving and the spend there is one further look, and like the
  first it is a **linearization point** rather than a promise about the
  window around it: a departure it observes stops the read, one landing
  while the ticket is being spent does not. [block cache](block_cache.md)'s
  waiterless-abandon bullet is the authority on that contract; the other
  descriptions of this in the tree defer to it rather than restating it.

The third case is bounded, not open-ended: at most **`FILL_CONCURRENCY`**
(4) fills can hold a semaphore permit — and so have crossed into that
third case — at once, so a drop leaves at most that many single-block
fetches outstanding (fewer once any of them were still unadmitted at the
moment of the drop), for any source (a `MockSource` fetch now survives a
fill's abort exactly the same way a real `PreadSource` one does — both
route through the identical detached-fetch path in the shared cache; this
is no longer a real-files-only residual). The binary accounts for stuck
real-file reads at shutdown rather than assuming them away: the runtime
itself is torn down with a short timeout after the event loop returns, so
quitting abandons any stuck background fills — cache-owned fetches
included — rather than waiting for a filesystem that may never answer.
