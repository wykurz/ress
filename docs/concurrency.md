# Concurrency

`ress` runs four independent background computations alongside the
interactive event loop: indexing a file's line structure, answering the
status line's current-line query, completing a navigation that outran its
interactive budget, and sweeping a committed search pattern over the whole
file for a running match count and density map. All four are built to one
shape, described here once rather than separately for each: one task, owned
outright by whoever needs its answer, consuming immutable inputs and
publishing immutable snapshots — never shared mutable state that two sides
reach into.

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
- **The search sweep** (`SweepAnalysis`, driven by `crate::analyzer::spawn` —
  a generic background-analysis loop generalized out of the status worker's
  own shape, `ress-core/src/search.rs`) enumerates every match of a
  committed search pattern over the whole file, publishing a bounded
  `SearchSummary` — a running match count, a fixed-size density histogram,
  how far it has gotten — after each bounded unit of work. Like the status
  worker, it has both an inbound channel (a fresh committed search
  supersedes whatever pattern it was counting) and an outbound one
  publishing its answer; see [search](search.md) for the full model.
- **Pending navigation** (`PendingNav`, `ress-core/src/resolve.rs`, owned by
  the run loop's own slot in `ress/src/app.rs`) is spawned fresh for one
  navigation that could not finish inside its interactive budget,
  publishing bytes-scanned progress and resolving to the final anchor.
  Unlike the three document-lifetime workers above, it answers exactly one
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
cancel-on-drop guarantee as the four background computations above — with
the same detached-fetch exception those four tasks share too (see
[prefetch](prefetch.md)'s own Cancellation section for the full
three-layer account and its `FILL_CONCURRENCY` bound, not restated here).

The block cache's own fetch (see [block cache](block_cache.md)) breaks the
"never detached" half of the shape outright, deliberately (batch 4
(2026-07-24), finding #11) — the one intentional exception, not an
oversight. A block's *fetch* is spawned as its own task the instant the
first requester misses, owned by nobody: no owner's drop reaches it
directly, because reaching it directly is exactly the bug this restructure
closes. The spawn is not itself the physical read, and the distinction is
load-bearing for the paragraph below: the task first waits to be admitted by
the source, then takes one last look at the waiter count, and only then
reads. The justification is the same uncancellable-pread reality the rest
of this document already accounts for below — the OS-level read cannot be
interrupted once started regardless of what owns the task around it, so a
requester's cancellation reaching the fetch could only ever detach the
*await*, never the read itself, and used to do exactly that: the read kept
running, and the caller who dropped it just lost the result, leaving a
replacement request to start over and duplicate the very read still
finishing in the background. Not owning the fetch at all — publishing its
result for whoever is still interested, cache-wide, instead of handing it
to one owner to lose — turns that previously-wasted, previously-duplicated
work into a plain cache fill. Memory-safety costs nothing extra (the task
holds an `Arc` of what it needs, same as any other clone of shared state).

What restructure R2 (2026-07-27) changes is *how much* of the general
shape's own "never left running after the interest that started it is
gone" guarantee is actually given up: not all of it, and not
unconditionally. A fetch that has already committed to its physical
read — **spent** its ticket, which is one step past the source admitting
it — keeps that guarantee's exception exactly as described above: nothing
reaches it, it runs to completion regardless. A fetch that has NOT yet
committed is largely not exempt: it *looks* for its last interested party
having left, at two points per pass, and dies unstarted without ever
costing a read whenever a look finds nobody.

Each look is a **linearization point, not a promise covering the window
between them** (batch 22 (2026-08-01) narrows this paragraph to the
contract [block cache](block_cache.md) already states; it used to say every
unspent fetch dies "once that happens and it is next scheduled to notice",
which the implementation does not enforce). Two consequences, both real:
the death is not instantaneous even when it does happen — a `watch` bump
and a task being polled again are different events, so the bound is
eventual — and a waiter that drops *after* the second look, while the
ticket is being spent, is simply not observed, so that read happens with
nobody left to want it. The window is one straight-line spend with no
`.await` between the check and the read, but it is not empty, and calling
it empty is what this document used to do.

So the trade this exception makes is narrower than it first looks:
cancellation reaching a fetch that has already SPENT its ticket would only
ever turn a certain read into a duplicated one, which is why that case gives
up the guarantee on purpose. (An *admitted* fetch is not yet that case: it
takes one more look at the waiter count before spending, so cancellation
observed there still costs nothing.) Cancellation reaching an unadmitted
fetch costs
nothing to honor, so the guarantee is not given up there at all.

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
genuine interest. The index scan, the status worker's count, and the
search sweep all read through the cache's non-promoting path (`warm()`, see
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
unwinds right there. Aborting an owned task mid-wait on a block is safe by
construction rather than by care taken at each call site: the block
cache's own fetch is not the thing being aborted (see [block
cache](block_cache.md) and its own note in "The shape," above) — dropping
a requester only ever drops that requester's own `WaiterTicket`, never the
fetch itself, so there is no stale in-flight registration to leave behind
and nothing for a dropped task to wedge for whoever asks next. The
registration lives until the fetch itself ends — by publishing, by dying
unstarted when one of its own abandon checks finds every waiter gone
(restructure R2, 2026-07-27; each check is a linearization point rather
than a promise covering the window around it, so a departure landing while
the ticket is being spent is not observed — see [block
cache](block_cache.md)'s "In-flight coalescing" for the three ways a fetch
can end), or by an involuntary teardown publishing an error — independent
of which, if any, of its original requesters are still around to receive
whatever it produces.

