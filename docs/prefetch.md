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
- A fill already **inside its own blocking read** — past `cache.warm` and
  into `PreadSource::read_block`'s `spawn_blocking` closure — is a third
  case, not a variant of the first two: tokio documents `spawn_blocking`
  tasks as uncancellable once running, so the blocking positioned read
  completes on its own (or, on a wedged network mount, hangs on its own)
  regardless of the abort. Only the outer wrapper task, and whatever value
  it would have produced, is gone.

The third case is bounded, not open-ended: at most **`FILL_CONCURRENCY`**
(4) fills can hold a semaphore permit — and so be actively reading — at
once, so a drop leaves at most that many single-block reads outstanding,
real-file sources only (`MockSource`'s own simulated latency is a plain
async sleep, fully abortable — this residual is specific to a real
`PreadSource`). The binary accounts for this at shutdown rather than
assuming it away: the runtime itself is torn down with a short timeout
after the event loop returns, so quitting abandons any stuck background
fills rather than waiting for a filesystem that may never answer.
