#!/usr/bin/env bash
# end-to-end perf harness: races the release ress binary against less on
# deterministic fixtures, inside tmux (so both pagers get a real pty). see
# docs/perf.md for the methodology this script implements and its caveats.
# run via `just perf` / `just perf --quick`; `just fixtures` materializes the
# standard fixture set alone (this script's --fixtures-only).
set -euo pipefail

# --- tunables -----------------------------------------------------------
# a representative modern terminal; fixed rather than inherited so a run's
# numbers do not depend on whoever's window happened to be focused.
readonly PANE_COLS=120
readonly PANE_ROWS=50
# first paint must be fast regardless of fixture size — that is the whole
# point of both pagers' first screen — so a short timeout doubles as a
# harness-health check, not just a safety net.
readonly PAINT_TIMEOUT_S=30
# jump/goto/scroll scenarios can legitimately take a while for less on the
# large fixtures (it may scan the whole file synchronously) — generous on
# purpose so a slow-but-real scan is never mistaken for a hang.
readonly SCENARIO_TIMEOUT_S=240
# the tracing writer is non-blocking; this just bounds the wait for its
# flush after capture-pane already saw the first paint land.
readonly LOG_TIMEOUT_S=5
# capture-pane poll cadence (see docs/perf.md's "10ms poll granularity" caveat).
readonly POLL_S=0.01
# WHITELIST, not blacklist: every pane's environment is BUILT from exactly
# the entries below (env -i, then only these) rather than filtered down
# from whatever the invoking shell happened to export. this round replaced
# an every-round-growing `-u` blacklist (LESS, LESSOPEN, LESSCLOSE,
# LESSKEY, LESSKEYIN, LESSKEYIN_SYSTEM, BASH_ENV — six rounds' worth of
# individually-discovered leaks) after two more real ones surfaced in the
# same review pass it was supposed to have already closed:
#   - LESSKEY_CONTENT (and LESSKEY_CONTENT_SYSTEM): lets a lesskey source
#     be supplied as literal environment-variable CONTENT, no file needed
#     at all — confirmed real via `strings` on this dev shell's actual
#     less binary (`LESSKEY_CONTENT`, "Use lesskey source file contents."),
#     despite being UNDOCUMENTED in that same binary's own `man less` (not
#     found anywhere in its output) — a blacklist built by reading the
#     manual, however carefully, cannot list a vector the manual itself
#     omits.
#   - LINES/COLUMNS: less(1) DOES document these (its ENVIRONMENT
#     VARIABLES section) — note the actual names, no "LESS_" prefix,
#     correcting this round's own brief, which named them LESS_LINES/
#     LESS_COLUMNS; verified directly against the installed man page. a
#     blacklist needs every such variable named correctly AND remembered;
#     a whitelist needs neither, since an admitted set excludes anything
#     not on it by construction, typo or omission be damned.
# a blacklist's failure mode is silence: a vector nobody has hit yet, or
# named wrong, or forgot to add, is simply not covered, and nothing
# reports that. a whitelist's failure mode is loud: a genuinely-needed
# variable that was left off breaks the very first real launch, which is
# exactly why every entry below is justified by a demonstrated need, not a
# guess — see each bullet.
#
#   - TERM=tmux-256color: less, run under a fully empty environment
#     (`env -i less`, no pty), fails outright — "'unknown': I need
#     something more specific" — confirming a terminal type is a genuine,
#     hard requirement, not a nice-to-have. tmux does NOT inject one into
#     a pane's own process environment on its own (confirmed: `env -i`
#     inside a real tmux pane, `printenv TERM`, produces nothing) — a
#     pane's process gets exactly what its own launch environment
#     supplies. fixed to one value rather than inherited from whoever
#     invokes this script, for the same reason PANE_COLS/PANE_ROWS above
#     are fixed rather than inherited: a run's numbers must not depend on
#     the operator's own terminal. `tmux-256color` specifically: this
#     script's own panes ARE tmux panes (`tmux show-options -g
#     default-terminal` reports the same value), and a real terminfo entry
#     for it exists in this dev shell (`infocmp tmux-256color`) — both
#     pagers confirmed painting correctly under it, in a real tmux pane,
#     with nothing else in the environment yet.
#   - LC_ALL=C.utf8: pins the same UTF-8 rendering for both pagers
#     regardless of the invoking shell's own locale (this harness's
#     Linux-only, nix-devshell-only contract already assumes a specific
#     toolchain, so C.utf8's presence, confirmed via `locale -a`, is the
#     same kind of assumption, not a new one). LESSCHARSET, separately
#     pinned to "utf-8" by the old blacklist, is DROPPED here — confirmed
#     redundant by the actual man page's own documented precedence: "If
#     neither LESSCHARSET nor LESSCHARDEF is set, but any of the strings
#     ... 'utf8' is found in the LC_ALL, LC_CTYPE or LANG environment
#     variables, then the default character set is utf-8" — LC_ALL=C.utf8
#     already contains "utf8" as a substring, so less resolves the
#     identical utf-8 charset from LC_ALL alone; confirmed empirically too
#     (correct multi-byte rendering with LC_ALL present and LESSCHARSET
#     absent). one fewer entry to justify is strictly better under a
#     whitelist, where every entry is a claim someone must be able to
#     defend.
#   - HOME=$FAKE_HOME, XDG_CONFIG_HOME=$FAKE_HOME/.config,
#     XDG_DATA_HOME=$FAKE_HOME/.local/share: FAKE_HOME is a real, empty,
#     per-run directory (see main()) neither pager has ever written to or
#     found anything pre-existing in. this closes LESSKEYIN's fallback
#     search (absent, less searches "$XDG_CONFIG_HOME/lesskey" or
#     "$HOME/.lesskey" — verified against less(1)'s KEY BINDINGS section)
#     without LESSKEYIN needing its own explicit entry at all: under a
#     blacklist, LESSKEYIN had to be unset explicitly AND HOME/XDG had to
#     be redirected, because an unset-but-otherwise-inherited HOME would
#     still point at the invoking user's real one; under a whitelist,
#     simply never admitting LESSKEYIN plus pointing these three at an
#     empty directory closes the same hole with one fewer named variable.
#   - LESSHISTFILE=-: less(1) documents that an ABSENT LESSHISTFILE falls
#     back to "$XDG_DATA_HOME/lesshst" or "$HOME/.lesshst" — the default
#     history file, not "no history file" (only "-" or "/dev/null" mean
#     that) — so simply never admitting it (as a whitelist otherwise
#     would for everything else) would silently re-enable history writes
#     to FAKE_HOME instead of suppressing them; needs its own explicit
#     value for exactly the reason LESSKEYIN above does not.
#   - LESSKEYIN_SYSTEM=/dev/null: NEW this round — not in the old
#     blacklist at all. Confirmed via the installed less(1) manual: "If
#     the environment variable LESSKEYIN_SYSTEM is set, less uses that as
#     the name of the system-wide lesskey file. Otherwise, less looks in a
#     standard place" — that standard place, confirmed via `strings` on
#     this dev shell's actual less binary, is `/etc/syslesskey`, a real,
#     absolute, machine-wide path with NO relationship to FAKE_HOME —
#     unlike LESSKEYIN's fallback above, redirecting HOME/XDG does nothing
#     to close this one; it needs its own explicit closure.
#   - LESSNOCONFIG: investigated, NOT used. Exists as a real string in
#     this dev shell's less binary (`strings`, adjacent to LESSSECURE/
#     LESSSECURE_ALLOW) but is undocumented in that same binary's `man
#     less` — its exact semantics cannot be confirmed from the installed
#     manual (the one source of truth this investigation was scoped to
#     trust), and the whitelist already achieves full closure of
#     everything LESSNOCONFIG might additionally suppress without
#     depending on an unconfirmed mechanism — same standing rule as
#     Round 10's ENV vs. BASH_ENV: verified dependencies only, no
#     defensive padding for behavior that cannot be confirmed.
#   - BASH_ENV: not admitted, and needs no explicit `-u` to keep out
#     anymore (see Round 10 for the original discovery of this vector, in
#     the tracing wrapper's outer bash). `env -i` starts from nothing;
#     `-u` only ever mattered for unsetting a variable `env` would
#     otherwise have inherited. a whitelist has nothing to unset.
#
# ress reads none of these — confirmed by grepping ress/ress-core's own
# source for every `env::var`/`EnvFilter` call site: the only one is
# RESS_LOG (cli.rs, `EnvFilter::try_from_env("RESS_LOG")`), consumed only
# by --tracing's launch (added there, not here — see run_sample) and
# confirmed load-bearing by omitting it from an otherwise-identical
# tracing launch: the log file comes back completely empty, no "perf:
# first paint" line, `_tracing_precise_ms` would have nothing to read.
readonly -a ENV_WHITELIST_STATIC=(
  env -i
  "TERM=tmux-256color"
  "LC_ALL=C.utf8"
  "LESSHISTFILE=-"
  "LESSKEYIN_SYSTEM=/dev/null"
)

QUICK=false
FIXTURES_ONLY=false
declare -a TABLE_ROWS=()
declare -a RSS_ROWS=()
# every same-directory temp path generate_fixture is about to write through,
# registered BEFORE the (potentially multi-GiB, potentially long-running)
# write starts — see cleanup() and generate_fixture() below. a path stays
# registered even after its own mv succeeds; removing it from this array
# post-mv would save nothing (rm -f on an already-moved-away path is a
# harmless no-op) and would need its own bookkeeping for no benefit.
declare -a GENERATED_TEMP_FILES=()
# every tmux session name run_sample has created, by EXACT name — see
# cleanup()'s comment for why matching by prefix instead is unsafe. a name
# stays recorded even after kill_session_quiet already tore it down;
# killing an already-dead session in cleanup() is a harmless no-op, and
# the array is bounded by the sample count of one run either way (at most
# a few hundred), not worth pruning.
declare -a CREATED_SESSIONS=()
# monotonic, bumped once per run_sample call — guarantees a unique
# session/workdir name per sample with no collision risk (unlike a
# random suffix) and no need for every caller to invent its own label.
SAMPLE_SEQ=0
# the mount leg's scenarios that need a fixture never touched by an
# earlier scenario in the same run (see run_mount_leg): each gets its OWN
# suffixed file so one scenario's repeated reads can never warm another's
# own first sample. "first", not "cold" — see run_mount_leg's own comment
# for why that distinction is the whole point of this leg's naming.
readonly MOUNT_FIRST_SCENARIOS=(open jump-end)

# --- small primitives -----------------------------------------------------

die() {
  echo "perf.sh: error: $*" >&2
  exit 1
}

# kills every tmux session this run created (matched by an EXACT recorded
# name in CREATED_SESSIONS, not by a prefix search — an unanchored
# tmux-list-sessions-then-grep-the-prefix match, this file's previous
# approach, can also match a DIFFERENT run's session whose PID happens to
# extend this run's own PID as a text prefix, e.g. PID 1234's sessions are
# named ress-perf-1234-sN, and PID 12345's ress-perf-12345-sN contains that
# exact substring — a real, if narrow, cross-run collision this exact-name
# list closes entirely, not just narrows), removes every in-flight
# fixture-generation temp file (GENERATED_TEMP_FILES — a
# generate_fixture() write interrupted mid-way, possibly multi-GiB, would
# otherwise leak forever, since nothing else ever names that exact path
# again), and the scratch workdir — the last line of defense on a timeout,
# an error, or ctrl-c, so a failed run never leaves an orphaned pane or a
# giant orphaned temp file behind.
cleanup() {
  local s f
  for s in "${CREATED_SESSIONS[@]}"; do
    tmux kill-session -t "$s" >/dev/null 2>&1 || true
  done
  for f in "${GENERATED_TEMP_FILES[@]}"; do
    rm -f "$f" >/dev/null 2>&1 || true
  done
  rm -rf "$WORKDIR" >/dev/null 2>&1 || true
}

pane_text() {
  tmux capture-pane -t "$1" -p 2>/dev/null
}

pane_row1() {
  pane_text "$1" | head -n1
}

kill_session_quiet() {
  tmux kill-session -t "$1" >/dev/null 2>&1 || true
}

