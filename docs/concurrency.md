# Concurrency

`ress` runs three independent background computations alongside the
interactive event loop: indexing a file's line structure, answering the
status line's current-line query, and completing a navigation that outran
its interactive budget. All three are built to one shape, described here
once rather than separately for each: one task, owned outright by whoever
needs its answer, consuming immutable inputs and publishing immutable
snapshots — never shared mutable state that two sides reach into.

## The shape

Each background computation is exactly one task, spawned and owned by
whoever asked for it — never detached, never left running after the
interest that started it is gone. Its outputs are immutable values
published through a `watch` channel (`tokio::sync::watch`), so any number
of readers see the latest one without contending a lock or missing an
update; consumers only ever read, never reach in and mutate what the task
itself owns.

- **The index scan** (`ScanScheduler`, `ress-core/src/schedule.rs`) reads a
  file once, sequentially, building a sparse line index and publishing its
  progress — a `Frontier`: bytes processed, lines found, done — after every
  block. The index itself grows too large to republish wholesale on every
  change, so it stays behind a `Mutex` only the scan task ever writes to;
  consumers lock it only to copy a value out, never to mutate it.
- **The status worker** (`StatusWorker`, `ress-core/src/status.rs`) answers
  "what line is this anchor on," one anchor at a time. It has both an
  inbound and an outbound channel: consumers send the anchor they currently
  care about through one `watch` channel and read the worker's answer — a
  small, `Copy` snapshot — from another, with no `Mutex` at all, since the
  whole answer fits in the channel's own value.
- **Pending navigation** (`PendingNav`, `ress-core/src/resolve.rs`, owned by
  the run loop's own slot in `ress/src/app.rs`) is spawned fresh for one
  navigation that could not finish inside its interactive budget,
  publishing bytes-scanned progress and resolving to the final anchor.
  Unlike the two document-lifetime workers above, it answers exactly one
  question and is then done: there is no inbound channel to re-target it,
  because a superseding motion drops it and spawns a new one instead.

Prefetch fills join half of this shape and stay deliberately outside the
other half. Many small, uncoordinated reads (never one task with a single
answer to publish) means there is no `watch` channel — a fill's only
observable effect is warming the shared cache, not a value a consumer reads
back, so there is nothing to publish. But every fill is still owned: they
live in one `TaskOwner` (a thin `JoinSet` wrapper kept only for
`JoinSet`'s own cancel-on-drop `Drop`) the `Prefetcher` holds, so dropping
the `Prefetcher` — which happens exactly when its `Document` does — drops
that `TaskOwner`, aborting every handle its `JoinSet` holds, the same
cancel-on-drop guarantee as the three background computations above — with
the same one blocking-read exception those three tasks share too (see
[prefetch](prefetch.md)'s own Cancellation section for the full
three-layer account and its `FILL_CONCURRENCY` bound, not restated here).

## Supersession is control flow

A fresher request to a document-lifetime worker is not a value to
reconcile against work already in flight: it is a `select!` branch racing
the input channel against whatever the task is currently awaiting, and
whichever resolves first wins. A new anchor jumps the status worker
straight back to resolving it, abandoning whatever it was counting toward
— no flag to check, no state to unwind, because the abandoned work was
never anything but a local variable in a stack frame a `continue` throws
away. Retries are the same kind of simplification: a loop variable
counting consecutive failures, reset on progress, checked against a
constant, rather than a field two call sites could each read or increment
out of turn.

Per-operation tasks supersede the other way, since there is no persistent
worker to re-target: the owning slot drops the old task outright and
spawns a new one. Either way, the unit of cancellation is exactly one
task, and exactly one place — the owner's slot — decides when to drop it,
whether by replacing it, clearing it, or letting an error unwind through
it.

This is what makes the shape immune to the usual failure modes of shared
mutable coordination: two readers disagreeing about which request is
current, a counter incremented on one path and never reset by another, a
task that outlives the interest that spawned it because nothing owns the
one handle that would cancel it. None of those states are representable
when the state lives in one task's local variables and the only way in is
a channel send.

A `watch` channel's own guarantee is what makes the coalescing above it
safe to keep simple: a receiver that has not looked in a while does not
miss the fact that something changed, even though it only ever sees the
latest value, never every intermediate one. A `dirty` flag and a periodic
repaint tick can therefore be nothing more than a bool and a timer —
correctness (the next repaint reflects the true latest state) comes from
the channel itself; the flag and the tick are purely about *when* to
spend the cost of a redraw, never about whether an update would otherwise
be lost.

## Honest, never blocking

A background answer is never a promise to wait for — it names its own
state honestly, and the consumer renders that state instead of blocking
to resolve it into something more final. The status worker's line-number
answer is resolved, still converging toward an answer, or never going to
have one; pending navigation's result is ready or still pending. Either
way, the type itself carries whether the answer is settled or still in
flight, so nothing downstream has to guess or poll blindly — the
interactive draw path in particular never waits on I/O: every query it
makes is a synchronous channel operation against a value some background
task already published or will publish next.

## Reading without disturbing

Every one of these background readers shares one more property with the
block cache itself: it never distorts what the cache remembers about
genuine interest. The index scan and the status worker's count both read
through the cache's non-promoting path (`warm()`, see
[block cache](block_cache.md)) rather than the interactive one, so however
large a background pass over the file gets, it cannot evict or reorder the
working set a user has actually scrolled back to — promotion stays
consumer-truthful, earned only by a real, interactive re-reference, never
by a background pass touching a block on its way through.

## Cancellation and shutdown

Every owned task is cancelled the same way: dropping its owner aborts the
task's handle outright, with no cooperative shutdown protocol to get
wrong — a queued task never starts, and one suspended at any ordinary
`.await` (a channel wait, a semaphore acquire, a between-blocks yield)
unwinds right there. Aborting mid-read is safe by construction rather than
by care taken at each call site — the block cache's own in-flight
registration is guarded to clean up after a cancelled fetcher instead of
leaving a stale entry behind (see [block cache](block_cache.md)), so an
aborted background reader never wedges the block it was fetching for
whoever asks next.

The one read that cannot be aborted this way is a blocking positioned
read already handed to the OS thread pool (`spawn_blocking`, which tokio
itself documents as uncancellable once running): a wedged network mount
can leave that read outstanding past its owning task's own cancellation,
finishing (or hanging) entirely on its own — every one of the three
background computations above, plus prefetch (outside the shape
architecturally, per its own section above, but reading through this
identical path), shares this exposure, real-file sources only, each
bounded by how many reads it can have in flight at once (`ScanScheduler`,
`StatusWorker`, and `PendingNav`: at most one each, being single tasks;
prefetch: up to `FILL_CONCURRENCY`, see [prefetch](prefetch.md)). Process
exit accounts for this directly rather than assuming every background
task unwinds cleanly — the runtime itself is shut down with a short
timeout after the event loop returns, so quitting abandons a stuck read
rather than waiting on a filesystem that may never answer.
