# Measuring performance

Two harnesses answer two different questions, and neither substitutes for
the other. The criterion benches isolate one engine operation at a time
under a controlled, injected latency, so a regression in one component's
cost is visible independent of everything else that happens to be
scheduled that run. The end-to-end harness drives the actual release
binary — the whole thing, tokio runtime, terminal setup, background
indexer and all — and races it against `less` on realistic fixtures, so
it answers the question a user actually experiences: does the pager on
screen feel fast. Neither number replaces the other; a criterion win in
`goto_line_cold_vs_indexed` that does not show up in the e2e `deep-goto`
row means something is eating the saving between the engine call and the
pixels, which is exactly the kind of gap only the whole-binary harness can
see.

## Criterion benches (`just bench`)

`ress-core/benches/engine.rs` measures engine operations — first paint,
warm scrolling, cold goto-end, indexed vs. cold goto-line, index
throughput — against in-memory fixtures with **injected latency** standing
in for a network filesystem (see the bench file's own `LATENCIES_MS`).
Because the latency is injected rather than real, the same bench run
covers both a local disk with no added delay and a slow mount with a
per-read delay layered on, without needing an actual slow filesystem on
hand, and it isolates
**cold-cache behavior** on demand: a fresh `Document` per iteration pays
the real first-read cost every time, which is the only way to observe
what a cold cache costs in isolation from everything warm-cache scrolling
benches measure separately.

Criterion benches never run in CI — machine-dependent numbers make poor
gate material — but `just ci` compiles them (`bench-build`, `--no-run`)
as a bit-rot guard, so a bench that no longer builds is still caught.

## The end-to-end harness (`just perf`)

The `ress-perf` binary measures **whole-binary wall clock**: it launches
the release `ress` binary and `less` each under its own directly-owned
pty (`openpty`, so both get a real terminal, the same way an interactive
user would), reconstructs each subject's screen from its raw output
through an in-process VT emulator, drives each through the same
keystrokes, and times how long the reconstructed screen takes to show the
expected content. This is the harness that can show a regression
criterion cannot: anything between an engine call returning and the frame
actually landing on screen — draw cost, event-loop scheduling, terminal
I/O — is invisible to a benchmark that never leaves the engine crate.
Every ceiling and every screen-anchored duration is measured against a
monotonic clock, immune by construction to a concurrent clock
adjustment skewing the result; a bash predecessor's timing, built on
re-reading bash's own wall-clock `$EPOCHREALTIME` for every check,
technically carried that gap. The one deliberate exception is the
ress-only precise number, which subtracts a wall-clock stamp from the
binary's own wall-clock tracing timestamp — two readings of the same
clock, because only that clock appears in the log — and so is the one
duration a mid-sample clock adjustment could still touch (see the
precise-number section below).

The two pagers are driven through one **symmetric methodology** so a
difference in the numbers reflects a real difference in the pagers, not
an artifact of how each was asked to work:

- Both run under a pty of the same size, and every scenario polls its
  reconstructed screen for completion rather than trusting either pager's
  own idea of "done."
- `less` always runs with `-S` (chop long lines). `ress` has no line-wrap
  mode at all — long lines are always chopped — so leaving `less` on its
  default wrap would hand it different work, not equivalent work, on any
  fixture with lines wider than the terminal.
- A scenario never sends a key until the pager has painted its first
  screen. Pre-paint input does arrive (the one historical exception — a
  cooked-mode CR→LF translation that turned a pre-paint Enter into an
  unbound key — is fixed in ress itself), but the wait is what anchors a
  keyed measurement to a ready subject, and it is applied to both pagers
  identically, so neither gets a head start the other is denied.

**Page cache stays warm between runs, by design.** Every run of a
scenario reuses the same fixture file without dropping caches, and
`fixtures/` persists across harness invocations rather than being
regenerated per run. Each scenario also reads its own fixture once, in
full, immediately before its first timed sample (a disclosed prewarm,
logged as `prewarming: ...`), so the very first sample never pays a
cache-fill cost the later ones don't — every scenario starts its timed
samples from the same warm baseline regardless of what ran before it.
Within a scenario, which pager runs first alternates by sample index
(sample 1: less then ress; sample 2: ress then less; and so on), so a
systematic edge for whichever pager happens to go second — inheriting the
other's residual warmth — cancels out across the reported median instead
of quietly favoring one pager. Even sample counts keep the two positions
exactly balanced (`RUNS/2` first, `RUNS/2` second, for each pager); an odd
count leaves one pager in the first, residual-warmth-free position one
more time than the other, so the default run count is even (`RESS_PERF_RUNS`
can override it, with a warning if the override itself is odd). The e2e
numbers therefore describe
steady-state, warm-cache behavior — repeatedly opening a file a user has
already been paging through — never a cold read. The cold-read story
belongs entirely to the criterion benches' injected-latency groups above;
this harness does not attempt to reproduce it on local disk (see the mount
leg below for the one partial exception).

## Fixtures

Three deterministic fixtures (`ress-filegen`, fixed seed) cover different
failure modes a pager can have with huge files:

- **`varied-log`** — ordinary log-shaped content: short-to-medium lines,
  a slice of multi-byte UTF-8. The general-purpose case every scenario
  runs against.
- **`megalines`** — mostly ordinary lines with a huge line embedded
  periodically, stressing a pager's handling of an occasional pathological
  line mixed into otherwise normal content.
- **`single-line`** — the whole fixture is one line with no interior
  newlines, stressing first-paint on content a line-oriented pager cannot
  meaningfully index or wrap its way through.

Fixtures are generated into `./fixtures/` **only if absent**, keyed by
filename — the harness never regenerates or overwrites a fixture that is
already there, so re-running it repeatedly does not re-pay generation
cost. `--quick` swaps the large `varied-log` fixture for a smaller one and
reduces the run count; `megalines` and `single-line` are already modest
in size and stay the same either way. `just fixtures` materializes the
standard (non-quick) set alone, without running any scenario.

## Scenarios

Each scenario runs against `varied-log` and `megalines`; `single-line`
has exactly one line, so only `open` applies to it (jump/goto/scroll have
nothing to jump, goto, or scroll to).

- **`open`** — launch to first paint. The core promise: the first screen
  should appear in bounded time regardless of file size.
- **`jump-end`** — send `G` to an already-open, already-responsive pager
  and time how long it takes to reach the true end of the file. This is
  the classic case where a synchronous pager can stall on a huge file;
  timed from the keypress, isolated from open cost, which `open` already
  covers.
- **`deep-goto`** — jump to a line near the end (90% of the file) from a
  cold launch. Timed from process launch, not from a keypress, because
  `less` has a real cold-start mechanism for this (`+N` on the command
  line jumps before ever painting line one) that `ress` does not — see
  the caveat below.
- **`scroll-cadence`** — a burst of 100 `j` presses sent as one literal
  string (both pagers get the identical burst rather than 100 separate
  keystrokes), timed from the keypress to the expected top line settling.
- **RSS after jump-to-end** — resident set size read from
  `/proc/<pid>/status` after the `jump-end` scenario's jump completes,
  on the same process. Settled before being trusted, not read on the
  first opportunity: `VmRSS` genuinely climbs roughly 2-4x within
  ~50-100ms of the target line becoming visible (the initial paint can
  render from a small, already-resident slice near EOF; the background
  indexer keeps building the full line index afterward), then plateaus
  hard — a single, immediate read can land anywhere along that climb
  depending on how much of it has happened by the moment the read
  fires, an artifact of observation timing rather than a genuine
  memory-usage difference (found directly, not assumed — see the PR's
  own RSS investigation). This harness samples `VmRSS` every 10ms
  across the whole 500ms ceiling — five times the observed climb — or
  until the subject exits, rather than stopping the instant three
  consecutive readings agree (at least 20ms of genuinely unchanged
  memory), reporting the HIGHEST value ever confirmed stable for that
  long at any point during the whole window. Changed from an earlier,
  early-return version (pass 8, superseded note below): stopping at
  the first three-in-a-row agreement can mistake a mid-ramp pause —
  e.g. a subject momentarily blocked on a slow or network-backed read
  — for the true, final settled value, understating it. Sampling the
  whole window regardless is slower for a subject that settles
  quickly (a sample now always costs up to the full ceiling, not just
  the ~20ms it takes to first confirm), the accepted cost of not
  mistaking a pause for done. On that ceiling (or the subject exiting)
  without ever confirming a stable plateau, the last reading taken is
  reported as-is, not flagged, since a settled-vs-unsettled marker has
  no existing channel to travel through in this report, and the
  constants above are chosen so the ceiling should essentially never
  fire against a healthy subject. Reported separately from the timing
  table since it is a memory number, not a latency one. This read is
  still opportunistic and can race the pager's own exit — the jump
  itself completed (the timing row's own value stands regardless), but
  the process may tear its pane down before even one `VmRSS` reading
  lands. A sample that races this way contributes nothing to the RSS
  row, so that row's own `runs` column reports how many readings
  actually landed, which can be lower than the timing row's `runs` for
  the identical samples — never the raw sample count standing in for a
  median it was not actually computed over. If no sample yields a
  reading at all, the row reads the literal token `no-data`, never a
  blank.

  The one-time parity gate that approved this port's swap-in for perf.sh
  used a ±40% per-cell band; the settled RSS numbers land inside it on
  every cell but jump-end/2GiB (42.2%), which is the retired script's own
  single immediate read racing the same `VmRSS` climb described above —
  not a port defect. That cell is accepted as a known, by-design
  exception to the (now-historical, since perf.sh is deleted) band,
  rather than chased further -- ruled and user-approved during PR #44's
  own post-v1-cleanup review (2026-07-20): settle-then-read (stopping
  at the first three-in-a-row agreement) was the frozen RSS baseline at
  that point.

  **Superseded, pass 8 (2026-07-21, user sign-off)**: settle-then-stop
  was itself found to understate a subject that pauses mid-ramp (see
  above) — the sample-full-window-then-plateau methodology described
  above is now the frozen RSS baseline going forward, not the
  settle-then-stop one this section originally documented. Re-measured
  directly, not guessed, against the same `varied-log-2g.log` fixture
  the 42.2% figure above references (`RESS_PERF_RUNS=2`, an override of
  the default six, to keep the re-measurement practical): `ress`'s own
  settled figure came back at 74306 KiB, indistinguishable from what
  the retired early-return algorithm also reported (74246 KiB) in this
  specific measurement environment — this harness's own fixtures sit on
  a fast, local disk, so they never exhibit the mid-ramp pause the new
  algorithm specifically guards against (the original RSS investigation
  associated that with slow or network-backed reads, not a local one).
  The 42.2% figure above therefore stands unchanged, since the
  underlying `ress`-side number it was computed from did not itself
  move here; a subject that genuinely does pause mid-ramp (the case
  this pass's fix targets) would show a larger divergence from the old
  algorithm than reproduced in this environment. See the PR's own RSS
  investigation for the full account of both baselines.

## Hangs and crashes are recorded, not fatal

A sample that never finishes is a result, not a harness failure — and a
subject that hangs (still running, just never responding) and one that
crashes (gone entirely) are reported as the distinct outcomes they are,
never folded into each other. Every timed wait — first paint,
jump-to-end, deep-goto's landing, scroll-cadence's settle — carries a
ceiling reused directly from that wait's own existing limit
(`PAINT_TIMEOUT_S` for a readiness wait, `SCENARIO_TIMEOUT_S` for a
completion wait). A crash is detected the instant it happens: the pty's
own end-of-file and the subject's exit status are interleaved into the
very same poll that checks for the expected content, so a pager that
dies mid-wait, after sending keys, is recorded as a crash within that
same poll tick — never only discovered by silently polling out the full
ceiling first and revalidating afterward, which is as precise as a
pane-polling predecessor could ever get. A wait that reaches its ceiling
with the subject still alive and still running the expected pager is a
genuine hang. Either way the sample is torn down exactly like a completed one,
and the run continues to the next sample. Each hang is attributed to the
SPECIFIC ceiling that fired for that sample, not assumed from the
scenario's nominal one: a scenario that sends keys (e.g. `jump-end`) can
hang either before it ever paints (`PAINT_TIMEOUT_S`) or after painting
but before reaching its target (`SCENARIO_TIMEOUT_S`), and those are
reported as the distinct outcomes they are — a pre-paint hang on a
fixture that always fails to paint reads `hung(>30s)`, never the
misleadingly larger `hung(>240s)`. A scenario's reported value follows
its samples: all completed, the plain median as before hangs were
tracked; a majority failed (hung, crashed, or both, including all of
them), a token naming every cause that fired — bare when the whole
majority shares one cause (`hung(>30s)`, `hung(>240s)`, or `crashed`),
bucketed with per-cause counts otherwise (e.g. `hung(>30s:2,>240s:1)`, or
`hung(>30s:2,crashed:1)` when a pager both hung and crashed across
different samples) — since a median built from whichever minority
happened to finish cannot honestly stand in for a result that mostly
never occurred; a minority failed, the median of the samples that did
complete, annotated with how many hung and/or crashed (e.g. `42(1h)`,
`42(1c)`, `42(1h,1c)`), so no real number silently absorbs another. The
harness's own exit code stays harness-health-only — a subject hanging or
crashing is exactly the measurement `open`, `jump-end`, and `deep-goto`
exist to catch, and aborting the run over it would be the harness
converting a subject's failure into its own.

## First paint, measured twice for `ress`

The `open` scenario reports two different numbers for `ress`, from the
same launch:

- A **screen-poll** number — wall clock from session start to the
  reconstructed screen's first non-blank row — measured identically for
  `less`, so it is the number that is directly comparable between the two
  pagers.
- A **precise** number, `ress`-only: the child process itself records a
  wall-clock reading immediately before its own `exec` — written over a
  pipe from the spawn hook, the same spot the retired wrapper stamped:
  as close to the `exec` as any stamp can get (only the hook's own
  return stands between them, schedule-free), with neither the parent's
  pty setup nor the spawn's scheduling inside the number — and the
  precise figure is
  the delta between that reading and the timestamp on the binary's own
  `perf`-targeted "first paint" tracing event (emitted once, immediately
  after the first frame is drawn). This isolates the binary's own
  startup-to-paint cost from everything the screen-poll number cannot
  separate out, at the tracing event's real timestamp precision rather
  than the screen-poll's granularity. The stamp side of the reading is
  opportunistic only for a genuinely clock-skewed pair (both readings
  present, but disagreeing — most plausibly a small clock adjustment
  landing between them): like RSS, that leaves the sample's precise
  value simply missing, and the precise row counts only the samples
  that contributed a reading (`no-data` when none did). A missing exec
  stamp is a different matter, held to the log side's own standard, not
  RSS's: the stamp pipe is harness-owned plumbing, written
  unconditionally before the subject execs, so its absence in front of
  a confirmed screen paint is not this one sample racing anything. The
  log side has always been that standard: once the screen has
  confirmed a paint, a first-paint log line that never appears (a
  misconfigured `RESS_LOG`, a renamed tracing event) on a still-live
  subject is a broken instrument, and the run fails loudly rather than
  printing rows that silently measure nothing; a subject that has
  already died explains its own silence instead (folded into that
  sample's crash, never blamed on the harness) — and that same
  liveness check now covers a missing stamp too, not only a missing
  log line.

`less` has no equivalent instrumentation, so it only ever gets the
screen-poll number.

## Running it

- `just bench` — criterion benches, local only, never in CI.
- `just fixtures` — materialize the standard fixture set without running
  any scenario.
- `just perf` — the full end-to-end comparison; `just perf --quick` for a
  faster pass against smaller fixtures and fewer runs per scenario.
- `RESS_PERF_MOUNT=<dir> just fixtures` then `RESS_PERF_MOUNT=<dir> just perf`
  — a two-step flow, not one. The first step materializes TWO fixtures
  inside `<dir>/ress-perf-fixtures/` (a dedicated subdirectory, not `<dir>`
  itself, so a fixture-shaped name never collides with a real file already
  on the mount), a real mounted filesystem (NFS, Lustre, Weka, ...) — one
  per scenario that needs its own never-touched-by-this-run fixture
  (`open`, `jump-end`), byte-identical in content but separate files, so
  one scenario's repeated reads can never warm another's own first sample
  (an earlier shape shared one file between them, which made that first
  sample's freshness order-dependent: whichever scenario happened to run
  second inherited the first's warmth). The second step repeats `open`
  and `jump-end`, each against its own already-existing fixture, and
  refuses to run — naming exactly which file(s) are missing — if either
  is absent, rather than generating them on the spot. The steps stay
  separate because writing 256 MiB onto the mount warms the very cache
  this leg wants a fresh read against — see the mount-first caveat below
  for exactly how warm, and why — so a single invocation that both
  generated and timed a fixture would measure its own generation-warmth
  instead. **ress only: no less runs on the mount at all**, and there is
  no pager-order alternation (nothing to alternate with) — see the
  caveat below for why. Results are labeled `mount-first:` (sample 1
  alone, `runs=1`, the first read this harness makes of the fixture — see
  the caveat for what that does and does not guarantee about the
  underlying cache state) and `mount-warm:` (the median of the remaining
  samples).
- `RESS_PERF_RUNS=<n> just perf` — overrides the sample count per scenario
  (default 6, or 2 under `--quick`) instead of using the derived default.
  A positive integer is required; an odd value is accepted (it is the
  caller's call) but prints a one-line warning, since alternation cannot
  balance an odd count.

## Caveats

- **The reader drains eagerly: an event-driven wait, not a
  fixed-cadence sleep.** Continuous draining is the harness's own
  baseline — every byte a subject writes is read as soon as it
  arrives, rather than sampling the screen on a fixed tick, so most
  completion checks observe a change immediately, not "somewhere in
  the next poll tick." What happens once a subject genuinely goes idle
  is a deliberate design choice, not a fallback to apologize for: the
  main loop's own wait, once idle, is a `ppoll(2)` wait for the pty
  master becoming readable again — not a blind sleep on a fixed
  cadence — capped to whatever time genuinely remains before the
  relevant deadline, so it can never wake meaningfully past a wall it
  is about to be judged against. This is the faithful terminal model:
  a real terminal (and perf.sh's own `tmux`) wakes the instant a
  subject writes, with no artificial poll delay of its own, and the
  `scroll-cadence` investigation below found this distinction is not
  cosmetic — a paced, sleep-based reader measurably changes how fast
  the SUBJECT itself can write, via ordinary pty backpressure, not
  merely how fast the harness notices. The same eager wake also
  removed a quieter artifact from every OTHER fast-completing
  scenario: `open`/`jump-end` readings that used to pin at a flat
  10ms — the sleep-paced reader's own wake latency putting an
  artificial floor under genuinely single-digit-millisecond events —
  now report their real value instead. An instrument change, not a
  subject performance change: those pagers did not get faster, the
  harness just stopped adding latency of its own to the measurement.
  A distinct, fixed-cadence 10ms checkpoint governs `scroll-cadence`'s
  own stability check independently of this wait (see below) — the two
  share only the number (10ms is also this wait's own upper bound, via
  the same clamp), not the trigger: the wait is event-driven, woken by
  data; the checkpoint is a genuine wall-clock timer, unaffected by how
  promptly a wait happens to return. (Found genuinely drifting to
  ~18ms in a later review, not the documented ~10ms: `libc::poll`'s
  own millisecond-truncating timeout undershot every wait by up to
  ~1ms, and the wait was not also clamped to the next checkpoint due —
  an early-but-still-too-soon wake requested a full fresh interval
  instead of the sliver actually remaining, landing two ~9ms waits
  between checkpoints instead of one ~10ms one. Fixed via `libc::
  ppoll`'s own nanosecond-precision timeout and clamping the wait to
  `next_checkpoint` too — confirmed by direct measurement to be back
  to ~10ms, not merely re-asserted.) This is why `ress`'s `open`
  scenario additionally reports the tracing-based precise number: for
  the one scenario where sub-poll precision matters most, there is a
  way to get it that does not depend on screen reconstruction at all.
- **`scroll-cadence`'s settling definition is stricter than a bash port
  would be, by construction.** perf.sh's `wait_top_line_stable` samples
  the pane on a fixed, blind 10ms tick regardless of whether the pager
  is still actively writing between samples, so two samples that happen
  to agree are enough to call it settled, even if more redraws are
  still in flight underneath — it cannot tell "held steady" apart from
  "changed and happened to come back" between its own two samples. This
  harness pairs a fixed-cadence needle match with a top-row generation
  counter (bumped whenever row 0's own content differs across a
  read-drain boundary — see `Screen::feed`'s own doc comment for the
  exact granularity this can and cannot see): a stability pair requires
  the expected content at one checkpoint *and* the generation still
  unchanged at the next one, spaced by the poll interval — unrelated
  output on other rows neither hurries nor delays the verdict, since
  only row 0's own generation is watched. A redraw that lands row 0
  back on the expected text after a genuine intermediate change does
  not count as settled: the generation will have moved, so the pair is
  discarded and confirmation restarts from that tick. That is the
  concrete mechanism behind "stricter" — not a hoped-for property of
  reconstructing the screen, but an explicit gate perf.sh's own
  consecutive-sample oracle has no equivalent of.
- **That gate did not explain why this port's own scroll-cadence
  number used to run 4-5x higher than perf.sh's — a dose-response
  investigation found the real cause was the reader, and it has since
  been fixed.** A real (row 0 content, generation) timeline, recorded
  at a fixed 10ms tick through a real 100-`j` burst against real
  `ress`, showed the two oracles above agreeing tick for tick, every
  time: `ress`'s own line-number prefix climbs strictly monotonically
  across the whole burst (1, 2, 3, ... 101, confirmed directly — never
  repeating or reverting), so the needle-match-twice oracle has no
  spurious-repeat window to fall into for this scenario — the target
  value is only ever reached once, at the very end. The generation
  gate's own strictness is real (a constructed churn-and-revert case
  defeats the old, bool-shaped logic and not the new one —
  `ress-perf/src/runner.rs`'s own
  `deadline_declines_a_stability_pair_when_row_zero_churned_and_reverted_in_between`
  pins it), but it was never what separated this port's own reading
  from perf.sh's for this scenario: that hypothesis, tested this way,
  was refuted.

  Isolating the one remaining variable instead — how fast each
  harness's own reader drains the pty — found the actual cause:
  replaying the identical recording loop with no sleep at all settled
  in ~50-65ms, matching perf.sh's own live reading; adding back this
  port's former `POLL_INTERVAL`-paced sleep (fired once the pty had
  gone idle) settled at ~310-330ms, matching both this port's own live
  reading at the time and a direct, unmodified `run_sample` call in
  the same process. `ress`'s own redraws are not coalesced (the
  recorded line number advances one at a time, never in jumps), so the
  100-redraw burst was identical work either way — what differed was
  how much of it happened while `ress`'s own blocking writes were
  stalled on a full pty buffer, waiting for a reader that was, up to a
  tenth of a second at a stretch, not there to drain it. `tmux`
  (perf.sh's own reader, an event-driven multiplexer with no
  artificial poll delay of its own) drains close to as fast as the
  no-sleep variant; this port's former checkpoint-paced sleep did not,
  by design — its purpose was to give `TopLineStableStartsWith` a
  well-defined cadence to pair confirmations against, never to
  minimize reader latency, and that backpressure compounded over ~100
  redraws into the gap this section used to describe. Consistent with
  the dose-response evidence, not proven beyond it — the blocking
  writes themselves were not straced. (Writes to other rows, like a
  trailing status-line update, never delayed the check itself: the
  predicate watches only the top line's own stability, checked every
  poll tick regardless of idle state — a separate property from the
  pacing effect above, unaffected by it.)

  **Acted on, not just diagnosed.** The main loop's own wait is now
  the `ppoll(2)`-based event wait described in the caveat above,
  replacing the sleep that created this backpressure. Re-measured
  directly against a resurrected `perf.sh` — quick, full, and
  fake-mount legs, shared fixtures — `ress`'s own absolute
  scroll-cadence reading now holds at 61-74ms across every leg and
  fixture (was 274-330ms before this fix), and the perf.sh-vs-port
  ratio is 1.0x-1.19x (was 4.8x-5.4x): the number that used to read as
  "this port is 4-5x slower than perf.sh at detecting scroll
  settlement" is now understood, in full, as an artifact of the
  reader's own former pacing, not a genuine terminal-fidelity
  difference — and it is gone with the pacing that caused it.
- **Warm-cache bias, by design.** See above — every number here describes
  repeated, warm-cache access. Do not read an e2e number as a claim about
  first-ever access to a file on a cold filesystem; that claim belongs to
  the criterion benches' injected-latency groups.
- **The mount leg is ress-only, and first-touch rather than genuinely
  cold.** Generating a fixture on the mount warms the host's own page
  cache through the write itself: measured directly with a small
  mmap+mincore probe, 81.25% of a freshly generated fixture's pages are
  resident immediately after generation, and no unprivileged step
  available to this harness can evict them — `posix_fadvise(DONTNEED)`
  was tried directly and made no difference, because a fresh write
  leaves dirty pages behind, and DONTNEED cannot drop a dirty page (only
  a clean one). So even sample 1 is not a guaranteed-cold read of the
  underlying storage; it is the first unprewarmed access this harness's
  own process makes, which is a real and useful thing to measure — it is
  exactly what a user's first `ress somefile` on a mount looks like — but
  a different, narrower claim than "the page cache was empty," which is
  why it is labeled `mount-first:`, not `mount-cold:`. Genuine cold would
  need either generating the fixture from a separate host (so this
  host's page cache is never touched by the write) or a root-only cache
  drop (`echo 3 > /proc/sys/vm/drop_caches` or equivalent); this harness
  performs neither — it stays unprivileged by design, the same reason it
  never touches a system-wide lesskey file either. The mount `jump-end`
  scenario is also timed differently from its local counterpart, for the
  same underlying reason: it reports elapsed time from process launch,
  not from the `G` keypress the local `jump-end` scenario times from.
  ress's own startup (`Document::new` spawning the background indexer,
  the first viewport read behind the pane's own first paint) already
  reads this mount-resident file throughout the paint wait, before any
  key is ever sent — a keypress anchor would silently exclude that first
  touch, leaving the number measuring only a warm continuation of a read
  this same process already started, not the cold open-and-jump story
  `mount-first` exists to capture. The local `jump-end` scenario keeps
  its keypress anchor because its fixture is explicitly prewarmed before
  any sample runs, which makes startup reads free there — excluding them
  from that timer is the correct, deliberate choice for what that number
  means; on the mount, where startup reads are exactly what is not free,
  including them is the correct, equally deliberate choice for this one.
  There is also no
  unprivileged way to create two *independently* first-touched windows
  on the same mount-resident fixture for a second pager to race against:
  running two pagers here and reporting a median would compare whichever
  pager happened to go first against one going second with residual
  warmth from the first's own read — a fake symmetry, not a real
  comparison, and worse than not reporting one at all — which is the
  separate reason (beyond the cache-state question above) this leg keeps
  only ress, the subject this harness exists to measure. Results are
  reported as two honest numbers for two different questions instead of
  one blending them: `mount-first:` (sample 1 alone, `runs=1`) and
  `mount-warm:` (the median of the remaining samples). It exercises
  exactly two scenarios (`open`, `jump-end`) on one fixture size, and
  whatever caching the mount's own server does beyond the client is
  entirely outside the harness's control — it cannot force a truly cold
  read the way injected latency can. Treat it as a real-filesystem sanity
  check, not a replacement for the criterion latency groups' controlled,
  genuinely cold-cache numbers.
- **`deep-goto`'s two pagers do not reach line N the same way.** `less`
  has a genuine cold-start mechanism for jumping to a line (`+N` on the
  command line, before painting anything) that `ress` does not have;
  `ress`'s side of this scenario opens normally, waits for the
  first screen, and only then issues the jump. Its number therefore
  honestly includes a first-paint-at-line-one cost that `less`'s does
  not — that gap is real product behavior, not a harness bug, and it is
  worth reading as a possible argument for a future `ress` flag rather
  than a straightforward "which pager seeks faster" comparison.