# "now" as a microsecond-resolution integer with no subprocess: bash 5's
# EPOCHREALTIME is always six fractional digits, so stripping the decimal
# separator is a safe fixed-point conversion. used for the reported
# elapsed time only — the poll loops below bound themselves with an
# iteration count instead, so they never fork `date` on the hot path
# (tmux capture-pane already is one subprocess per 10ms tick; a second
# one would only add jitter for nothing).
#
# strips every non-digit character, not a literal '.': bash formats
# EPOCHREALTIME with the CURRENT LC_NUMERIC locale's own decimal
# separator, which is a comma rather than a period under some locales
# (confirmed directly, LC_NUMERIC=de_DE.UTF-8) — a hardcoded '.' pattern
# simply does not match there, silently leaving the comma in place. the
# failure this causes downstream is WORSE than the clean abort it might
# look like at a glance: bash arithmetic's `,` is a real operator (the C
# comma expression — evaluate left, discard it, evaluate right), so
# `10#1737034567,891234` does not error, it silently DISCARDS the whole-
# seconds part and evaluates to just `891234` — confirmed directly against
# every call-site shape in this file (`10#$t0_us + timeout_s * 1000000`
# degenerates to a tiny, garbage "deadline" a handful of seconds in
# magnitude; `10#$(now_us) < deadline_us` then compares that garbage
# against a comparably tiny, similarly-mangled "now", which — since every
# timeout this file uses is several seconds — is USUALLY still true,
# meaning a real hang would never time out at all, not a diagnosable
# abort). check_preconditions already pins LC_NUMERIC=C before this can
# ever run under anything else — this is the second, independent half of
# that fix: correct even if something downstream of check_preconditions
# ever overrides LC_NUMERIC again, and byte-identical to the old
# literal-'.' behavior on every locale that already used '.' (a bash glob
# pattern, not a regex — [!0-9] is "any character that is not a digit",
# matching and removing the separator whatever character it is, and
# nothing else, since EPOCHREALTIME's only other characters are digits).
now_us() {
  echo "${EPOCHREALTIME//[!0-9]/}"
}

