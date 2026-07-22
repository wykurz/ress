# ress

A fast terminal pager for huge files, optimized for high-latency network
filesystems (NFS, Lustre, Weka). Linux-first — the roadmap embraces
Linux-specific I/O (io_uring, fadvise) as it matures. Other Unix-likes may
build but are untested; non-Unix platforms are unsupported (the file backend
is Unix-only). Early development.

[![CI](https://github.com/wykurz/ress/actions/workflows/ci.yml/badge.svg)](https://github.com/wykurz/ress/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)

Why not just `less`? `less` issues small synchronous reads (painful on
high-latency filesystems) and can stall building its line map. `ress` paints
the first screen instantly regardless of file size, reads through a shared
prefetching block cache, and strictly bounds the read work of every
interactive attempt — a motion that needs more scanning continues as a
visible, cancellable background operation instead of blocking the UI.

Opening a multi-gigabyte file shows the first screen in milliseconds; paging
back through recently viewed content issues no reads at all, for as much
content as the cache holds (256 MiB by default).

A status line on the bottom row reads `{name} · L{n}[/{total}] · {pct}%` —
the current line, the file's total once the background index finishes
cleanly, and how far the anchor sits through the file by byte offset. An
empty file reads `{name} · empty · {pct}%` instead, skipping the
misleading `L1/0`. While the background index is still running, an
unresolved line number reads `indexing… {k} lines`; once indexing has
finished it reads `L?` until the count resolves. If a read error ends
indexing early, the total never appears — its partial line count is real,
but it is not the file's total.

Above the status line, a dim hint bar shows the live keymap for whichever
mode is active — motions in normal use, edit keys while composing `:N`,
`any key closes` in help — whenever the terminal has room to spare for it.
A one-cell scrollbar marks the viewport's position on the right edge of
the content area, proportional to its byte offset through the file — a
position marker, not a size-proportional thumb. Short terminals degrade in
steps: 4 rows or more keep both the hint bar and the bottom row, 2-3 rows
keep the bottom row alone, and 1 row or fewer drops chrome entirely so
content still has somewhere to paint.

Usage
=====

```fish
> ress <file>
```

| Flag | Default | Meaning |
|---|---|---|
| `--log-file <path>` | off | write debug logs to a file (the TUI owns the screen; logs never print to it) |
| `-v` / `-vv` / `-vvv` | warn | log verbosity (info / debug / trace); `RESS_LOG` env filter overrides |
| `--cache-mib <n>` | 256 | block cache size in MiB |
| `--prefetch-depth <n>` | 8 | blocks to keep warm ahead of the viewport (0 disables) |

Key bindings
------------

Vim-first, informed by helix and zellij, and preserving `less` navigation
where it costs nothing (see [docs/architecture.md](docs/architecture.md) for
the philosophy). Counts prefix motions: `12j` moves twelve lines.

| Keys | Action |
|---|---|
| `j` / `k` / `↓` / `↑` | line down / up |
| `d` / `u` (also `Ctrl-d` / `Ctrl-u`) | half page down / up |
| `Ctrl-f` / `Space` / `PgDn` | full page down |
| `Ctrl-b` / `PgUp` | full page up |
| `gg` / `ge` / `G` | top / end of file (`ge` and `G` are aliases) |
| `<count>G` / `<count>gg` | jump to line N (past the end clamps to the last line) |
| `<count>%` | jump to the line containing the byte at that percentage |
| `:N` | jump to line N (Enter to go — Ctrl+J works too, so a command typed before the first paint still commits; Esc to cancel, Backspace to edit; needs 2+ rows, 3+ columns) |
| `h` / `l` / `←` / `→` | horizontal scroll (long lines are chopped) |
| mouse wheel | scroll three lines (normal view only; a tick in command or help is ignored and never touches a running jump) |
| `Ctrl-l` | force redraw |
| `Esc` | cancel a pending operation, count, or chord |
| `F1` | toggle the help overlay (needs 7+ rows, 5+ columns) |
| `q` / `Ctrl-c` | quit |

Jumps that need more scanning than the interactive budget (for example `G`
into a file whose tail is one giant line, a very large count, or a
`<count>G`, `<count>gg`, or `:N` jump beyond what the background line index
has scanned) continue in the background: the viewport stays put, a
transient bottom-row indicator shows progress, `Esc` cancels, and any new
motion supersedes the scan.

Reserved for upcoming features: `/` `?` `n` `N` (search), `w` (wrap
toggle), `0` (reset horizontal scroll).

Building
========

The repository pins its toolchain via nix + direnv; `just` wraps the common
commands:

```fish
> nix develop      # or let direnv load the environment
> just build       # debug build
> just build-release
> just ci          # fmt check + clippy (warnings denied) + doc build + tests
```

A plain `cargo build` works too if you have a recent stable Rust.

Measuring performance
======================

`just bench` runs the criterion benches against engine operations with
injected network latency, isolating one component's cost — including
cold-cache behavior — at a time; it never runs in CI. `just perf` (add
`--quick` for a faster pass over smaller fixtures) races the release
binary against `less` end-to-end, each under its own pty, on
deterministic fixtures, for the whole-binary, warm-cache number a user
actually experiences. See [docs/perf.md](docs/perf.md) for the full
methodology, scenario definitions, and caveats.

Documentation
=============

Design documents describing the internals live in the `docs/` directory:

- [Architecture](docs/architecture.md) — crate layout, offset-driven
  rendering, the event loop, the mode machine and chrome stack, the status
  line, and the type-level invariants
- [Block cache](docs/block_cache.md) — scan-resistant eviction, read
  coalescing, and the promotion policy
- [Budgeted scanning](docs/budgeted_scanning.md) — how every read path is
  bounded, and what happens when a budget runs out
- [Prefetch](docs/prefetch.md) — keeping the scroll direction warm without
  polluting the cache
- [Concurrency](docs/concurrency.md) — the one-owned-task, watch-channel
  shape every background computation shares
- [Measuring performance](docs/perf.md) — the criterion and end-to-end
  harnesses, what each isolates, and how to run them

Session-scoped specs and implementation plans are working artifacts and are
deliberately not part of the repository.
