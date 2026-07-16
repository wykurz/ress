# Measuring performance

Two harnesses answer two different questions, and neither substitutes for
the other. The criterion benches isolate one engine operation at a time
under a controlled, injected latency, so a regression in one component's
cost is visible independent of everything else that happens to be
scheduled that run. The end-to-end script drives the actual release
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

## The end-to-end script (`just perf`)

`scripts/perf.sh` measures **whole-binary wall clock**: it launches the
release `ress` binary and `less` inside `tmux` (so both get a real pty,
the same way a terminal user would), drives each through the same
keystrokes, and times how long the pane takes to show the expected
content. This is the harness that can show a regression criterion cannot:
anything between an engine call returning and the frame actually landing
on screen — draw cost, event-loop scheduling, terminal I/O — is invisible
to a benchmark that never leaves the engine crate.

The two pagers are driven through one **symmetric methodology** so a
difference in the numbers reflects a real difference in the pagers, not
an artifact of how each was asked to work:

- Both run in a tmux session of the same size, and every scenario polls
  `tmux capture-pane` for completion rather than trusting either pager's
  own idea of "done."
- `less` always runs with `-S` (chop long lines). `ress` has no line-wrap
  mode at all — long lines are always chopped — so leaving `less` on its
  default wrap would hand it different work, not equivalent work, on any
  fixture with lines wider than the terminal.
- A scenario never sends a key until the pager has painted its first
  screen. `ress` drops input sent before it enables raw mode rather than
  queuing it, so this wait is a correctness requirement for it, not an
  optimization — and it is applied to `less` too, so neither pager gets a
  head start the other is denied.

**Page cache stays warm between runs, by design.** Every run of a
scenario reuses the same fixture file without dropping caches, and
`fixtures/` persists across script invocations rather than being
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
this script does not attempt to reproduce it on local disk (see the mount
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
filename — the script never regenerates or overwrites a fixture that is
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
- **RSS after jump-to-end** — resident set size read from `/proc/<pid>/status`
  immediately after the `jump-end` scenario's jump completes, on the same
  process. Reported separately from the timing table since it is a
  memory number, not a latency one. This read is opportunistic and can
  race the pager's own exit — the jump itself completed (the timing
  row's own value stands regardless), but the process may tear its pane
  down before `/proc/<pid>/status` is even read. A sample that races this
  way contributes nothing to the RSS row, so that row's own `runs` column
  reports how many readings actually landed, which can be lower than the
  timing row's `runs` for the identical samples — never the raw sample
  count standing in for a median it was not actually computed over. If no
  sample yields a reading at all, the row reads the literal token
  `no-data`, never a blank.

## Hangs and crashes are recorded, not fatal

A sample that never finishes is a result, not a harness failure — and a
subject that hangs (still running, just never responding) and one that
crashes (gone entirely) are reported as the distinct outcomes they are,
never folded into each other. Every timed wait — first paint,
jump-to-end, deep-goto's landing, scroll-cadence's settle — carries a
ceiling reused directly from that wait's own existing limit
(`PAINT_TIMEOUT_S` for a readiness wait, `SCENARIO_TIMEOUT_S` for a
completion wait); a sample that hits it is revalidated before being
trusted as a hang: still alive and still running the expected pager means
a genuine hang; pane gone, or running something else, means the pager
crashed instead (a pager that dies mid-wait, after sending keys, would
otherwise poll silently to the full ceiling and get misreported as merely
slow). Either way the sample is torn down exactly like a completed one,
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
script's own exit code stays harness-health-only — a subject hanging or
crashing is exactly the measurement `open`, `jump-end`, and `deep-goto`
exist to catch, and aborting the run over it would be the harness
converting a subject's failure into its own.

## First paint, measured twice for `ress`

The `open` scenario reports two different numbers for `ress`, from the
same launch:

- A **capture-pane** number — wall clock from session start to the pane's
  first non-blank row — measured identically for `less`, so it is the
  number that is directly comparable between the two pagers.
- A **precise** number, `ress`-only: a wrapper stamps a start time just
  before exec-ing the binary, and the precise figure is the delta between
  that stamp and the timestamp on the binary's own `perf`-targeted
  "first paint" tracing event (emitted once, immediately after the first
  frame is drawn). This isolates the binary's own startup-to-paint cost
  from tmux/shell overhead the capture-pane number cannot separate out,
  at the tracing event's real timestamp precision rather than the
  capture-pane poll's granularity.

`less` has no equivalent instrumentation, so it only ever gets the
capture-pane number.

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
  alone, `runs=1`, the first read this script makes of the fixture — see
  the caveat for what that does and does not guarantee about the
  underlying cache state) and `mount-warm:` (the median of the remaining
  samples).
- `RESS_PERF_RUNS=<n> just perf` — overrides the sample count per scenario
  (default 6, or 2 under `--quick`) instead of using the derived default.
  A positive integer is required; an odd value is accepted (it is the
  caller's call) but prints a one-line warning, since alternation cannot
  balance an odd count.

## Caveats

- **10ms poll granularity.** Every completion check polls
  `tmux capture-pane` on a fixed interval; a scenario that genuinely
  finishes in, say, 2ms can only be observed as "somewhere in the next
  poll tick," not sub-poll-precision. This is why `ress`'s `open` scenario
  additionally reports the tracing-based precise number: for the one
  scenario where sub-poll precision matters most, there is a way to get
  it that does not depend on capture-pane at all.
- **Warm-cache bias, by design.** See above — every number here describes
  repeated, warm-cache access. Do not read an e2e number as a claim about
  first-ever access to a file on a cold filesystem; that claim belongs to
  the criterion benches' injected-latency groups.
- **The mount leg is ress-only, and first-touch rather than genuinely
  cold.** Generating a fixture on the mount warms the host's own page
  cache through the write itself: measured directly with a small
  mmap+mincore probe, 81.25% of a freshly generated fixture's pages are
  resident immediately after generation, and no unprivileged step
  available to this script can evict them — `posix_fadvise(DONTNEED)`
  was tried directly and made no difference, because a fresh write
  leaves dirty pages behind, and DONTNEED cannot drop a dirty page (only
  a clean one). So even sample 1 is not a guaranteed-cold read of the
  underlying storage; it is the first unprewarmed access this script's
  own process makes, which is a real and useful thing to measure — it is
  exactly what a user's first `ress somefile` on a mount looks like — but
  a different, narrower claim than "the page cache was empty," which is
  why it is labeled `mount-first:`, not `mount-cold:`. Genuine cold would
  need either generating the fixture from a separate host (so this
  host's page cache is never touched by the write) or a root-only cache
  drop (`echo 3 > /proc/sys/vm/drop_caches` or equivalent); this script
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
  entirely outside the script's control — it cannot force a truly cold
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