The one read that cannot be aborted at all, by anyone, is the blocking
positioned read itself, already handed to the OS thread pool
(`spawn_blocking`, which tokio itself documents as uncancellable once
running): a wedged network mount can leave that read outstanding
indefinitely, finishing (or hanging) entirely on its own, regardless of
every requester's cancellation. `ress-core/src/source.rs`'s `ReadTicket`
(restructure R1, 2026-07-27) is this fact made a value: **spending** one
(`ReadTicket::read`) is the point past which cancellation can no longer
prevent the read from happening. Merely *holding* one does not commit
anything — an admitted-but-unspent ticket can still be dropped for free,
releasing its permit on the spot, which is what lets the block cache take
one last cancellation decision after admission resolves (batch 16
(2026-07-31); see that type's own doc comment for the exact contract). The block cache's own fetch is built around that boundary
rather than exposed to it — for the SAME block, repeated supersession
shares that one still-running read instead of piling up duplicates, no
matter how many requesters come and go.

**What is actually enforced is one live REGISTRATION per block** (batch 23
(2026-08-01) narrows this bullet, which used to claim "at most one physical
read per block, cache-wide, at any time" outright). Every ordinary
requester either finds that registration and joins it or creates it, so no
ordinary sequence — supersession included — produces two concurrent reads
of one block. The gap is the involuntary ending: a fetch destroyed
mid-`.await` by a panic or by runtime teardown deregisters from inside
`Drop`, while a read it had already SPENT can outlive the future that
started it (`PreadSource` hands the permit into `spawn_blocking`; see
`ReadTicket`'s own doc comment for which sources have that property and
which do not). A retry registering after that teardown can therefore
overlap the surviving read. Nothing is unsound about it — both reads answer
the same block and the monotone `absorb` accepts either — but the count is
bounded by `PreadSource`'s per-source semaphore, not by this rule.

For DIFFERENT blocks, restructure R2 (2026-07-27) replaces what used to be
an open, unstated accumulation with two separate, stated bounds — one for
each side of the `ReadTicket` boundary. A status worker re-anchored by every
motion, or a pending navigation replaced by the next one, can each leave a
fetch behind as they move on to a different block; what happens to that
fetch next depends on which side of admission it is on:

- **Unadmitted fetches are bounded by live interest, not by history.** A
  fetch that has not yet been admitted dies once its last waiter's own
  `WaiterTicket` drops and the fetch task is next scheduled to notice —
  eventual, not instantaneous (a `watch` bump and the task actually being
  polled again are two different events) — then deregisters and publishes
  nothing, since nobody is left to receive it (batch 5, finding #4; see
  [block cache](block_cache.md)'s "In-flight coalescing" for the
  mechanism). So in steady state — once every fetch task has had a chance
  to notice its own last waiter leaving — the number of unadmitted fetches
  is bounded by however many *distinct blocks some still-live requester
  currently wants*: abandoned interest does not accumulate past the point
  the last requester for it walks away, no matter how many motions churned
  through before that. This bound depends on the fetch task's own
  `waiterless` signal actually being seeded before any drop can bump it,
  not merely on the drop happening — a subscribe that ran too late could
  silently miss an already-happened bump, with no self-heal — pinned by
  `ress-core/src/cache.rs`'s own
  `probe_p1_requester_dropped_before_the_fetch_task_first_poll` and
  `probe_p1_production_shaped_unbiased_select_hits_it_about_half_the_time`.
- **Committed fetches are bounded by their source's own admission cap, where
  one exists.** Once a fetch commits — which is **spending** the ticket, not
  admission resolving — it always completes and publishes, regardless of who
  is left waiting: D's rule, unchanged in substance, with the boundary named
  one step later than this bullet used to name it (batch 16 (2026-07-31)).
  Between admission and the spend the fetch takes one final decision, and
  that decision is a linearization point rather than a window-wide promise:
  a cancellation the recheck OBSERVES stops the read; one landing after it,
  while the ticket is being spent, does not. The trait itself requires no cap at all (an uncapped
  source admits unboundedly, by design — most of this crate's own test
  fakes are exactly that); `PreadSource`, the real-file source this binary
  actually uses, is the one that routes every admission through a
  per-source semaphore (`--read-concurrency`, default 16), and the
  `ReadTicket` it mints carries that permit across the read — see that
  type's own doc comment for the exact contract (the permit's lifetime,
  what a dropped, unspent ticket costs). For `PreadSource` specifically, at
  most N OS reads exist per source, ever, no matter how many distinct
  blocks the tasks above churn through.

Both bounds are enforced, not aspirational, though by different mechanisms:
`WaiterTicket::drop` shrinks the *waiter count* on a registration, not the
registry itself — the *registry* only ever shrinks through
`in_flight::InFlight::end`, reachable from `FetchCompletion`'s three
endings, `publish`/`try_abandon`/`Drop` (see [block cache](block_cache.md)).
The first bound is a property of the cache's own registry, exercised by the
probes named above plus
`k_distinct_block_fetches_die_unstarted_once_every_requester_is_gone`; the
second is `PreadSource`'s own pre-existing semaphore, unchanged by this
restructure. One known gap, stated rather than papered over: the
*interactive* attempt awaits its own first uncached read inline in the
event loop, so a fully wedged mount still hangs that one motion
(bounded-work budgets bound read *count*, not a single read's latency) —
a pre-existing behavior this bound neither causes nor fixes. Process exit
accounts for stuck reads directly rather than assuming every background
task unwinds cleanly — the runtime itself is shut down with a short
timeout after the event loop returns, so quitting abandons a stuck read
(cache-owned fetches included) rather than waiting on a filesystem that
may never answer.