# polls tmux capture-pane -p every 10ms until the pane's first row is
# non-blank (capture-pane returns an empty string for an unpainted row —
# verified against a real blank pane, not assumed). prints elapsed ms since
# $2 on success; returns 1 on timeout. this is the harness's core
# methodology primitive, also reused as a bare readiness gate wherever a
# scenario needs the pager responsive before it sends a key — ress
# silently drops input sent before it enables raw mode (confirmed
# empirically: sending a goto command immediately after launch, with no
# wait, lands nowhere; the same command sent after first paint lands
# correctly every time), and the same wait is applied to less too so
# neither pager gets a head start the other does not.
#
# the deadline below (and in every other wait_* function) is ABSOLUTE from
# $t0_us, not a fresh $timeout_s counted from whenever this function
# happens to be called: deep-goto's ress leg chains wait_first_paint then
# wait_top_line on the SAME t0, and less's single wait_top_line covers the
# whole sample from the same t0 — if a chained wait restarted its own
# fresh timeout_s on every call, ress could accumulate PAINT_TIMEOUT_S +
# SCENARIO_TIMEOUT_S of total budget from launch while less only ever gets
# SCENARIO_TIMEOUT_S, a real advantage one pager gets and the other does
# not. an absolute deadline (t0_us + timeout_s) makes a late-starting wait
# correctly get only what is left of its sample's total budget.
wait_first_paint() {
  local session="$1" t0_us="$2" timeout_s="$3"
  local deadline_us=$(( 10#$t0_us + timeout_s * 1000000 ))
  local row1
  while (( 10#$(now_us) < deadline_us )); do
    # `|| true`, defensively: this function is only ever called through a
    # command substitution (run_sample's `ms="$("$wait1" ...)" || ...`),
    # and bash suspends errexit propagation for commands running INSIDE a
    # $(...) subshell unless `shopt -s inherit_errexit` is set (not set
    # here) — verified directly: an isolated function with this exact
    # unguarded shape, invoked the same way run_sample invokes this one,
    # polls a genuinely-gone target to completion and returns normally,
    # never aborting the outer script. so this call site is already safe
    # as wired today; the `|| true` makes that safety a property of the
    # function itself rather than of how it happens to be called — it does
    # not depend on staying invoked exclusively via command substitution.
    # capture-pane failing because the pane died mid-poll (not merely "not
    # painted yet", which succeeds with empty output) folds into the same
    # empty-row1 case that already means "keep polling"; pane_crashed
    # (run_sample's caller) is what actually tells the two apart once this
    # wait gives up.
    row1="$(pane_row1 "$session")" || true
    if [[ -n "$row1" ]]; then
      echo $(( (10#$(now_us) - 10#$t0_us) / 1000 ))
      return 0
    fi
    sleep "$POLL_S"
  done
  return 1
}

# polls until $needle appears anywhere in the pane (used for jump-to-end,
# where the target line lands at the BOTTOM of the viewport, not the top —
# verified against a real run rather than assumed symmetric with goto).
# deadline is absolute from t0_us — see wait_first_paint.
wait_pane_contains() {
  local session="$1" needle="$2" t0_us="$3" timeout_s="$4"
  local deadline_us=$(( 10#$t0_us + timeout_s * 1000000 ))
  while (( 10#$(now_us) < deadline_us )); do
    if pane_text "$session" | grep -qF -- "$needle"; then
      echo $(( (10#$(now_us) - 10#$t0_us) / 1000 ))
      return 0
    fi
    sleep "$POLL_S"
  done
  return 1
}

# polls until the pane's top row starts with $needle (goto-line lands the
# target line at the top of the viewport for both pagers — verified: less
# `+N`/`+Ng` and ress `:N` all place it there). deadline is absolute from
# t0_us — see wait_first_paint; this is the function deep-goto chains
# after wait_first_paint on the same t0, so this is where the asymmetry
# actually lived.
wait_top_line() {
  local session="$1" needle="$2" t0_us="$3" timeout_s="$4"
  local deadline_us=$(( 10#$t0_us + timeout_s * 1000000 ))
  local row1
  while (( 10#$(now_us) < deadline_us )); do
    row1="$(pane_row1 "$session")" || true   # see wait_first_paint's comment
    if [[ "$row1" == "$needle"* ]]; then
      echo $(( (10#$(now_us) - 10#$t0_us) / 1000 ))
      return 0
    fi
    sleep "$POLL_S"
  done
  return 1
}

# same as wait_top_line but requires the prefix on two consecutive polls —
# the scroll-cadence scenario's "stabilizes" (a defensive measure against a
# transient/partial redraw landing between two polls; the two pagers see
# the same requirement so neither is favored). deadline is absolute from
# t0_us — see wait_first_paint.
wait_top_line_stable() {
  local session="$1" needle="$2" t0_us="$3" timeout_s="$4"
  local deadline_us=$(( 10#$t0_us + timeout_s * 1000000 ))
  local row1 confirmed=0
  while (( 10#$(now_us) < deadline_us )); do
    row1="$(pane_row1 "$session")" || true   # see wait_first_paint's comment
    if [[ "$row1" == "$needle"* ]]; then
      if (( confirmed )); then
        echo $(( (10#$(now_us) - 10#$t0_us) / 1000 ))
        return 0
      fi
      confirmed=1
    else
      confirmed=0
    fi
    sleep "$POLL_S"
  done
  return 1
}

wait_for_log_line() {
  local logfile="$1" needle="$2" timeout_s="$3"
  # derived from POLL_S (the loop's own sleep interval), not a hardcoded
  # "100" silently assuming POLL_S is 0.01s: a bare `timeout_s * 100` would
  # quietly drift away from the real elapsed timeout the moment POLL_S is
  # ever retuned to a different interval. bash arithmetic has no floating
  # point, hence awk for this one division (already a dependency of this
  # file, e.g. the RSS read below).
  local max_iters i
  max_iters="$(awk -v t="$timeout_s" -v p="$POLL_S" 'BEGIN { printf "%d", t / p }')"
  for (( i = 0; i < max_iters; i++ )); do
    if [[ -f "$logfile" ]] && grep -qF -- "$needle" "$logfile" 2>/dev/null; then
      return 0
    fi
    sleep "$POLL_S"
  done
  return 1
}

# confirms the pane's process is actually the pager we launched, not a
# shell still wrapping it. every launch site execs its pager, but a shell's
# choice to exec its trailing simple command instead of forking one is an
# implementation detail, not a guarantee — this assertion turns a broken
# exec into a loud, immediate abort instead of a silently wrong RSS (and,
# less critically, timing) reading for the shell instead of the pager.
# called only once a wait has SUCCEEDED (a real paint, or a real
# completion, was observed) — but "succeeded a moment ago" is not "still
# true right now": the pane can still have exited in the gap since (real,
# not hypothetical — a comm-faithful fake pager that paints then exits
# immediately, before this assertion runs, reliably lands here). that
# case — pane_pid gone entirely — is therefore a legitimate
# SAMPLE_CRASHED outcome for the caller to record, the exact same
# treatment pane_crashed already gives a vanished pane on the timeout
# side (see just below); returned as a plain failure, not a die(), and
# the caller (run_sample) is the one that decides what a failure here
# means, same as it already does for pane_crashed's own return. a LIVE
# pane running the WRONG process stays a hard die(), unconditionally:
# the paint this file's own wait just confirmed came from something
# alive and responsive, so if that something is not $pager, the launch
# chain itself is broken (e.g. a missing exec letting a wrapping shell's
# own output land first) — a harness bug, never a subject outcome, and
# categorically different from the pane simply being gone.
assert_pane_is() {
  local session="$1" expected="$2" pid comm
  pid="$(tmux list-panes -t "$session" -F '#{pane_pid}' 2>/dev/null || true)"
  [[ -n "$pid" ]] || return 1
  comm="$(cat "/proc/$pid/comm" 2>/dev/null || true)"
  [[ "$comm" == "$expected" ]] \
    || die "session $session pane runs '$comm', not '$expected' (missing exec in the launch string?)"
}

# the timeout-side counterpart to assert_pane_is: called only once a wait
# has TIMED OUT, to tell a subject that is still running but slow (a real
# hang) from one that is simply gone (a crash) — this never dies at all,
# since a subject crashing is a result to record, not a harness failure
# (assert_pane_is's own no-pane case reaches the identical conclusion the
# identical way, just from the post-success side of a wait rather than
# the timeout side — only a LIVE pane running the wrong process is ever
# fatal there, and this function has no equivalent case to be fatal
# about: a live pane on the timeout path is a hang, not a crash, by
# definition). tmux's own default (remain-on-exit off, never changed by
# this script) destroys a pane's window the instant its sole process exits,
# and destroys the session too once no window is left in it — observed
# directly (a real tmux session running a command that exits quickly, then
# probed): list-panes reliably fails, "can't find window: <name>" if
# another session keeps the server alive, "no server running" if that was
# the last one — either way exit 1 with an empty result, which the existing
# `|| true` already reduces to an empty $pid. display-message was also
# probed and rejected for this: display-message -t <a-dead-target> -p can
# silently SUCCEED against tmux's ambient "current" session instead of
# erroring — an unreliable target-existence check — so this reuses
# list-panes, the same primitive assert_pane_is already relies on.
pane_crashed() {
  local session="$1" expected="$2" pid comm
  pid="$(tmux list-panes -t "$session" -F '#{pane_pid}' 2>/dev/null || true)"
  [[ -n "$pid" ]] || return 0
  comm="$(cat "/proc/$pid/comm" 2>/dev/null || true)"
  [[ "$comm" == "$expected" ]] && return 1
  return 0
}

# reads the whole fixture into the page cache before any timed sample — an
# explicit, disclosed prewarm removes "which scenario happens to run first
# against a just-generated, maybe-still-cold file" as a source of bias:
# every scenario starts its timed samples from the same warm baseline the
# warm-cache methodology (see docs/perf.md) already assumes, rather than
# inheriting whatever warmth an earlier scenario happened to leave behind.
prewarm_fixture() {
  local fixture="$1"
  echo "prewarming: $fixture" >&2
  cat "$fixture" > /dev/null
}

# median of N integers (quick mode's N=2 averages the two values, the usual
# convention for an even-sized sample).
median() {
  local -a vals=("$@")
  local n=${#vals[@]}
  (( n > 0 )) || { echo ""; return 0; }
  local sorted
  sorted="$(printf '%s\n' "${vals[@]}" | sort -n)"
  local -a arr=()
  while IFS= read -r v; do arr+=("$v"); done <<<"$sorted"
  local mid=$(( n / 2 ))
  if (( n % 2 == 1 )); then
    echo "${arr[$mid]}"
  else
    echo $(( (arr[mid - 1] + arr[mid]) / 2 ))
  fi
}

# summarizes one pager's samples for a metric. $1/$2 are how many of the
# samples hung at each of the harness's two ceilings — PAINT_TIMEOUT_S and
# SCENARIO_TIMEOUT_S, read directly off the globals rather than threaded
# through every call site, since those are the only two ceilings any wait
# in this script ever uses. each hang is bucketed by WHICH ceiling actually
# fired for that sample (run_sample's SAMPLE_HUNG_CEILING — see its header
# comment), not assumed from the scenario's nominal ceiling: a scenario
# that sends keys can hang at either its readiness wait (paint) or its
# completion wait (scenario), and reporting the wrong one mislabels a
# pre-paint hang as having waited the full scenario budget. $3 is how many
# crashed (run_sample's SAMPLE_CRASHED — a subject that died or changed
# comm on a wait timeout, a distinct outcome from hung, never folded into
# it). $4 is the total sample count, and the rest are the completed
# samples' values. a hang or a crash is a recorded result, not a script
# failure (see docs/perf.md), so this always produces a value for the
# table/TSV instead of an empty or fabricated one: zero failures -> the
# plain median, unchanged from before hangs were tracked; a majority
# failed (including all of them) -> a token naming every cause that
# actually fired and, when more than one did, how many samples each one
# claimed — bare (e.g. "hung(>Ns)", "crashed") when the whole majority
# shares one cause, since a per-cause count would be redundant with "runs"
# in that case; bucketed (e.g. "hung(>30s:2,crashed:1)") otherwise — a
# median built from a minority of samples that happened to finish cannot
# honestly stand in for a result that mostly never occurred; otherwise (a
# minority failed) -> the median of the samples that DID complete,
# annotated with how many hung and/or crashed, e.g. "42(1h)", "42(1c)",
# "42(1h,1c)" — no real number is ever silently absorbed into another.
# $4 (total) is expected to equal hung+crashed+len(vals) — every caller
# accounts for every one of its samples in exactly one of those three
# places — so total==0 means every count and vals are ALSO empty: nothing
# happened here at all (unreachable for a timing row, whose total is
# always the run count; reachable for an RSS row's own, narrower total —
# see report_rss_row), reported as the literal token "no-data" rather
# than median()'s own blank string for an empty array.
summarize() {
  local hung_paint="$1" hung_scenario="$2" crashed="$3" total="$4"
  shift 4
  local -a vals=("$@")
  local hung=$(( hung_paint + hung_scenario ))
  local failed=$(( hung + crashed ))
  if (( total == 0 )); then
    echo "no-data"
    return
  fi
  if (( failed == 0 )); then
    median "${vals[@]}"
    return
  fi
  if (( failed * 2 > total )); then
    if (( crashed == 0 && hung_paint > 0 && hung_scenario == 0 )); then
      echo "hung(>${PAINT_TIMEOUT_S}s)"
    elif (( crashed == 0 && hung_scenario > 0 && hung_paint == 0 )); then
      echo "hung(>${SCENARIO_TIMEOUT_S}s)"
    elif (( hung == 0 )); then
      echo "crashed"
    else
      local -a parts=()
      (( hung_paint > 0 )) && parts+=(">${PAINT_TIMEOUT_S}s:${hung_paint}")
      (( hung_scenario > 0 )) && parts+=(">${SCENARIO_TIMEOUT_S}s:${hung_scenario}")
      (( crashed > 0 )) && parts+=("crashed:${crashed}")
      local joined
      joined="$(IFS=,; echo "${parts[*]}")"
      echo "hung(${joined})"
    fi
    return
  fi
  if (( crashed == 0 )); then
    echo "$(median "${vals[@]}")(${hung}h)"
  elif (( hung == 0 )); then
    echo "$(median "${vals[@]}")(${crashed}c)"
  else
    echo "$(median "${vals[@]}")(${hung}h,${crashed}c)"
  fi
}

add_row() {
  TABLE_ROWS+=("$1"$'\t'"$2"$'\t'"$3"$'\t'"$4"$'\t'"$5")
}

add_rss_row() {
  RSS_ROWS+=("$1"$'\t'"$2"$'\t'"$3"$'\t'"$4")
}

# add_rss_row's own honesty layer, called instead of add_rss_row directly
# by every RSS-reporting call site. an RSS reading is an opportunistic
# extra read taken right after a completed sample (run_sample's --rss),
# and can race the pager's own exit — the process may leave the target
# line and tear its pane down before /proc/<pid>/status is even read. a
# sample that races this way is neither hung nor crashed: its jump
# completed, exactly what the timing row already reports, so a missing
# RSS reading never feeds back into hung/crashed bucketing (hung_paint/
# hung_scenario/crashed here are the SAME counts the timing row for these
# same samples uses) — it simply has nothing to contribute HERE. the
# "runs" this reports is therefore hung+crashed+(readings actually
# collected), never the caller's raw sample count — the timing row and
# the RSS row for the identical samples can honestly show different
# "runs" when some readings raced away, rather than the RSS row silently
# claiming a run it has no value for. summarize()'s own total==0 case
# (every count and $vals empty — every sample raced away with nothing to
# show for it here) reports "no-data" rather than a blank.
report_rss_row() {
  local label="$1" pager="$2" hung_paint="$3" hung_scenario="$4" crashed="$5"
  shift 5
  local -a vals=("$@")
  local total=$(( hung_paint + hung_scenario + crashed + ${#vals[@]} ))
  add_rss_row "$label" "$pager" "$(summarize "$hung_paint" "$hung_scenario" "$crashed" "$total" "${vals[@]}")" "$total"
}

# prints $1 (a tab-separated header) followed by the rows in "$@", each
# column padded to its widest value — no `column` dependency.
print_aligned_table() {
  local header="$1"
  shift
  local -a rows=("$header" "$@")
  local -a widths=()
  local row i len
  for row in "${rows[@]}"; do
    IFS=$'\t' read -r -a fields <<<"$row"
    for i in "${!fields[@]}"; do
      len=${#fields[$i]}
      if [[ -z "${widths[$i]:-}" ]] || (( len > widths[i] )); then
        widths[i]=$len
      fi
    done
  done
  for row in "${rows[@]}"; do
    IFS=$'\t' read -r -a fields <<<"$row"
    local line=""
    for i in "${!fields[@]}"; do
      line+="$(printf '%-*s' "${widths[$i]}" "${fields[$i]}")  "
    done
    printf '%s\n' "$line"
  done
}

# `bytes=<n> lines=<n>` is ress-filegen's whole stderr — read is exact, not
# a grep, so a malformed stats file fails loudly instead of returning "".
read_lines_from_stats() {
  local bytes_kv lines_kv
  read -r bytes_kv lines_kv < "$1"
  echo "${lines_kv#lines=}"
}

# generates $1 via ress-filegen's $2 preset with the remaining args, unless
# both the fixture and its .stats sidecar already exist — keyed by filename,
# never regenerated silently. both the data file and its .stats sidecar are
# written through a same-directory temp file and atomically mv'd into place,
# data first and .stats last, so a completeness check of "both files exist"
# is actually true: a run interrupted mid-generation leaves either neither
# file (killed before the data mv) or the data file with no .stats (killed
# after) — never a half-written file sitting at the final name. that second
# case is a hard abort, not a silent overwrite: an existing data file with no
# .stats is never assumed to be our own regenerable leftover — it could be a
# real file that happens to collide with the name, especially on a shared
# mount — so it is left exactly as found for a human to inspect.
generate_fixture() {
  local out="$1" preset="$2"
  shift 2
  local stats="${out}.stats"
  if [[ -f "$out" && -f "$stats" ]]; then
    echo "fixture present, reusing: $out" >&2
    return 0
  fi
  if [[ -f "$out" && ! -f "$stats" ]]; then
    die "partial fixture pair, never auto-overwritten: $out exists but $stats does not (inspect/remove $out by hand, then re-run)"
  fi
  if [[ ! -f "$out" && -f "$stats" ]]; then
    die "partial fixture pair, never auto-overwritten: $stats exists but $out does not (inspect/remove $stats by hand, then re-run)"
  fi
  echo "generating fixture: $out (preset=$preset $* --seed 42)" >&2
  mkdir -p "$(dirname "$out")"
  local tmp_out="${out}.tmp.$$" tmp_stats="${stats}.tmp.$$"
  # registered BEFORE the write starts (not after — the write itself is the
  # long, interruptible part, potentially multi-GiB for the large presets):
  # an interrupt mid-write leaves exactly these two paths as the orphans
  # cleanup() needs to know about, since nothing else ever names them again.
  GENERATED_TEMP_FILES+=("$tmp_out" "$tmp_stats")
  "$FILEGEN_BIN" --preset "$preset" "$@" --seed 42 "$tmp_out" 2>"$tmp_stats"
  mv "$tmp_out" "$out"
  mv "$tmp_stats" "$stats"
}

# builds $pager's own argv for $fixture, into the global array PAGER_ARGV —
# called only from run_sample (which prepends the built environment and
# wraps the whole thing for tmux, see its header comment), so this is the
# one thing that actually varies between a less sample and a ress sample.
# every element lands in the array directly, never through a string that
# something else has to re-parse — a fixture path containing spaces or
# apostrophes needs
# no escaping here or anywhere downstream, since run_sample hands this
# array to tmux new-session as separate trailing arguments, which tmux
# execve()s directly rather than interpreting through a shell (verified:
# tmux-1(1)'s own documented behavior for a shell-command given as multiple
# arguments, "This can avoid issues with shell quoting"). less always gets
# -S: ress has no wrap mode at all (only chop), so leaving less's default
# wrap on would give it different, not equivalent, work on the long-line
# fixtures. $extra_args, if given, is inserted after -S as one argv element
# — deep-goto's less leg uses it for the cold-start "+$target" jump flag;
# nothing else does.
pager_cmd() {
  local pager="$1" fixture="$2" extra_args="${3:-}"
  case "$pager" in
    less)
      # $LESS_BIN, not the bare word "less": resolved once, client-side, in
      # check_preconditions — see its comment for why a bare "less" here
      # would be looked up again at pane-launch time against tmux's
      # SERVER environment, which can differ from this script's own.
      if [[ -n "$extra_args" ]]; then
        PAGER_ARGV=("$LESS_BIN" -S "$extra_args" "$fixture")
      else
        PAGER_ARGV=("$LESS_BIN" -S "$fixture")
      fi
      ;;
    ress) PAGER_ARGV=("$RESS_BIN" "$fixture") ;;
    *) die "unknown pager: $pager" ;;
  esac
}

# runs $RUNS samples of one local (both-pagers) scenario, alternating pager
# order per sample index — this loop used to be hand-copied into every
# scenario function; it now lives here, once, and every scenario drives it
# through $callback (a function name — bash's nearest thing to a closure:
# the callback is expected to be a nested function defined inside the
# scenario that called for_each_sample, so it can stash results straight
# into that scenario's own local arrays/counters without any of them
# needing to be threaded through here). called as "$callback" pager i.
#
# $callback is called as a BARE statement, so under this file's `set -e`
# its own return status matters: bash a function returns whatever its last
# executed command returned, and a trailing "guard && action" (or a `for`
# loop's last statement being one) returns the GUARD's own status whenever
# the guard is false — a real bug this refactor shipped once already (a
# successful, non-hung sample's callback ended in "$SAMPLE_HUNG && echo
# ...", so a normal sample made the callback return 1 and set -e aborted
# the whole run on the very first success). every $callback must end in a
# plain statement or a proper if/fi, never a bare "cond && action", for
# exactly this reason.
for_each_sample() {
  local callback="$1" i pager
  for (( i = 1; i <= RUNS; i++ )); do
    # alternates which pager runs first per sample index, so whichever
    # pager would otherwise systematically benefit from the other's
    # residual warmth (page cache, cpu cache, scheduler state) does not —
    # the advantage rotates and cancels out across the reported median.
    # an even RUNS keeps the two positions exactly balanced (see main()'s
    # RUNS comment); this is the one place that alternation is decided.
    local -a order=(less ress)
    (( i % 2 == 0 )) && order=(ress less)
    for pager in "${order[@]}"; do
      "$callback" "$pager" "$i"
    done
  done
}

# reads the tracing wrapper's start-of-exec stamp and the binary's own
# "perf: first paint" tracing line, and returns their delta in ms — the
# ress-only precise number run_sample captures under --tracing. the fmt
# subscriber's line looks like (verified against a real run):
#   2026-07-14T07:14:04.905722Z  INFO perf: first paint
# field 1 is the RFC3339 timestamp; GNU date -d parses its fractional
# seconds directly into %s%N (nanoseconds — date has no direct
# microsecond format), then /1000 brings it to microseconds to match the
# wrapper's own stamp — the wrapper stamps in microseconds now, not
# nanoseconds ($EPOCHREALTIME, not `date +%s%N` — see run_sample's
# tracing launch_argv comment for why), this file's one established
# elapsed-time unit (now_us(), the wait primitives) rather than a second
# one introduced just for this function. this `date` call itself is
# unaffected by the tracing wrapper's own PATH concerns (it runs in this
# script's own process, which has this invocation's normal PATH, not the
# wrapper's env -i one) — nothing to eliminate here, only the unit to
# realign with the stamp it is compared against.
_tracing_precise_ms() {
  local stamp="$1" logf="$2"
  local start_us line_ts tracing_us
  start_us="$(cat "$stamp")"
  line_ts="$(grep 'perf: first paint' "$logf" | tail -n1 | awk '{print $1}')"
  tracing_us=$(( $(date -d "$line_ts" +%s%N) / 1000 ))
  echo $(( (tracing_us - start_us) / 1000 ))
}

# --- the sample primitive ------------------------------------------------
#
# run_sample is the one place that owns the protocol every timed sample
# follows. the six review rounds before this refactor each landed a fix at
# a variation point where some scenario function hand-rolled a piece of
# this protocol slightly differently from the others; the invariants below
# are now upheld HERE, in exactly one place, and this comment is the
# contract a future finding gets checked against:
#
#   - clock-before-spawn: $t0 is captured immediately before tmux
#     new-session, identically for every pager, so tmux's own spawn cost
#     is included the same way on both sides of any comparison.
#   - even-count alternation: which pager goes first alternates by sample
#     index, decided in exactly one place (for_each_sample above) — no
#     longer re-derived per scenario. see main()'s RUNS comment for why an
#     even count matters.
#   - whitelist: every launch's environment is BUILT from exactly
#     ENV_WHITELIST_STATIC's entries (env -i, then only those) plus
#     HOME/XDG_CONFIG_HOME/XDG_DATA_HOME pointed at a real, empty, per-run
#     directory (FAKE_HOME) — never filtered down from whatever the
#     invoking shell happened to export. a variable this file has never
#     heard of (this round's own motivating case: LESSKEY_CONTENT,
#     undocumented in less(1)'s own manual) is excluded by construction,
#     the same as one it has — for both pagers symmetrically, and never
#     through a string a fixture's own characters could break out of
#     (env_argv is an array; see pager_cmd's comment for the same
#     discipline applied to the pager's own argv).
#   - exec: tmux new-session's trailing arguments are handed to it as a
#     real argv, not a joined string — tmux execve()s the first one (`env`,
#     or `bash -c ...` for --tracing) directly, with no shell in between at
#     all for the common case. `env`'s own contract is to exec (not fork)
#     into the command it is given, which is what makes pane_pid land on
#     the pager rather than a wrapping process — not a shell's tail-call
#     optimization (an implementation detail some shells apply to a single
#     trailing simple command, not a guarantee), which the old string-based
#     launch depended on. see the launch_argv construction below.
#   - comm: assert_pane_is confirms the pane is genuinely running the
#     expected pager right after a wait SUCCEEDS (a real paint, or a real
#     completion). the wait's success is already an instant in the past by
#     the time this runs, though, so it splits on what it finds: no pane at
#     all (painted, then died before this check landed — a real, narrow
#     race, not hypothetical) is SAMPLE_CRASHED, exactly like a vanished
#     pane on the timeout side (see below) — a subject outcome, the run
#     continues. a LIVE pane running the WRONG process is a categorically
#     different finding — the pane is alive and just responded, so a
#     mismatch there is unambiguously a harness bug (a broken exec chain),
#     never a subject outcome — and stays a hard abort. a wait that TIMES
#     OUT is different again: pane_crashed revalidates instead, since a
#     timeout alone cannot tell "still running, just slow" from "gone" —
#     see hang-as-result below for what each of those becomes.
#   - atomic fixtures: a sample only ever reads a fixture generate_fixture
#     already produced atomically — run_sample itself never writes one.
#   - partial-pair abort: not this function's concern — enforced once in
#     generate_fixture and in run_mount_leg's preflight.
#   - no-prewarm-on-mount: run_sample never prewarms anything — prewarm is
#     each LOCAL scenario's own explicit, disclosed, pre-loop step; the
#     mount scenarios simply never call it.
#   - hang-as-result: every wait timeout is revalidated with pane_crashed
#     before being trusted. still alive, right comm -> a genuine hang:
#     SAMPLE_HUNG=true, and WHICH ceiling fired is recorded in
#     SAMPLE_HUNG_CEILING, so summarize() can emit an honest per-ceiling
#     token instead of assuming every hang shares one scenario-wide
#     ceiling. dead, or the wrong comm -> a crash instead: SAMPLE_CRASHED=
#     true, its own distinct outcome, not folded into hung.
#   - no accidental abort on the way there: getting to "revalidate with
#     pane_crashed" at all requires every tmux call ON THE WAY — not just
#     the final wait timeout — to survive a pager dying under it, since
#     this whole file runs under set -e. two independent things make a
#     command safe here: running inside a $(...) command substitution
#     (bash suspends errexit propagation for a subshell's own internal
#     command failures unless `inherit_errexit` is set, which this file
#     does not set — verified directly), which is why wait_first_paint/
#     wait_top_line/wait_top_line_stable's capture-pane read is already
#     safe exactly as called today (`ms="$("$wait1" ...)" || ...`) even
#     before their own defensive `|| true`; or being an if/while CONDITION,
#     exempt by construction (wait_pane_contains' capture-pane-piped-to-
#     grep). a BARE statement in run_sample's own body has neither
#     property — send-keys is exactly that shape, hence its own explicit
#     `if ! tmux send-keys ...; then` (verified directly, both generically
#     and with a deterministic repro: an unguarded send-keys against a
#     session whose pager just crashed returns nonzero and, pre-fix, took
#     the entire harness down with it, silently). a crash can still
#     surface as a deliberate, diagnosable die() in exactly one narrower
#     case than it used to: assert_pane_is, called only once a wait has
#     already SUCCEEDED, now dies only for a LIVE pane running the wrong
#     process — its own no-pane case is SAMPLE_CRASHED like everywhere
#     else (see the comm bullet above for the full split and why the
#     wrong-comm case alone stays intentional rather than becoming a
#     fourth revalidation site). subject to that one exception, a subject
#     hanging or crashing is exactly the measurement open/jump-end/
#     deep-goto exist to catch, so both are recorded results, not a
#     script failure.
#   - Linux gate: not this function's concern — enforced once in
#     check_preconditions before any sample runs.
#   - forced target dir: not this function's concern — enforced once in
#     main(), before the release build, via CARGO_TARGET_DIR.
#   - teardown: the tmux session is killed on every path out of this
#     function — paint hang, completion hang, crash, or a finished sample.
#
# required positional args: pager fixture
#   run_sample builds the pager's own argv itself (via pager_cmd() above,
#   given pager/fixture/--extra-args) and prepends the built environment —
#   callers never assemble a command themselves, so there is no string
#   anywhere in this path a fixture's own characters could break out of.
#
# options (all optional):
#   --extra-args STR       inserted into the pager's argv as pager_cmd()
#                          describes — only deep-goto's less leg uses this,
#                          for its cold-start "+$target" jump flag.
#   --tracing              ress-only precise first-paint timing: stamps
#                          start_us just before exec and reads the
#                          binary's own tracing line after paint (see
#                          _tracing_precise_ms). never combined with --keys
#                          by any current scenario (tracing is open-only,
#                          and open never sends keys) but captured right
#                          after the readiness wait succeeds regardless,
#                          since the paint moment is unambiguous whether or
#                          not a completion wait follows it.
#   --keys ARG...          tmux send-keys arguments sent once the readiness
#                          wait succeeds; omitted entirely for a paint-only
#                          sample (open; less's deep-goto leg, where the
#                          jump is baked into the launch instead).
#   --wait1 FN             the readiness wait — defaults to wait_first_paint,
#   --wait1-needle N       correct for every scenario that sends keys
#   --wait1-ceiling S      (ress drops input sent before it paints, so this
#                          is a correctness gate, not a choice) and also
#                          correct for a paint-only sample. a --keys-less
#                          scenario with a DIFFERENT single wait (less's
#                          deep-goto leg, where +N on the command line
#                          makes "painted" and "landed" the same event)
#                          overrides these directly instead of taking the
#                          default. default ceiling: $PAINT_TIMEOUT_S.
#   --wait2 FN             the completion wait — only meaningful with
#   --wait2-needle N       --keys; run after --keys is sent.
#   --wait2-ceiling S
#   --timing-basis MODE   launch (default) or keypress. launch: elapsed
#                          time is measured from $t0, i.e. process launch —
#                          the correct basis whenever the reported number's
#                          whole point is "time since this pager started"
#                          (deep-goto, where ress's number must honestly
#                          include its line-1-flash cost that less's does
#                          not — see docs/perf.md). keypress: elapsed time
#                          is measured from just before --keys is sent —
#                          the readiness wait's own time is a gate, not
#                          part of the reported number (jump-end,
#                          scroll-cadence).
#   --rss                  sample RSS from /proc/<pid>/status once a
#                          completion wait succeeds.
#
# outputs (globals, valid once this function returns — it always returns
# 0; a hang or a crash is a recorded result, never this function's own
# failure). SAMPLE_HUNG and SAMPLE_CRASHED are mutually exclusive — a
# timeout is revalidated into exactly one of the two, never both:
#   SAMPLE_HUNG          true|false
#   SAMPLE_HUNG_CEILING  the ceiling (seconds) that fired, if hung
#   SAMPLE_CRASHED       true|false — the pane died or changed comm on a
#                        wait timeout (see pane_crashed), or vanished
#                        entirely in the instant after a wait SUCCEEDED
#                        (see assert_pane_is) — a wait-SUCCESS mismatch is
#                        only ever a hard abort when the pane is still
#                        alive and running the wrong process
#   SAMPLE_MS            elapsed ms, if not hung and not crashed
#   SAMPLE_PRECISE_MS    tracing-precise elapsed ms, if --tracing and not
#                        hung and not crashed
#   SAMPLE_RSS           KiB, if --rss and not hung, not crashed, and the
#                        /proc read succeeded
run_sample() {
  local pager="$1" fixture="$2"
  shift 2
  local tracing=false rss=false extra_args=""
  local wait1="wait_first_paint" wait1_needle="" wait1_ceiling="$PAINT_TIMEOUT_S"
  local wait2="" wait2_needle="" wait2_ceiling=""
  local -a keys=()
  local timing_basis="launch"
  while (( $# > 0 )); do
    case "$1" in
      --extra-args) extra_args="$2"; shift 2 ;;
      --tracing) tracing=true; shift ;;
      --rss) rss=true; shift ;;
      --wait1) wait1="$2"; shift 2 ;;
      --wait1-needle) wait1_needle="$2"; shift 2 ;;
      --wait1-ceiling) wait1_ceiling="$2"; shift 2 ;;
      --wait2) wait2="$2"; shift 2 ;;
      --wait2-needle) wait2_needle="$2"; shift 2 ;;
      --wait2-ceiling) wait2_ceiling="$2"; shift 2 ;;
      --timing-basis) timing_basis="$2"; shift 2 ;;
      --keys)
        shift
        keys=()
        while (( $# > 0 )) && [[ "$1" != --* ]]; do
          keys+=("$1")
          shift
        done
        ;;
      *) die "run_sample: unknown option: $1" ;;
    esac
  done

  # PAGER_ARGV (a global — see pager_cmd's own comment) is consumed
  # immediately below, before any other pager_cmd call (e.g. a future
  # sample) can overwrite it.
  pager_cmd "$pager" "$fixture" "$extra_args"

  SAMPLE_HUNG=false
  SAMPLE_HUNG_CEILING=""
  SAMPLE_CRASHED=false
  SAMPLE_MS=""
  SAMPLE_PRECISE_MS=""
  SAMPLE_RSS=""

  SAMPLE_SEQ=$(( SAMPLE_SEQ + 1 ))
  local session="${SESSION_PREFIX}-s${SAMPLE_SEQ}"
  # recorded by EXACT name, not just matched by prefix later — see
  # cleanup()'s comment for why an unanchored prefix match is unsafe.
  CREATED_SESSIONS+=("$session")

  # HOME/XDG_CONFIG_HOME/XDG_DATA_HOME point at FAKE_HOME (a real, empty,
  # per-run directory — see main()), extending the static whitelist with
  # the one part that cannot be a readonly constant: it needs FAKE_HOME,
  # which does not exist until main() creates it.
  local -a env_argv=(
    "${ENV_WHITELIST_STATIC[@]}"
    "HOME=$FAKE_HOME"
    "XDG_CONFIG_HOME=$FAKE_HOME/.config"
    "XDG_DATA_HOME=$FAKE_HOME/.local/share"
  )

  # every element below is a genuine argv entry, assembled as an array and
  # handed to tmux new-session as separate trailing arguments — never
  # concatenated into a string for a shell to re-parse (see pager_cmd's
  # comment for why that matters). BOTH cases put `env` as the OUTERMOST
  # exec'd process — tmux execve()s it directly — so the built environment
  # (nothing BASH_ENV or anything else uninvited, see ENV_WHITELIST_STATIC's
  # comment) is already in effect for every later process in the chain
  # from the moment it starts, not merely by the time it happens to read
  # its own argv. the non-tracing case needs nothing past that: env's own
  # exec into the pager (not a fork) is the pager's entire launch, no
  # explicit "exec" keyword to write or forget. the tracing case DOES
  # need a shell after env (it
  # must stamp a file before the pager starts), so env's final exec
  # target is `"$BASH_BIN" -c 'exec "$@"'` instead of the pager directly —
  # bash itself then starts already under the built environment env just
  # built, the same guarantee the non-tracing case gets for free.
  # $BASH_BIN, not the bare word "bash": ENV_WHITELIST_STATIC's env -i
  # leaves no PATH at all for env's own execvp()-style resolution of a
  # bare command to fall back on — see check_preconditions' comment for
  # the fallback this was silently, host-dependently relying on instead.
  # an earlier version instead put bash outermost and threaded env_argv in
  # as positional arguments to the INNER exec — every value still reached
  # the pager correctly, but bash's own startup, before that inner exec
  # ever ran, was unscrubbed: a BASH_ENV present in the environment tmux's
  # pane naturally inherits (the tmux server's own ambient environment,
  # not this scrub) is sourced by bash before it runs a single line of
  # its own "-c" script, a hook that prints or sleeps there corrupting
  # wait_first_paint's detection or the tracing stamp, sometimes worse
  # (confirmed directly: assert_pane_is hard die()d on a pane caught
  # mid-exec-chain). the script text itself is fixed and contains no
  # interpolated values (nothing to quote wrong); the stamp path and the
  # pager's own argv flow through as real positional parameters via "$@",
  # the same array-not-string discipline as the non-tracing path — env_argv
  # and RESS_LOG do not need to flow through as arguments at all now, only
  # as the environment bash (and everything after it) already starts under.
  local -a launch_argv
  if $tracing; then
    local rundir="$WORKDIR/sample-$SAMPLE_SEQ"
    mkdir -p "$rundir"
    local stamp="$rundir/start_us" logf="$rundir/ress.log"
    launch_argv=(
      "${env_argv[@]}" "RESS_LOG=info"
      # no external `date` call: the wrapper IS bash 5, so it reads its
      # own $EPOCHREALTIME directly (a builtin, not an inherited env var —
      # available regardless of what ENV_WHITELIST_STATIC's env -i left
      # out) instead of forking a process whose own resolution would hit
      # the identical bare-word-under-env–i problem $BASH_BIN above just
      # solved for bash itself — one external-tool dependency avoided
      # instead of chased. the same radix-agnostic digit extraction
      # now_us() uses (strip everything that is not 0-9, not a literal
      # '.') stamps microseconds, not nanoseconds — matching this file's
      # one established elapsed-time unit throughout (now_us(), the wait
      # primitives) rather than introducing a second one just for this
      # wrapper; _tracing_precise_ms's own parsing was updated to match
      # (see its comment).
      "$BASH_BIN" -c 'printf %s "${EPOCHREALTIME//[!0-9]/}" > "$1"; shift; exec "$@"' _
      "$stamp" "${PAGER_ARGV[@]}" --log-file "$logf"
    )
  else
    launch_argv=("${env_argv[@]}" "${PAGER_ARGV[@]}")
  fi

  local t0 ms wait1_ok=true
  t0="$(now_us)"
  tmux new-session -d -s "$session" -x "$PANE_COLS" -y "$PANE_ROWS" "${launch_argv[@]}"

  if [[ -n "$wait1_needle" ]]; then
    ms="$("$wait1" "$session" "$wait1_needle" "$t0" "$wait1_ceiling")" || wait1_ok=false
  else
    ms="$("$wait1" "$session" "$t0" "$wait1_ceiling")" || wait1_ok=false
  fi
  if ! $wait1_ok; then
    # a timeout, not by itself a harness failure — but not by itself proof
    # of a hang either: revalidate before trusting it as one (see
    # pane_crashed's comment for why polling alone cannot make this call).
    if pane_crashed "$session" "$pager"; then
      SAMPLE_CRASHED=true
    else
      SAMPLE_HUNG=true
      SAMPLE_HUNG_CEILING="$wait1_ceiling"
    fi
    kill_session_quiet "$session"
    return 0
  fi
  # the wait SUCCEEDED (a real paint landed) an instant ago — but the pane
  # can still have exited in the gap since then, and that gap is real: a
  # pager that painted once and immediately died before this exact check
  # runs is a legitimate, if narrow, race (see assert_pane_is's own
  # comment). no pane at all here is therefore recorded as a crash and
  # the run continues, the identical treatment the timeout path already
  # gives a vanished pane via pane_crashed; a comm mismatch against a
  # STILL-LIVE pane, though, is unambiguously a harness bug, not a
  # subject outcome, and stays a hard abort, before any key is sent and
  # before this sample's timing is trusted at all.
  if ! assert_pane_is "$session" "$pager"; then
    SAMPLE_CRASHED=true
    kill_session_quiet "$session"
    return 0
  fi

  if $tracing; then
    wait_for_log_line "$logf" "perf: first paint" "$LOG_TIMEOUT_S" \
      || die "ress first-paint log line missing (session $session)"
    SAMPLE_PRECISE_MS="$(_tracing_precise_ms "$stamp" "$logf")"
  fi

  if (( ${#keys[@]} == 0 )); then
    # no completion wait declared: the readiness wait's own success IS
    # this sample's result (open; less's deep-goto leg).
    SAMPLE_MS="$ms"
    kill_session_quiet "$session"
    return 0
  fi

  local t2="$t0"
  [[ "$timing_basis" == keypress ]] && t2="$(now_us)"
  # unlike the wait_*/pane_row1 shape above, this one is a genuine, live set
  # -e hazard: it is a BARE statement in run_sample's own body, not wrapped
  # in a command substitution — so it runs in the real, outer shell, where
  # errexit is not suspended the way it is inside a $(...) subshell. a
  # pager that crashes in the gap between assert_pane_is succeeding and
  # this send-keys call (still possible — the paint assert_pane_is
  # confirmed does not guarantee the process stays alive one instant
  # longer) takes its pane down with it, and tmux send-keys against a
  # now-nonexistent target returns nonzero (verified directly against a
  # real dead session, and against a deterministic repro that kills the
  # session immediately after a real assert_pane_is check succeeds:
  # unguarded, that nonzero status aborts the entire harness right here,
  # silently, bypassing every bit of the crash-as-result machinery below —
  # confirmed both ways, pre- and post-fix). no need to wait out wait2's
  # full ceiling to learn what send-keys' own failure already proved:
  # pane_crashed is consulted immediately instead.
  if ! tmux send-keys -t "$session" "${keys[@]}"; then
    if pane_crashed "$session" "$pager"; then
      SAMPLE_CRASHED=true
    else
      die "session $session: send-keys failed but the pane is still alive and running $pager (not a crash — investigate; tmux send-keys should not fail against a live, correctly identified target)"
    fi
    kill_session_quiet "$session"
    return 0
  fi

  if ms="$("$wait2" "$session" "$wait2_needle" "$t2" "$wait2_ceiling")"; then
    SAMPLE_MS="$ms"
    if $rss; then
      local pid
      pid="$(tmux list-panes -t "$session" -F '#{pane_pid}' 2>/dev/null || true)"
      [[ -n "$pid" ]] && SAMPLE_RSS="$(awk '/VmRSS/{print $2}' "/proc/$pid/status" 2>/dev/null || true)"
    fi
  else
    # same revalidation as wait1's timeout branch above — a subject that
    # crashed AFTER keys were sent (e.g. mid-scroll) would otherwise poll
    # silently to the full completion ceiling and get misreported as hung.
    if pane_crashed "$session" "$pager"; then
      SAMPLE_CRASHED=true
    else
      SAMPLE_HUNG=true
      SAMPLE_HUNG_CEILING="$wait2_ceiling"
    fi
  fi
  kill_session_quiet "$session"
}

# --- scenarios --------------------------------------------------------

# open -> first paint, capture-pane wall clock, for both pagers; ress
# additionally gets a precise tracing-based number from the same launch.
# local fixtures only — the mount leg has its own dedicated, ress-only,
# never-prewarmed scenario functions (run_mount_open_scenario and
# run_mount_jump_end_scenario below), since it needs a different pager
# shape (no less) and a different reporting shape (first/warm split, not a
# plain median) than this function provides.
run_open_scenario() {
  local fixture="$1" label="$2"
  echo "== open: $label ==" >&2
  prewarm_fixture "$fixture"
  local -a less_ms=() ress_cp_ms=() ress_precise_ms=()
  local less_hung=0 ress_hung=0 less_crashed=0 ress_crashed=0
  # open sends no keys: run_sample's readiness wait (its default,
  # wait_first_paint) IS the sample — no --wait2 needed. ress additionally
  # gets --tracing for the precise number; less does not.
  _open_sample() {
    local pager="$1" i="$2"
    echo "   $pager open $i/$RUNS" >&2
    if [[ "$pager" == less ]]; then
      run_sample less "$fixture"
      if $SAMPLE_HUNG; then
        less_hung=$(( less_hung + 1 ))
        echo "   less open $i/$RUNS: hung (no paint within ${SAMPLE_HUNG_CEILING}s)" >&2
      elif $SAMPLE_CRASHED; then
        less_crashed=$(( less_crashed + 1 ))
        echo "   less open $i/$RUNS: crashed (no paint, pane gone)" >&2
      else
        less_ms+=("$SAMPLE_MS")
      fi
    else
      run_sample ress "$fixture" --tracing
      if $SAMPLE_HUNG; then
        ress_hung=$(( ress_hung + 1 ))
        echo "   ress open $i/$RUNS: hung (no paint within ${SAMPLE_HUNG_CEILING}s)" >&2
      elif $SAMPLE_CRASHED; then
        ress_crashed=$(( ress_crashed + 1 ))
        echo "   ress open $i/$RUNS: crashed (no paint, pane gone)" >&2
      else
        ress_cp_ms+=("$SAMPLE_MS")
        ress_precise_ms+=("$SAMPLE_PRECISE_MS")
      fi
    fi
  }
  for_each_sample _open_sample
  add_row "open" "$label" "less" "$(summarize "$less_hung" 0 "$less_crashed" "$RUNS" "${less_ms[@]}")" "$RUNS"
  add_row "open" "$label" "ress" "$(summarize "$ress_hung" 0 "$ress_crashed" "$RUNS" "${ress_cp_ms[@]}")" "$RUNS"
  add_row "open-precise" "$label" "ress" "$(summarize "$ress_hung" 0 "$ress_crashed" "$RUNS" "${ress_precise_ms[@]}")" "$RUNS"
}

# G jump-to-end on an already-responsive pager: timed from the keypress,
# not process launch — this is "does an open pager stay responsive when
# asked to seek to EOF," the classic case less struggles with on huge
# files, isolated from open cost (which run_open_scenario already covers).
# RSS is sampled from the same process right after the jump completes.
# local fixtures only — see run_open_scenario's comment; the mount leg
# uses run_mount_jump_end_scenario below.
run_jump_end_scenario() {
  local fixture="$1" label="$2" stats="$3"
  local lines target_prefix
  lines="$(read_lines_from_stats "$stats")"
  target_prefix="$(printf '%010d' "$lines")"
  echo "== jump-end: $label (last line $lines) ==" >&2
  prewarm_fixture "$fixture"
  local -a less_ms=() ress_ms=() less_rss=() ress_rss=()
  local less_hung_paint=0 less_hung_scenario=0 ress_hung_paint=0 ress_hung_scenario=0
  local less_crashed=0 ress_crashed=0
  # G to an already-painted pager: --keys leaves --wait1 at its default
  # (wait_first_paint — a pager that never paints and a pager that paints
  # but never reaches the target are the same outcome from the outside,
  # so a paint hang and a completion hang both count as "hung" here, just
  # attributed to different ceilings), --timing-basis keypress times only
  # from the "G" send, and --rss samples the same process right after.
  _jump_end_sample() {
    local pager="$1" i="$2"
    echo "   $pager jump-end $i/$RUNS" >&2
    run_sample "$pager" "$fixture" \
      --keys -l "G" \
      --wait2 wait_pane_contains --wait2-needle "$target_prefix" --wait2-ceiling "$SCENARIO_TIMEOUT_S" \
      --timing-basis keypress --rss
    if $SAMPLE_HUNG; then
      if [[ "$pager" == less ]]; then
        if [[ "$SAMPLE_HUNG_CEILING" == "$PAINT_TIMEOUT_S" ]]; then less_hung_paint=$(( less_hung_paint + 1 ))
        else less_hung_scenario=$(( less_hung_scenario + 1 )); fi
      else
        if [[ "$SAMPLE_HUNG_CEILING" == "$PAINT_TIMEOUT_S" ]]; then ress_hung_paint=$(( ress_hung_paint + 1 ))
        else ress_hung_scenario=$(( ress_hung_scenario + 1 )); fi
      fi
      echo "   $pager jump-end $i/$RUNS: hung (no jump-to-end within ${SAMPLE_HUNG_CEILING}s)" >&2
    elif $SAMPLE_CRASHED; then
      if [[ "$pager" == less ]]; then less_crashed=$(( less_crashed + 1 ))
      else ress_crashed=$(( ress_crashed + 1 )); fi
      echo "   $pager jump-end $i/$RUNS: crashed (pane gone before jump-to-end)" >&2
    else
      if [[ "$pager" == less ]]; then
        less_ms+=("$SAMPLE_MS")
        if [[ -n "$SAMPLE_RSS" ]]; then less_rss+=("$SAMPLE_RSS"); fi
      else
        ress_ms+=("$SAMPLE_MS")
        if [[ -n "$SAMPLE_RSS" ]]; then ress_rss+=("$SAMPLE_RSS"); fi
      fi
    fi
    # NOTE: this function is called as a bare statement (for_each_sample's
    # "$callback" "$pager" "$i"), so under set -e its OWN return status
    # matters — bash functions return whatever their last executed command
    # returned. a trailing "guard && action" would make this function
    # return 1 (guard's own status) whenever the guard is false, which
    # set -e treats as a real failure at the call site and aborts the
    # whole script even though nothing actually went wrong (see run_sample's
    # header comment's teardown invariant for the analogous discipline
    # inside the primitive itself). every branch above ends in a plain
    # assignment/append or a proper if/fi for exactly this reason — do not
    # replace one with a bare "cond && action" as this function's tail.
  }
  for_each_sample _jump_end_sample
  add_row "jump-end" "$label" "less" "$(summarize "$less_hung_paint" "$less_hung_scenario" "$less_crashed" "$RUNS" "${less_ms[@]}")" "$RUNS"
  add_row "jump-end" "$label" "ress" "$(summarize "$ress_hung_paint" "$ress_hung_scenario" "$ress_crashed" "$RUNS" "${ress_ms[@]}")" "$RUNS"
  report_rss_row "$label" "less" "$less_hung_paint" "$less_hung_scenario" "$less_crashed" "${less_rss[@]}"
  report_rss_row "$label" "ress" "$ress_hung_paint" "$ress_hung_scenario" "$ress_crashed" "${ress_rss[@]}"
}

# deep goto at 90% of the line count, cold start: timed from PROCESS
# LAUNCH, matching less's own +N ("jump straight there, no line-1 paint
# first") which is a real startup flag; ress has no such flag, so its
# nearest equivalent is open, wait for first paint (unavoidable — see
# wait_first_paint), then ":N" — meaning ress's number here honestly
# includes a line-1 flash less's does not. that asymmetry is a real,
# documented limitation of the comparison (see docs/perf.md), not a bug in
# the harness: it is the actual gap between "less has a jump-on-open flag"
# and "ress does not, yet."
run_deep_goto_scenario() {
  local fixture="$1" label="$2" stats="$3"
  local lines target target_prefix
  lines="$(read_lines_from_stats "$stats")"
  target=$(( lines * 9 / 10 ))
  (( target < 1 )) && target=1
  target_prefix="$(printf '%010d' "$target")"
  echo "== deep-goto: $label (target line $target of $lines) ==" >&2
  prewarm_fixture "$fixture"
  local -a less_ms=() ress_ms=()
  local less_hung_scenario=0 ress_hung_paint=0 ress_hung_scenario=0
  local less_crashed=0 ress_crashed=0
  _deep_goto_sample() {
    local pager="$1" i="$2"
    echo "   $pager deep-goto $i/$RUNS" >&2
    if [[ "$pager" == less ]]; then
      # bare +N (equivalent to +Ng — verified against a real file) is
      # less's own cold-start jump flag, baked straight into the launch:
      # "painted" and "landed" are the same event, so this scenario's only
      # wait is declared as --wait1 directly (no --keys — see run_sample's
      # wait1-default note) at the SCENARIO ceiling, not the paint one.
      run_sample less "$fixture" --extra-args "+$target" \
        --wait1 wait_top_line --wait1-needle "$target_prefix" --wait1-ceiling "$SCENARIO_TIMEOUT_S"
      if $SAMPLE_HUNG; then less_hung_scenario=$(( less_hung_scenario + 1 ))
      elif $SAMPLE_CRASHED; then less_crashed=$(( less_crashed + 1 ))
      else less_ms+=("$SAMPLE_MS"); fi
    else
      # ress has no such flag (see this scenario's header comment on the
      # asymmetry that produces): open, wait for first paint (--keys keeps
      # --wait1 at its default, wait_first_paint), send ":$target" Enter,
      # wait for the same top-line landing. --timing-basis launch keeps
      # the reported number "since process launch" for BOTH pagers, so
      # ress's number honestly includes its line-1 flash.
      run_sample ress "$fixture" \
        --keys ":$target" "Enter" \
        --wait2 wait_top_line --wait2-needle "$target_prefix" --wait2-ceiling "$SCENARIO_TIMEOUT_S" \
        --timing-basis launch
      if $SAMPLE_HUNG; then
        if [[ "$SAMPLE_HUNG_CEILING" == "$PAINT_TIMEOUT_S" ]]; then ress_hung_paint=$(( ress_hung_paint + 1 ))
        else ress_hung_scenario=$(( ress_hung_scenario + 1 )); fi
      elif $SAMPLE_CRASHED; then
        ress_crashed=$(( ress_crashed + 1 ))
      else
        ress_ms+=("$SAMPLE_MS")
      fi
    fi
    # if/fi, not "$SAMPLE_HUNG && echo ...": this function is called as a
    # bare statement (for_each_sample's "$callback" "$pager" "$i"), so
    # under set -e its own return status — the exit status of whatever it
    # last executed — matters at the call site. a trailing "guard && echo"
    # would make a SUCCESSFUL (non-hung) sample, the common case, return 1
    # (the guard's own false status, since echo would never run), which
    # set -e treats as a real failure and aborts the whole script even
    # though nothing went wrong — see _jump_end_sample's identical note.
    if $SAMPLE_HUNG; then
      echo "   $pager deep-goto $i/$RUNS: hung (no goto within ${SAMPLE_HUNG_CEILING}s)" >&2
    elif $SAMPLE_CRASHED; then
      echo "   $pager deep-goto $i/$RUNS: crashed (pane gone before goto)" >&2
    fi
  }
  for_each_sample _deep_goto_sample
  add_row "deep-goto" "$label" "less" "$(summarize 0 "$less_hung_scenario" "$less_crashed" "$RUNS" "${less_ms[@]}")" "$RUNS"
  add_row "deep-goto" "$label" "ress" "$(summarize "$ress_hung_paint" "$ress_hung_scenario" "$ress_crashed" "$RUNS" "${ress_ms[@]}")" "$RUNS"
}

# 100 j's at full speed from the top, timed from the keypress like
# jump-end: sent as one -l literal (both pagers get the same burst rather
# than 100 separate tmux calls each), landing on line 101.
run_scroll_cadence_scenario() {
  local fixture="$1" label="$2"
  local keys target_prefix
  keys="$(printf 'j%.0s' {1..100})"
  target_prefix="$(printf '%010d' 101)"
  echo "== scroll-cadence: $label ==" >&2
  prewarm_fixture "$fixture"
  local -a less_ms=() ress_ms=()
  local less_hung_paint=0 less_hung_scenario=0 ress_hung_paint=0 ress_hung_scenario=0
  local less_crashed=0 ress_crashed=0
  _scroll_cadence_sample() {
    local pager="$1" i="$2"
    echo "   $pager scroll-cadence $i/$RUNS" >&2
    run_sample "$pager" "$fixture" \
      --keys -l "$keys" \
      --wait2 wait_top_line_stable --wait2-needle "$target_prefix" --wait2-ceiling "$SCENARIO_TIMEOUT_S" \
      --timing-basis keypress
    if $SAMPLE_HUNG; then
      if [[ "$pager" == less ]]; then
        if [[ "$SAMPLE_HUNG_CEILING" == "$PAINT_TIMEOUT_S" ]]; then less_hung_paint=$(( less_hung_paint + 1 ))
        else less_hung_scenario=$(( less_hung_scenario + 1 )); fi
      else
        if [[ "$SAMPLE_HUNG_CEILING" == "$PAINT_TIMEOUT_S" ]]; then ress_hung_paint=$(( ress_hung_paint + 1 ))
        else ress_hung_scenario=$(( ress_hung_scenario + 1 )); fi
      fi
      echo "   $pager scroll-cadence $i/$RUNS: hung (no stable line within ${SAMPLE_HUNG_CEILING}s)" >&2
    elif $SAMPLE_CRASHED; then
      if [[ "$pager" == less ]]; then less_crashed=$(( less_crashed + 1 ))
      else ress_crashed=$(( ress_crashed + 1 )); fi
      echo "   $pager scroll-cadence $i/$RUNS: crashed (pane gone mid-scroll)" >&2
    else
      if [[ "$pager" == less ]]; then less_ms+=("$SAMPLE_MS"); else ress_ms+=("$SAMPLE_MS"); fi
    fi
  }
  for_each_sample _scroll_cadence_sample
  add_row "scroll-cadence" "$label" "less" "$(summarize "$less_hung_paint" "$less_hung_scenario" "$less_crashed" "$RUNS" "${less_ms[@]}")" "$RUNS"
  add_row "scroll-cadence" "$label" "ress" "$(summarize "$ress_hung_paint" "$ress_hung_scenario" "$ress_crashed" "$RUNS" "${ress_ms[@]}")" "$RUNS"
}

# --- fixtures, mount leg, output --------------------------------------

# the suffixed on-mount path for $scenario's OWN dedicated first-touch
# fixture — see run_mount_leg for why each MOUNT_FIRST_SCENARIOS entry
# needs one. same preset/size/seed as the label implies (byte-identical
# content to every other scenario's copy), just a distinct inode, so one
# scenario's repeated reads can never warm another's.
mount_fixture_path() {
  local scenario="$1"
  echo "$MOUNT_FIXTURES_DIR/varied-log-256m-${scenario}.log"
}

# the standard set lives here, in one place: the justfile's `fixtures`
# recipe calls this script with --fixtures-only rather than repeating the
# list, so there is exactly one place that knows what a full run needs.
# the mount fixtures are generated ONLY on the --fixtures-only path, never
# on a timed run: writing 256 MiB onto the mount warms the client's (and
# possibly the server's) cache through the write itself — see run_mount_leg
# for exactly how warm, and why nothing unprivileged undoes it — so a run
# that generates a fixture and then immediately times access to it would be
# measuring its own generation-warmth, not a fresh, unprewarmed first read.
# one fixture is generated per MOUNT_FIRST_SCENARIOS entry, not one shared
# file: sharing one file meant whichever scenario ran second inherited the
# first scenario's warmth (background indexing, repeated reads) on what
# was supposed to be ITS OWN never-before-touched-by-this-run sample 1 — an
# order-dependent lie run_mount_leg's preflight and the scenario functions
# below both now assume is impossible by construction. run_mount_leg
# requires every one of these to already exist.
ensure_fixtures() {
  mkdir -p "$FIXTURES_DIR"
  if $QUICK; then
    generate_fixture "$FIXTURES_DIR/varied-log-256m.log" varied-log --mib 256
  else
    generate_fixture "$FIXTURES_DIR/varied-log-2g.log" varied-log --gib 2
  fi
  generate_fixture "$FIXTURES_DIR/megalines-512m.log" megalines --mib 512
  generate_fixture "$FIXTURES_DIR/single-line-256m.log" single-line --mib 256
  if [[ -n "${RESS_PERF_MOUNT:-}" ]]; then
    [[ -d "$RESS_PERF_MOUNT" ]] || die "RESS_PERF_MOUNT is not a directory: $RESS_PERF_MOUNT"
    if $FIXTURES_ONLY; then
      local scenario
      for scenario in "${MOUNT_FIRST_SCENARIOS[@]}"; do
        generate_fixture "$(mount_fixture_path "$scenario")" varied-log --mib 256
      done
    fi
  fi
}

# reports one add_row-shaped metric's first/warm split (see run_mount_leg's
# doc comment for why the split exists at all, and why "first" rather than
# "cold"): $1 scenario name, $2 the bare fixture label (mount-first:/
# mount-warm: is added here), $3 whether sample 1 (the first touch) hung
# ("true"/"false"), $4 whether it crashed instead ("true"/"false" —
# mutually exclusive with $3, see run_sample), $5 the ceiling it hung at
# (SAMPLE_HUNG_CEILING — ignored unless $3), $6 sample 1's own value
# (ignored if hung or crashed), $7/$8 how many of the REMAINING samples
# hung at the paint/scenario ceiling respectively, $9 how many of the
# REMAINING samples crashed, $10 the total sample count, and the rest are
# those remaining (warm) samples' values.
report_mount_row() {
  local scenario="$1" label="$2" first_hung="$3" first_crashed="$4" first_hung_ceiling="$5"
  local first_val="$6" warm_hung_paint="$7" warm_hung_scenario="$8" warm_crashed="$9" total="${10}"
  shift 10
  local -a warm_vals=("$@")
  if [[ "$first_hung" == true ]]; then
    add_row "$scenario" "mount-first:$label" "ress" "hung(>${first_hung_ceiling}s)" 1
  elif [[ "$first_crashed" == true ]]; then
    add_row "$scenario" "mount-first:$label" "ress" "crashed" 1
  else
    add_row "$scenario" "mount-first:$label" "ress" "$first_val" 1
  fi
  local warm_total=$(( total - 1 ))
  # if/fi, not "cond && add_row ...": report_mount_row is called as a bare
  # statement, so under set -e its own return status matters — see
  # _jump_end_sample's note. RUNS=1 makes warm_total 0, and a bare
  # "cond && action" would then make this function return 1 (the false
  # cond's own status) and abort the whole script even though "no warm
  # samples to report" is entirely expected at RUNS=1.
  if (( warm_total > 0 )); then
    add_row "$scenario" "mount-warm:$label" "ress" "$(summarize "$warm_hung_paint" "$warm_hung_scenario" "$warm_crashed" "$warm_total" "${warm_vals[@]}")" "$warm_total"
  fi
}

# same first/warm split as report_mount_row, for the RSS table
# (add_rss_row has no scenario column).
report_mount_rss_row() {
  local label="$1" first_hung="$2" first_crashed="$3" first_hung_ceiling="$4" first_val="$5"
  local warm_hung_paint="$6" warm_hung_scenario="$7" warm_crashed="$8" total="$9"
  shift 9
  local -a warm_vals=("$@")
  if [[ "$first_hung" == true ]]; then
    add_rss_row "mount-first:$label" "ress" "hung(>${first_hung_ceiling}s)" 1
  elif [[ "$first_crashed" == true ]]; then
    add_rss_row "mount-first:$label" "ress" "crashed" 1
  elif [[ -n "$first_val" ]]; then
    add_rss_row "mount-first:$label" "ress" "$first_val" 1
  else
    # completed (neither hung nor crashed — the jump landed, exactly what
    # report_mount_row's timing row for this same sample reports) but the
    # RSS read itself raced the pager's own exit — see report_rss_row's
    # identical reasoning for the warm bucket just below. this sample
    # contributed nothing HERE, so it is not counted as a run here either.
    add_rss_row "mount-first:$label" "ress" "no-data" 0
  fi
  local warm_total=$(( total - 1 ))
  # see report_mount_row's identical note on why this is if/fi, not
  # "cond && add_rss_row ...".
  if (( warm_total > 0 )); then
    report_rss_row "mount-warm:$label" "ress" "$warm_hung_paint" "$warm_hung_scenario" "$warm_crashed" "${warm_vals[@]}"
  fi
}

# ress-only open, first/warm split — see run_mount_leg for why. same launch
# shape as run_open_scenario's ress leg (--tracing for the precise number)
# but no less leg, no pager-order alternation (nothing to alternate with,
# so a plain loop replaces for_each_sample), and no prewarm (run_mount_leg's
# whole point) — run_sample itself never prewarms either way.
run_mount_open_scenario() {
  local fixture="$1" label="$2"
  echo "== open (mount): $label ==" >&2
  local -a warm_cp_ms=() warm_precise_ms=()
  local warm_hung_paint=0 warm_crashed=0
  local first_hung=false first_crashed=false first_hung_ceiling="" first_cp_ms="" first_precise_ms=""
  local i
  for (( i = 1; i <= RUNS; i++ )); do
    echo "   ress open $i/$RUNS" >&2
    run_sample ress "$fixture" --tracing
    if $SAMPLE_HUNG; then
      # open has no completion wait, so a hang here is always the paint
      # ceiling — but SAMPLE_HUNG_CEILING is read anyway, not assumed, for
      # the same reason report_mount_row takes it as a parameter rather
      # than a caller-picked constant.
      if (( i == 1 )); then
        first_hung=true; first_hung_ceiling="$SAMPLE_HUNG_CEILING"
      else
        warm_hung_paint=$(( warm_hung_paint + 1 ))
      fi
      echo "   ress open $i/$RUNS: hung (no paint within ${SAMPLE_HUNG_CEILING}s)" >&2
    elif $SAMPLE_CRASHED; then
      if (( i == 1 )); then
        first_crashed=true
      else
        warm_crashed=$(( warm_crashed + 1 ))
      fi
      echo "   ress open $i/$RUNS: crashed (pane gone before paint)" >&2
    else
      if (( i == 1 )); then
        first_cp_ms="$SAMPLE_MS"; first_precise_ms="$SAMPLE_PRECISE_MS"
      else
        warm_cp_ms+=("$SAMPLE_MS"); warm_precise_ms+=("$SAMPLE_PRECISE_MS")
      fi
    fi
  done
  report_mount_row "open" "$label" "$first_hung" "$first_crashed" "$first_hung_ceiling" "$first_cp_ms" \
    "$warm_hung_paint" 0 "$warm_crashed" "$RUNS" "${warm_cp_ms[@]}"
  report_mount_row "open-precise" "$label" "$first_hung" "$first_crashed" "$first_hung_ceiling" "$first_precise_ms" \
    "$warm_hung_paint" 0 "$warm_crashed" "$RUNS" "${warm_precise_ms[@]}"
}

# ress-only jump-end, first/warm split — see run_mount_leg for why. same
# --keys/--wait2/--rss shape as run_jump_end_scenario's ress leg, no less
# leg, no alternation — but a DIFFERENT timing basis (launch, not
# keypress): see the comment at the run_sample call below for why this
# leg cannot reuse jump-end's own keypress anchor.
run_mount_jump_end_scenario() {
  local fixture="$1" label="$2" stats="$3"
  local lines target_prefix
  lines="$(read_lines_from_stats "$stats")"
  target_prefix="$(printf '%010d' "$lines")"
  echo "== jump-end (mount): $label (last line $lines) ==" >&2
  local -a warm_ms=() warm_rss=()
  local warm_hung_paint=0 warm_hung_scenario=0 warm_crashed=0
  local first_hung=false first_crashed=false first_hung_ceiling="" first_ms="" first_rss=""
  local i
  for (( i = 1; i <= RUNS; i++ )); do
    echo "   ress jump-end $i/$RUNS" >&2
    # --timing-basis launch, NOT keypress (unlike the local jump-end
    # scenario just above): ress's own startup — Document::new spawning
    # the background indexer, the first viewport read for the pane's
    # first paint — already reads this mount-resident file throughout the
    # paint wait, before a single key is ever sent. a keypress-anchored
    # number would silently exclude that first touch from the timer,
    # leaving "mount-first" measuring only a warm continuation of a read
    # this same process already started — not the cold open-and-jump user
    # story this leg exists to capture. one coherent anchor for the whole
    # row (both first and warm samples): every ress jump-end sample here
    # is timed from process launch, run_sample's own default (stated
    # explicitly rather than left implicit, matching deep-goto's ress leg
    # below, which states it for the identical reason — a launch anchor
    # here is a deliberate choice about what the number means, not
    # incidental). the LOCAL jump-end scenario keeps --timing-basis
    # keypress: its fixture is explicitly prewarmed before any sample runs
    # (prewarm_fixture, this scenario's own no-prewarm design is the one
    # thing that makes the mount leg different — see run_mount_leg), so
    # startup reads there are already free, and excluding them from the
    # timer is the correct, deliberate choice for what THAT number means.
    run_sample ress "$fixture" \
      --keys -l "G" \
      --wait2 wait_pane_contains --wait2-needle "$target_prefix" --wait2-ceiling "$SCENARIO_TIMEOUT_S" \
      --timing-basis launch --rss
    if $SAMPLE_HUNG; then
      # unlike open, jump-end CAN hang at either ceiling (never painted,
      # or painted but never reached the target) — this is exactly the
      # first sample's own version of Codex r6 finding 3, so the fired
      # ceiling is read per-sample here too, same as the warm bucket below.
      if (( i == 1 )); then
        first_hung=true; first_hung_ceiling="$SAMPLE_HUNG_CEILING"
      elif [[ "$SAMPLE_HUNG_CEILING" == "$PAINT_TIMEOUT_S" ]]; then
        warm_hung_paint=$(( warm_hung_paint + 1 ))
      else
        warm_hung_scenario=$(( warm_hung_scenario + 1 ))
      fi
      echo "   ress jump-end $i/$RUNS: hung (no jump-to-end within ${SAMPLE_HUNG_CEILING}s)" >&2
    elif $SAMPLE_CRASHED; then
      if (( i == 1 )); then
        first_crashed=true
      else
        warm_crashed=$(( warm_crashed + 1 ))
      fi
      echo "   ress jump-end $i/$RUNS: crashed (pane gone before jump-to-end)" >&2
    else
      if (( i == 1 )); then
        first_ms="$SAMPLE_MS"; first_rss="$SAMPLE_RSS"
      else
        warm_ms+=("$SAMPLE_MS")
        # if/fi, not "cond && warm_rss+=...": harmless HERE (the report_*
        # calls after the loop absorb its status — a short-circuit only
        # escapes set -e when it is a function's own last executed
        # statement), but kept in the safe shape so the pattern never
        # migrates somewhere that condition holds; the real rule lives at
        # _jump_end_sample's note.
        if [[ -n "$SAMPLE_RSS" ]]; then warm_rss+=("$SAMPLE_RSS"); fi
      fi
    fi
  done
  report_mount_row "jump-end" "$label" "$first_hung" "$first_crashed" "$first_hung_ceiling" "$first_ms" \
    "$warm_hung_paint" "$warm_hung_scenario" "$warm_crashed" "$RUNS" "${warm_ms[@]}"
  report_mount_rss_row "$label" "$first_hung" "$first_crashed" "$first_hung_ceiling" "$first_rss" \
    "$warm_hung_paint" "$warm_hung_scenario" "$warm_crashed" "$RUNS" "${warm_rss[@]}"
}

# repeats open + jump-to-end on a mount-resident fixture, always 256 MiB
# regardless of --quick (only the run count follows --quick). ress only —
# no less on the mount at all. the local leg already owns the pager
# comparison; the mount leg exists to answer the question ress exists
# for, real behavior on a real mount — but "cold" overclaims what this
# leg can actually guarantee, which is why its label is "first", not
# "cold". generating a fixture on the mount warms the host's own page
# cache through the write itself: measured directly with a small
# mmap+mincore probe, 81.25% of a freshly generated fixture's pages are
# resident immediately after generation, and no unprivileged step
# available to this script can evict them — posix_fadvise(DONTNEED) was
# tried directly and made no difference, because a fresh write leaves
# dirty pages behind, and DONTNEED cannot drop a dirty page (only a clean
# one). so sample 1 is the first read THIS SCRIPT'S OWN PROCESS makes of
# the fixture, not a guarantee the underlying cache was ever empty —
# genuine cold would need generating the fixture from a separate host (so
# this host's page cache is never touched by the write), or a root-only
# cache drop, neither of which this script performs; it stays
# unprivileged by design. there is also no unprivileged way to create two
# INDEPENDENTLY first-touched windows on the same mount-resident file for
# a second pager to race against: only sample 1 is ever a first read of a
# run, since every read after it — for EITHER pager — benefits from the
# file already living in the mount client's (and possibly its server's)
# own cache. running two pagers here and averaging them would compare
# whichever pager happened to run first against one running second with
# residual warmth from the first's own read — a fake symmetry, not a
# real comparison, and worse than not reporting it. instead, ress's own
# sample 1 is reported alone (mount-first, runs=1) as the one first,
# unprewarmed touch this script can produce, and its remaining samples'
# median is reported separately (mount-warm) — two honest numbers
# answering two different questions instead of one number quietly
# blending them. run_mount_open_scenario and run_mount_jump_end_scenario
# never prewarm, unlike their local-fixture counterparts: a cat of a
# mount-resident file would pull it into the local client's page cache
# before sample 1 is even timed, destroying the one first-touch sample
# this leg can produce.
#
# each MOUNT_FIRST_SCENARIOS entry gets its OWN fixture (mount_fixture_path),
# not one shared one: open's background indexer and its own repeated reads
# would otherwise warm the file before jump-end's own sample 1 ever runs,
# making the "first" label a lie whenever open happens to run first —
# exactly the failure a shared file could never avoid, since "which
# scenario runs first" is this function's own call order, not a property
# of the mount. every required fixture must already exist by the time this
# runs — asserted once, early, by check_mount_fixtures_ready (main() calls
# it right after check_preconditions, long before this function is ever
# reached), not re-checked here: a timed run can never generate-then-time
# one in the same invocation, and there is exactly one caller of this
# function to keep that invariant true for.
run_mount_leg() {
  local label="varied-log-256m.log"
  local fixture
  fixture="$(mount_fixture_path open)"
  run_mount_open_scenario "$fixture" "$label"
  fixture="$(mount_fixture_path jump-end)"
  run_mount_jump_end_scenario "$fixture" "$label" "${fixture}.stats"
}

print_results() {
  local tsv="$FIXTURES_DIR/last-run.tsv"
  {
    printf 'scenario\tfixture\tpager\tmedian_ms\truns\n'
    local row
    for row in "${TABLE_ROWS[@]}"; do printf '%s\n' "$row"; done
    printf '\n# RSS after jump-to-end (KiB) — median_kib, not median_ms\n'
    printf 'fixture\tpager\tmedian_kib\truns\n'
    for row in "${RSS_ROWS[@]}"; do printf '%s\n' "$row"; done
  } > "$tsv"

  echo
  print_aligned_table "$(printf 'scenario\tfixture\tpager\tmedian_ms\truns')" "${TABLE_ROWS[@]}"
  echo
  echo "RSS after jump-to-end (KiB):"
  print_aligned_table "$(printf 'fixture\tpager\tmedian_kib\truns')" "${RSS_ROWS[@]}"
  echo
  echo "machine-readable copy: $tsv"
}

# --- setup --------------------------------------------------------------

check_preconditions() {
  # bash formats EPOCHREALTIME (now_us()'s own source) using the CURRENT
  # LC_NUMERIC locale's decimal separator, not unconditionally ".": under
  # a comma-radix locale (confirmed directly, LC_NUMERIC=de_DE.UTF-8),
  # EPOCHREALTIME comes back as e.g. "1737034567,891234" — a string
  # now_us() (see below) cannot turn into a pure-digit microsecond count,
  # and every wait primitive's `10#$(now_us)` arithmetic silently produces
  # garbage (see now_us()'s own comment for the exact mechanism — it does
  # not cleanly abort, which would at least be diagnosable). this harness
  # already pins locale-sensitive behavior for its SUBJECTS (LC_ALL=C.utf8
  # in the pager launch, ENV_WHITELIST_STATIC's comment) but never pinned
  # one for ITSELF — set here, before anything below (or anything this
  # function's own caller does afterward) can read EPOCHREALTIME under
  # whatever locale this process happened to inherit.
  # belt AND suspenders with now_us()'s own radix-agnostic extraction
  # below — either alone is sufficient, but this is the cheaper of the
  # two (skips the problem instead of tolerating it) and documents the
  # actual mechanism for a reader who has not seen now_us() yet.
  export LC_NUMERIC=C
  # assert_pane_is and the RSS sample both read /proc/<pid>/... directly,
  # which is Linux-only; this harness is Linux-first by spec and has no
  # fallback (e.g. ps-based) for any other kernel — no such fallback is
  # planned, so failing loudly and explaining why is the whole gate here,
  # not a stand-in for one.
  [[ "$(uname -s)" == Linux ]] \
    || die "this harness only runs on Linux (it reads /proc/<pid>/comm and /proc/<pid>/status directly; no fallback exists for $(uname -s))"
  # now_us() reads EPOCHREALTIME, a bash-5+ built-in — on an older bash it is
  # simply never defined (not "defined empty"), and set -u turns the FIRST
  # reference anywhere in the script into a raw, unhelpful "EPOCHREALTIME:
  # unbound variable" instead of one of this script's own die() messages —
  # and that first reference happens deep inside the first run_sample call,
  # well after the release build and fixture generation already ran, so the
  # unhelpful failure also arrives late. probed directly (${EPOCHREALTIME:-}
  # — safe under set -u, unlike a bare reference) rather than checking
  # BASH_VERSINFO: this is the actual dependency, not a version-number proxy
  # for it, so it still catches a hypothetical future bash that drops or
  # renames the variable despite reporting version 5+.
  [[ -n "${EPOCHREALTIME:-}" ]] \
    || die "this harness needs bash 5+ (EPOCHREALTIME, used for microsecond-resolution timing with no per-tick subprocess, is unset — this bash is likely older than 5.0)"
  command -v tmux >/dev/null 2>&1 || die "tmux not found on PATH"
  # resolved to an absolute path HERE, on the CLIENT side, using this
  # process's own PATH (the nix devshell's, when run the intended way) —
  # not left as the bare word "less" for pager_cmd to embed in PAGER_ARGV.
  # tmux's own documented model (see tmux(1)'s "GLOBAL AND SESSION
  # ENVIRONMENT": the server copies its OWN environment at birth into a
  # global environment; a new session's session environment is refreshed
  # from the client only for variables in `update-environment`, whose
  # default list does not include PATH) implies a bare "less" resolved at
  # pane-launch time could differ from the less this preflight just
  # validated, or fail outright, on a server that predates this
  # invocation's nix devshell — the same failure mode RESS_BIN/FILEGEN_BIN
  # (already always absolute, derived from repo_root, never bare "ress")
  # were built to avoid. tried directly to reproduce that exact mechanism
  # (a real server, its own /proc/<pid>/environ independently confirmed to
  # carry a different PATH, then a real new-session launch using the same
  # no-shell multi-argument form run_sample uses): the spawned pane
  # resolved "less" against something matching the CLIENT's own current
  # PATH instead, both times, an apparent discrepancy from the documented
  # model this investigation did not fully resolve (arbitrary env vars,
  # tested the same way, DID come from the server's own captured
  # environment, matching the docs — PATH specifically did not). resolving
  # to an absolute path removes the ambiguity either way: there is no
  # lookup left for tmux's server-vs-client environment handling to affect,
  # so the pane's less is unconditionally the one this preflight validated.
  LESS_BIN="$(command -v less)" || die "less not found on PATH"
  # the SAME resolve-once-here discipline, for a DIFFERENT underlying
  # reason than LESS_BIN's: the tracing launch's wrapper (see run_sample)
  # execs a bare "bash" under ENV_WHITELIST_STATIC's env -i, which has no
  # PATH entry at all (by design — Round 11). that "works" today only
  # because env's own execvp()-style resolution of a bare command name
  # falls back to glibc's compiled-in default path when the environment
  # it is building has no PATH of its own, and /usr/bin (part of that
  # default) happens to hold a real bash on this machine — confirmed
  # directly: `env -i TERM=... bash -c '...'`, then reading the pane's
  # own /proc/<pid>/exe, resolves to exactly /usr/bin/bash. a pure NixOS
  # host has no such fallback to land in (binaries live only under
  # /nix/store/<hash>-*/bin, never at a fixed /usr/bin or /bin), so that
  # exact launch would fail outright there. resolved to an absolute path
  # here for the identical reason LESS_BIN is: removes the lookup
  # entirely rather than depending on a fallback this preflight cannot
  # itself guarantee holds on every host it might run on.
  BASH_BIN="$(command -v bash)" || die "bash not found on PATH"
}

# asserts every mount fixture run_mount_leg will need already exists —
# called EARLY (right after check_preconditions, before the release build,
# before ensure_fixtures, before any scenario times anything), not from
# inside run_mount_leg itself, which used to run this identical check only
# after every LOCAL scenario had already completed. for a non-quick run
# that is minutes of real samples, discarded the instant this died, since
# print_results is the very last statement in main() — an unprepped mount
# must instead abort in seconds, before any of that work starts. a
# separate function rather than folded into check_preconditions itself:
# that one asks "is the environment/tooling sufficient", this one asks "is
# the data ready" — a different kind of question, kept in its own place,
# just called from the same spot. safe to call unconditionally from that
# spot: returns immediately when there is no mount to check (RESS_PERF_MOUNT
# unset), and is never reached at all for --fixtures-only (which exits
# before this point in main() — that mode's whole job, when a mount IS
# set, is to CREATE these exact fixtures, so asserting they already exist
# would be asserting against the one case they are expected to be absent).
check_mount_fixtures_ready() {
  [[ -n "${RESS_PERF_MOUNT:-}" ]] || return 0
  local scenario fixture
  local -a missing=()
  for scenario in "${MOUNT_FIRST_SCENARIOS[@]}"; do
    fixture="$(mount_fixture_path "$scenario")"
    [[ -f "$fixture" && -f "${fixture}.stats" ]] || missing+=("$fixture")
  done
  (( ${#missing[@]} == 0 )) \
    || die "mount fixture(s) missing: ${missing[*]} — generate first: RESS_PERF_MOUNT=$RESS_PERF_MOUNT just fixtures (a run that both generates and times it would measure its own generation-warmth, not a fresh first read)"
}

parse_args() {
  while (( $# > 0 )); do
    case "$1" in
      --quick) QUICK=true ;;
      --fixtures-only) FIXTURES_ONLY=true ;;
      *) die "unknown argument: $1" ;;
    esac
    shift
  done
}

main() {
  parse_args "$@"

  local script_dir repo_root
  script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  repo_root="$(cd "$script_dir/.." && pwd)"
  cd "$repo_root"

  FIXTURES_DIR="$repo_root/fixtures"
  RESS_BIN="$repo_root/target/release/ress"
  FILEGEN_BIN="$repo_root/target/release/ress-filegen"
  # forced, not derived: cargo's own precedence is CLI flag > CARGO_TARGET_DIR
  # env var > a .cargo/config.toml build.target-dir setting > the "target"
  # default, so a CARGO_TARGET_DIR already set in the invoking environment
  # (or a config file's target-dir) would otherwise send cargo build's
  # output somewhere other than $RESS_BIN/$FILEGEN_BIN above, without this
  # script ever knowing — it would see cargo succeed and then die on a
  # missing binary that in fact exists, just not where it looked. exporting
  # the env var here — the second-highest-precedence source — beats any
  # config file and pins every subsequent cargo invocation in this script
  # to the one directory these hardcoded paths agree with.
  export CARGO_TARGET_DIR="$repo_root/target"
  # a dedicated subdirectory, not the mount's root: a fixture-shaped name
  # colliding with a real file the mount's owner put there is exactly the
  # scenario generate_fixture's partial-pair abort exists to never paper
  # over by guessing.
  if [[ -n "${RESS_PERF_MOUNT:-}" ]]; then
    MOUNT_FIXTURES_DIR="$RESS_PERF_MOUNT/ress-perf-fixtures"
  fi

  if $QUICK; then
    RUNS=2
    VARIED_LOG_NAME="varied-log-256m.log"
  else
    # even, not odd: the pager-order alternation in every scenario flips by
    # sample parity (order=(less ress) on odd i, (ress less) on even i), so
    # an odd RUNS gives one pager the first (residual-warmth-free) position
    # one more time than the other — the exact bias alternation exists to
    # cancel, and per-row medians are what the table reports. an even count
    # keeps the two positions balanced N/2 and N/2.
    RUNS=6
    VARIED_LOG_NAME="varied-log-2g.log"
  fi
  if [[ -n "${RESS_PERF_RUNS:-}" ]]; then
    [[ "$RESS_PERF_RUNS" =~ ^[1-9][0-9]*$ ]] \
      || die "RESS_PERF_RUNS must be a positive integer, got: $RESS_PERF_RUNS"
    RUNS="$RESS_PERF_RUNS"
  fi
  # not a rejection: an odd override is the user's informed call (there may
  # be a real reason, e.g. matching a previous run's sample count), but the
  # imbalance above is real and silent otherwise.
  if (( RUNS % 2 != 0 )); then
    echo "perf.sh: warning: RUNS=$RUNS is odd — pager-order alternation cannot balance an odd sample count, leaving a one-sample first-position imbalance (see docs/perf.md)" >&2
  fi

  WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/ress-perf.XXXXXX")"
  SESSION_PREFIX="ress-perf-$$"
  trap cleanup EXIT

  # a real, empty, per-run stand-in for $HOME (and XDG_CONFIG_HOME/
  # XDG_DATA_HOME under it) — see ENV_WHITELIST_STATIC's comment for why
  # LESSHISTFILE alone still needs its own explicit "-" value, and why
  # LESSKEYIN needs no explicit entry at all once HOME/XDG point here.
  # lives under WORKDIR, so the existing cleanup trap removes it with everything
  # else; every run_sample launch points HOME/XDG_CONFIG_HOME/XDG_DATA_HOME
  # here.
  FAKE_HOME="$WORKDIR/fake-home"
  mkdir -p "$FAKE_HOME/.config" "$FAKE_HOME/.local/share"

  # the pager preflight (tmux/less) is only needed once a scenario is about
  # to launch a pager — --fixtures-only never does, so it must not refuse to
  # run just because neither tool happens to be installed. same reasoning
  # for the mount-fixture check: called here, before the release build and
  # before ensure_fixtures, so an unprepped mount aborts in seconds — not
  # after minutes of local-scenario samples that print_results would then
  # never get a chance to report.
  if ! $FIXTURES_ONLY; then
    check_preconditions
    check_mount_fixtures_ready
  fi

  echo "building release binaries..." >&2
  cargo build --release --workspace
  [[ -x "$RESS_BIN" ]] || die "release ress binary missing: $RESS_BIN"
  [[ -x "$FILEGEN_BIN" ]] || die "release ress-filegen binary missing: $FILEGEN_BIN"

  ensure_fixtures

  if $FIXTURES_ONLY; then
    echo "fixtures ready; --fixtures-only set, exiting." >&2
    exit 0
  fi

  local variedlog="$FIXTURES_DIR/$VARIED_LOG_NAME"
  local megalines="$FIXTURES_DIR/megalines-512m.log"
  local singleline="$FIXTURES_DIR/single-line-256m.log"

  run_open_scenario "$variedlog" "$VARIED_LOG_NAME"
  run_jump_end_scenario "$variedlog" "$VARIED_LOG_NAME" "${variedlog}.stats"
  run_deep_goto_scenario "$variedlog" "$VARIED_LOG_NAME" "${variedlog}.stats"
  run_scroll_cadence_scenario "$variedlog" "$VARIED_LOG_NAME"

  run_open_scenario "$megalines" "megalines-512m.log"
  run_jump_end_scenario "$megalines" "megalines-512m.log" "${megalines}.stats"
  run_deep_goto_scenario "$megalines" "megalines-512m.log" "${megalines}.stats"
  run_scroll_cadence_scenario "$megalines" "megalines-512m.log"

  # single-line is exactly one line: G, deep-goto and scroll-cadence have
  # nothing to jump or scroll to, so only open->first-paint applies.
  run_open_scenario "$singleline" "single-line-256m.log"

  if [[ -n "${RESS_PERF_MOUNT:-}" ]]; then
    run_mount_leg
  fi

  print_results
}

main "$@"
