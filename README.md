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
| `<count>%` | jump to the line containing the byte at that percentage |
| `h` / `l` / `←` / `→` | horizontal scroll (long lines are chopped) |
| mouse wheel | scroll three lines |
| `Ctrl-l` | force redraw |
| `Esc` | cancel a pending operation, count, or chord |
| `q` / `Ctrl-c` | quit |

Jumps that need more scanning than the interactive budget (for example `G`
into a file whose tail is one giant line, or a very large count) continue in
the background: the viewport stays put, a transient bottom-row indicator
shows progress, `Esc` cancels, and any new motion supersedes the scan.

Reserved for upcoming features: `/` `?` `n` `N` (search), `:` and `<count>G`
(go to line — today a count before `G` is ignored and `G` jumps to the end),
`w` (wrap toggle), `0` (reset horizontal scroll).

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

Documentation
=============

Design documents describing the internals live in the `docs/` directory:

- [Architecture](docs/architecture.md) — crate layout, offset-driven
  rendering, the event loop, and the type-level invariants
- [Block cache](docs/block_cache.md) — scan-resistant eviction, read
  coalescing, and the promotion policy
- [Budgeted scanning](docs/budgeted_scanning.md) — how every read path is
  bounded, and what happens when a budget runs out
- [Prefetch](docs/prefetch.md) — keeping the scroll direction warm without
  polluting the cache

Session-scoped specs and implementation plans are working artifacts and are
deliberately not part of the repository.
