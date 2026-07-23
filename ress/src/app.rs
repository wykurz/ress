//! App state and the event loop. `handle_key` is the deterministic, IO-free
//! state-machine decision; `run` is the thin async driver over crossterm
//! events.
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEventKind};
use futures::StreamExt;
use ratatui::layout::Rect;
use ress_core::document::{Anchor, Document, HScroll};
use ress_core::resolve::{NavOutcome, PendingNav, Resolution};
use ress_core::search::SearchPattern;
/// What the loop should do after handling an input event.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Quit,
    Nav(Nav),
    Redraw,
    Cancel,
    /// A search buffer was committed with Enter on a non-empty buffer (`raw` is the typed
    /// pattern text). Compilation happens in the run loop, where `Document` lives — `handle_key`
    /// itself stays IO- and regex-free, matching every other `Action` variant here.
    CommitSearch {
        raw: String,
        backward: bool,
    },
    None,
}
/// A navigation intent; the async loop turns it into `Document` calls.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Nav {
    Lines(i64),
    HalfPage(i8),
    Page(i8),
    Top,
    Bottom,
    Line(u64),
    Percent(u8),
    Horizontal(i64),
    /// Advances to the next match in the ACTIVE search's own direction (vim's `n`).
    SearchNext,
    /// Advances to the next match in the REVERSE of the active search's direction (vim's `N`).
    SearchPrev,
}
/// A live pending navigation and the intent that produced it. Dropping it
/// cancels the background scan, so every exit from the run loop — quit,
/// stream end, supersession, and every error `?` — aborts the task instead
/// of detaching it (aborted fetches clean up after themselves in the cache).
struct ActiveNav {
    nav: Nav,
    p: PendingNav,
}
impl Drop for ActiveNav {
    fn drop(&mut self) {
        self.p.cancel();
    }
}
/// Which grammar `App::handle_key_command` applies to the shared `command` buffer: `Colon`'s
/// digits-only line-jump (unchanged from before this type existed), or `Search`'s arbitrary-text
/// regex pattern, remembering the direction (`/` vs `?`) it was entered with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandKind {
    Colon,
    Search { backward: bool },
}
/// The input router's top-level state: which key table `handle_key` consults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Command(CommandKind),
    Help,
}
/// A committed search: the compiled pattern, the background sweep's own generation stamp (so a
/// stale `SearchSummary` snapshot is never mistaken for this search's — see `SearchSummary`'s own
/// doc comment), and `current` — the byte offset of the match `n`/`N` last landed on (`None`
/// until the first jump completes). `current` is the next search's own origin ingredient
/// (`apply_nav`'s `SearchNext`/`SearchPrev` arm) AND the render-facing highlight target (`draw`'s
/// own `viewport` call passes `search.current` directly); `apply_outcome` is its only writer.
struct ActiveSearch {
    pattern: std::sync::Arc<SearchPattern>,
    generation: u64,
    current: Option<u64>,
}
/// Pager state: the byte anchor of the top line, the horizontal column offset,
/// the in-progress count prefix / `g` chord, and the current input mode.
pub struct App {
    pub top: Anchor,
    pub hscroll: HScroll,
    pub mode: Mode,
    /// The in-progress `:`/`/`/`?` command line, live only in `Mode::Command(_)` — shared by
    /// both `CommandKind`s (the digits-only `:` grammar and the arbitrary-text search grammar);
    /// which grammar `handle_key_command` applies comes from `Mode::Command`'s own payload, not
    /// a second buffer field.
    command: String,
    count: Option<usize>,
    pending_g: bool,
    /// The committed search, if any: pattern, sweep generation, and the current match. `None`
    /// until the first `Action::CommitSearch` is compiled successfully (the run loop's job); `n`/
    /// `N` are notice-only no-ops while this is `None` (see `apply_nav`).
    search: Option<ActiveSearch>,
    /// A one-line transient notice ("search wrapped", "pattern not found", a bad-pattern error).
    /// `apply_outcome`/the run loop's `CommitSearch` arm are its writers; `handle_key` clears it
    /// as the first thing it does on every key, per its own doc comment.
    notice: Option<String>,
}
impl App {
    pub fn new() -> Self {
        Self {
            top: Anchor::TOP,
            hscroll: HScroll::ZERO,
            mode: Mode::Normal,
            command: String::new(),
            count: None,
            pending_g: false,
            search: None,
            notice: None,
        }
    }
    /// Test-only peek at the in-progress command buffer; production code
    /// never needs it, since `draw` reads the field directly (same module).
    #[cfg(test)]
    pub(crate) fn command_buffer(&self) -> &str {
        &self.command
    }
    /// Sets a one-line transient notice. Bookkeeping only — Task 6 wires the
    /// render path that displays and clears `notice`.
    fn set_notice(&mut self, msg: &str) {
        self.notice = Some(msg.to_string());
    }
    fn take_count(&mut self) -> usize {
        self.count.take().unwrap_or(1)
    }
    fn take_count_i64(&mut self) -> i64 {
        self.take_count().min(i64::MAX as usize) as i64
    }
    /// Exits a mode the terminal can no longer show anything for, back to
    /// `Normal` — asked every time `handle_key` is about to route a key,
    /// against `rows`/`cols` AS THAT CALL RECEIVED THEM, never a value
    /// remembered from an earlier call. That is the whole fix for the
    /// cached-vs-live-size bug class: there is no field left to go stale,
    /// so a shrink the caller has not yet reported cannot be missed —
    /// there is nothing cached that could disagree with it.
    /// `ChromeLayout::can_enter` is the same bound the entry gates below
    /// ask, so a mode can never survive here that a key press could not
    /// have entered in the first place.
    fn reconcile_mode(&mut self, rows: u16, cols: u16) {
        if self.mode != Mode::Normal && !ChromeLayout::can_enter(self.mode, rows, cols) {
            if matches!(self.mode, Mode::Command(_)) {
                self.command.clear();
            }
            self.mode = Mode::Normal;
        }
    }
    /// Routes a key press by mode, then decides what it means, consuming any
    /// pending count/`g` state along the way. `rows`/`cols` are the
    /// terminal's live size as of THIS call — the caller (`run`) samples it
    /// fresh immediately before calling this, rather than this type caching
    /// its own copy, so `reconcile_mode` and the `:`/F1 entry gates below
    /// always judge against the terminal as it actually is right now, not
    /// as it was the last time some other event happened to update a
    /// field. See `run`'s `resample_size` for where that fresh sample comes
    /// from and why it must be taken there rather than trusted from a
    /// previous call.
    pub fn handle_key(&mut self, key: KeyEvent, rows: u16, cols: u16) -> Action {
        self.reconcile_mode(rows, cols);
        // a transient notice is exactly that -- transient: any key at all retires it, even one
        // that itself produces `Action::None` (e.g. an unmapped key). Unconditional, not
        // conditioned on whether one was actually set: the OBSERVABLE effect only shows up "when
        // set", the statement itself is simplest left unconditional. The run loop is what turns
        // this into a repaint even when the routed key's own action is `Action::None` -- it peeks
        // `notice.is_some()` BEFORE calling this, since by the time this returns the clearing has
        // already happened (see `run`'s own `Event::Key` arm).
        self.notice = None;
        match self.mode {
            Mode::Help => {
                // any key closes help; none of them reach Normal's table, so
                // e.g. `q` here cannot quit the pager.
                self.mode = Mode::Normal;
                Action::Redraw
            }
            Mode::Command(kind) => self.handle_key_command(kind, key),
            Mode::Normal => self.handle_key_normal(key, rows, cols),
        }
    }
    /// Routes a `Mode::Command(_)` key to the grammar its `kind` selects — `Colon`'s digits-only
    /// line-jump (unchanged, see `handle_key_command_colon`'s own doc comment) or `Search`'s
    /// arbitrary-text pattern buffer (`handle_key_command_search`). Both share the one `command`
    /// buffer field; only which grammar interprets it differs.
    fn handle_key_command(&mut self, kind: CommandKind, key: KeyEvent) -> Action {
        match kind {
            CommandKind::Colon => self.handle_key_command_colon(key),
            CommandKind::Search { backward } => self.handle_key_command_search(backward, key),
        }
    }
    /// `:N` line jumps: digits accumulate (capped at 20 — a u64 line number
    /// is at most 20 digits, so further digits are simply dropped rather
    /// than rejected), Backspace edits, Enter commits, Esc cancels. The
    /// buffer holds ASCII digits and nothing else — `render_command_line`'s
    /// tail windowing counts on one char per terminal cell to keep the
    /// cursor visible, so widening this grammar means revisiting it. Leading
    /// zeros collapse to at most one: a `0` typed onto an EMPTY buffer
    /// pushes it (buffer becomes `"0"`), but any digit typed while the
    /// buffer is EXACTLY `"0"` REPLACES that zero instead of accumulating
    /// alongside it — `"0"` + `'0'` stays `"0"`, `"0"` + `'4'` becomes
    /// `"4"`. This is what keeps an arbitrarily long run of leading zeros
    /// from ever eating the 20-char cap before a significant digit
    /// arrives: the buffer never grows past one character while it is
    /// still all-zero, so `:0000000000000000000000042` types as `42`
    /// regardless of how many zeros precede it, not just short runs. A `0`
    /// once the buffer holds anything other than a lone `"0"` is a normal
    /// digit, since it is no longer leading. Enter on `"0"` commits
    /// `Nav::Line(0)` — `goto_line`'s vim-style clamp already treats line 0
    /// the same as line 1, so `:0` is a working, if redundant, spelling of
    /// jump-to-top; Enter on a genuinely EMPTY buffer (no digit typed at
    /// all) still cancels instead. A 20-digit buffer can still overflow
    /// `u64`; `unwrap_or(u64::MAX)` treats that the same as a past-the-end
    /// line number, which `goto_line`'s clamp already resolves to the last
    /// line, so there is no separate error path to render. Every buffer
    /// edit that changes what's on screen — a significant digit, a
    /// Backspace that removes one, the first zero of a run — returns
    /// `Action::Redraw`, not `Action::None`: unlike Normal's count prefix,
    /// the buffer is on screen, so a silently-dropped `Action::None` would
    /// leave the row frozen while keystrokes kept registering underneath
    /// it. Three cases change nothing on screen, so `Action::None` is
    /// correct for each: a zero that only replaces the buffer's existing
    /// lone zero with itself, a digit dropped once the buffer is already
    /// at the 20-char cap, and a Backspace on an already-empty buffer,
    /// which has nothing left to pop. The digit arm carries Normal's
    /// `false` ctrl-guard for the same reason it exists there — Ctrl+digit
    /// is not a digit. Everything else is ignored — vim beeps, we no-op.
    fn handle_key_command_colon(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match (key.code, ctrl) {
            (KeyCode::Esc, _) => {
                self.mode = Mode::Normal;
                self.command.clear();
                Action::Redraw
            }
            // Ctrl+J is bound as Enter's alias (harmless: it has no other meaning here) because
            // an Enter typed before Term::new()'s enable_raw_mode() call arrives while the tty
            // is still cooked, where POSIX ICRNL translates Enter's CR byte to LF as it's
            // received — and crossterm only decodes a bare LF as Enter when raw mode is NOT
            // active at parse time, which by construction it always is by the time ress's loop
            // parses anything. Without this alias, a `:N` typed and submitted before first
            // paint would land exactly the misinterpreted LF produces: Ctrl+J, unhandled, so
            // the command line is left stuck showing `:N_`, uncommitted (see issue #30's
            // investigation report and docs/architecture.md's event loop section for the full
            // mechanism, confirmed against crossterm 0.29's own source and its own issue #371).
            (KeyCode::Enter, _) | (KeyCode::Char('j'), true) => {
                self.mode = Mode::Normal;
                let command = std::mem::take(&mut self.command);
                if command.is_empty() {
                    Action::Redraw
                } else {
                    Action::Nav(Nav::Line(command.parse::<u64>().unwrap_or(u64::MAX)))
                }
            }
            (KeyCode::Backspace, _) => {
                if self.command.pop().is_some() {
                    Action::Redraw
                } else {
                    // an empty buffer has nothing left to pop: the row
                    // still reads ":_", so there's nothing to repaint.
                    Action::None
                }
            }
            (KeyCode::Char(d @ '0'..='9'), false) => {
                if self.command == "0" {
                    if d == '0' {
                        return Action::None;
                    }
                    self.command.clear();
                    self.command.push(d);
                    return Action::Redraw;
                }
                if self.command.len() < 20 {
                    self.command.push(d);
                    Action::Redraw
                } else {
                    // dropped at the cap: the buffer is unchanged, so
                    // there's nothing to repaint for this keystroke either.
                    Action::None
                }
            }
            _ => Action::None,
        }
    }
    /// `Search{backward}`'s grammar: unlike `Colon`'s digits-only line, the buffer holds
    /// arbitrary regex text. Backspace pops (a no-op `Action::None` on an already-empty buffer,
    /// same reasoning as `Colon`'s). Esc exits to `Normal` and clears the buffer. Enter commits
    /// (Ctrl+J is its alias here too — same mechanism as `Colon`'s own alias arm, see the arm
    /// below): an EMPTY buffer just closes (`Action::Redraw`, mirroring `Colon`'s empty-Enter
    /// cancel — no `Action::CommitSearch` for a search nobody typed); a non-empty buffer
    /// produces `Action::CommitSearch { raw, backward }` — compiling the pattern needs
    /// `Document`, so it happens in the run loop, not here (`handle_key` stays IO- and
    /// regex-free throughout). The push arm accepts `KeyCode::Char(c)` only when BOTH `c` is
    /// not a control character AND the key carries no ctrl modifier — a ctrl-modified letter
    /// (Ctrl+J foremost) must never be read as pattern text; skipping that guard here would be
    /// strictly worse than `Colon`'s own stuck-buffer symptom, since the pattern would corrupt
    /// silently instead of visibly refusing to commit (see the alias arm's own comment).
    fn handle_key_command_search(&mut self, backward: bool, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match (key.code, ctrl) {
            (KeyCode::Esc, _) => {
                self.mode = Mode::Normal;
                self.command.clear();
                Action::Redraw
            }
            // same alias as Colon's, same mechanism -- see the comment there; the push arm
            // below must never see a ctrl-modified char as pattern text.
            (KeyCode::Enter, _) | (KeyCode::Char('j'), true) => {
                self.mode = Mode::Normal;
                let raw = std::mem::take(&mut self.command);
                if raw.is_empty() {
                    Action::Redraw
                } else {
                    Action::CommitSearch { raw, backward }
                }
            }
            (KeyCode::Backspace, _) => {
                if self.command.pop().is_some() {
                    Action::Redraw
                } else {
                    Action::None
                }
            }
            // ctrl-modified chars are never pattern text -- only a bare, non-control char is
            // pushed (mirrors Colon's own digit arm's ctrl guard: "Ctrl+digit is not a digit").
            (KeyCode::Char(c), false) if !c.is_control() => {
                self.command.push(c);
                Action::Redraw
            }
            _ => Action::None,
        }
    }
    fn handle_key_normal(&mut self, key: KeyEvent, rows: u16, cols: u16) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // a refused entry is as if the key was never pressed: gates run
        // before any state is touched — not just the count/mode a
        // successful `:`/F1 would otherwise clear below, but also the
        // pending `g`-chord just below this, which would otherwise treat
        // this key as the one that aborts it. ChromeLayout::can_enter is
        // the single source for both bounds, shared with reconcile_mode
        // and ChromeLayout::of's `bottom`/`overlay` fields. `rows`/`cols`
        // are `handle_key`'s own parameters, not a field — see its doc
        // comment for why that is the whole point.
        let refused = match (key.code, ctrl) {
            (KeyCode::Char(':'), false) => {
                !ChromeLayout::can_enter(Mode::Command(CommandKind::Colon), rows, cols)
            }
            // `/`/`?` share `:`'s exact floor — `can_enter`'s `Command(_)` arm doesn't
            // distinguish `CommandKind`, so either concrete kind asks the identical question.
            (KeyCode::Char('/'), false) => !ChromeLayout::can_enter(
                Mode::Command(CommandKind::Search { backward: false }),
                rows,
                cols,
            ),
            (KeyCode::Char('?'), false) => !ChromeLayout::can_enter(
                Mode::Command(CommandKind::Search { backward: true }),
                rows,
                cols,
            ),
            (KeyCode::F(1), _) => !ChromeLayout::can_enter(Mode::Help, rows, cols),
            _ => false,
        };
        if refused {
            return Action::None;
        }
        if self.pending_g {
            self.pending_g = false;
            match (key.code, ctrl) {
                (KeyCode::Char('g'), false) => {
                    return match self.count.take() {
                        Some(n) => Action::Nav(Nav::Line(n as u64)),
                        None => Action::Nav(Nav::Top),
                    };
                }
                // helix-style goto-end, alias of bare G; a count means goto-line only through G/gg.
                (KeyCode::Char('e'), false) => {
                    self.count = None;
                    return Action::Nav(Nav::Bottom);
                }
                _ => {
                    // vim aborts the pending count with the chord.
                    self.count = None;
                }
            }
        }
        match (key.code, ctrl) {
            (KeyCode::Char('q'), _) => Action::Quit,
            (KeyCode::Char('c'), true) => Action::Quit,
            (KeyCode::Char(':'), false) => {
                // the command line has nowhere to paint without a bottom
                // chrome row at all, or too little width for typing to be
                // meaningfully visible on it — entering Command there
                // would compose a buffer the user can't make sense of,
                // which then swallows every following keystroke silently.
                // `refused` above already confirmed `can_enter`, so this
                // always succeeds from here.
                self.mode = Mode::Command(CommandKind::Colon);
                self.command.clear();
                self.count = None;
                Action::Redraw
            }
            // `/`/`?` mirror `:` exactly, modulo which `CommandKind` they enter — same gate
            // (already confirmed via `refused` above), same buffer/count reset.
            (KeyCode::Char('/'), false) => {
                self.mode = Mode::Command(CommandKind::Search { backward: false });
                self.command.clear();
                self.count = None;
                Action::Redraw
            }
            (KeyCode::Char('?'), false) => {
                self.mode = Mode::Command(CommandKind::Search { backward: true });
                self.command.clear();
                self.count = None;
                Action::Redraw
            }
            (KeyCode::F(1), _) => {
                // render_help_overlay draws nothing below this same bound —
                // both a row floor and, unlike Command's bottom row, a
                // width floor (see help_overlay_fits). `refused` above
                // already confirmed `can_enter`, so this always succeeds
                // from here.
                //
                // vim-first ruling: a SUCCESSFUL help entry neither builds
                // nor consumes a pending count, so it must abort it like
                // any other key that doesn't (the catch-all, Ctrl-l, `:`)
                // — but only on success; a refused one changes nothing,
                // per the `refused` check above.
                self.count = None;
                self.mode = Mode::Help;
                Action::Redraw
            }
            (KeyCode::Char('0'), false) if self.count.is_none() => Action::None,
            (KeyCode::Char(d @ '0'..='9'), false) => {
                let digit = d as usize - '0' as usize;
                let cur = self.count.unwrap_or(0);
                self.count = Some(cur.saturating_mul(10).saturating_add(digit));
                Action::None
            }
            (KeyCode::Char('j'), false) | (KeyCode::Down, _) => {
                Action::Nav(Nav::Lines(self.take_count_i64()))
            }
            (KeyCode::Char('k'), false) | (KeyCode::Up, _) => {
                Action::Nav(Nav::Lines(-self.take_count_i64()))
            }
            // bare d/u match less and zellij; a pager has no delete/undo to clash with.
            (KeyCode::Char('d'), _) => {
                self.count = None;
                Action::Nav(Nav::HalfPage(1))
            }
            (KeyCode::Char('u'), _) => {
                self.count = None;
                Action::Nav(Nav::HalfPage(-1))
            }
            (KeyCode::Char('f'), true) | (KeyCode::PageDown, _) | (KeyCode::Char(' '), false) => {
                self.count = None;
                Action::Nav(Nav::Page(1))
            }
            (KeyCode::Char('b'), true) | (KeyCode::PageUp, _) => {
                self.count = None;
                Action::Nav(Nav::Page(-1))
            }
            (KeyCode::Char('g'), false) => {
                self.pending_g = true;
                Action::None
            }
            (KeyCode::Char('G'), false) => match self.count.take() {
                Some(n) => Action::Nav(Nav::Line(n as u64)),
                None => Action::Nav(Nav::Bottom),
            },
            (KeyCode::Char('%'), false) => match self.count.take() {
                Some(p) => Action::Nav(Nav::Percent(p.min(100) as u8)),
                None => Action::None,
            },
            (KeyCode::Char('h'), false) | (KeyCode::Left, _) => {
                Action::Nav(Nav::Horizontal(-self.take_count_i64()))
            }
            (KeyCode::Char('l'), false) | (KeyCode::Right, _) => {
                Action::Nav(Nav::Horizontal(self.take_count_i64()))
            }
            // a count is consumed/ignored-with-reset like other non-count motions in v1 (`d`/`u`/
            // `f`/`b` above); no chrome gate, like `j`/`k` — `n`/`N` open no new UI surface, they
            // just navigate. Whether there IS an active search to navigate against, and which
            // effective direction `n`/`N` resolve to, are apply-time questions (`apply_nav`'s
            // `SearchNext`/`SearchPrev` arm) — this table only knows the KEY, not the search.
            (KeyCode::Char('n'), false) => {
                self.count = None;
                Action::Nav(Nav::SearchNext)
            }
            (KeyCode::Char('N'), false) => {
                self.count = None;
                Action::Nav(Nav::SearchPrev)
            }
            (KeyCode::Char('l'), true) => {
                // vim aborts a pending count on unmapped input; a redraw
                // request neither builds nor consumes one, so it must not
                // let a count survive through it either.
                self.count = None;
                Action::Redraw
            }
            (KeyCode::Esc, _) => {
                self.count = None;
                self.pending_g = false;
                Action::Cancel
            }
            _ => {
                self.count = None;
                Action::None
            }
        }
    }
}
/// The bottom chrome's height in rows: `rows <= 1` has no room to spare for
/// chrome at all (0) — a 1- or 0-row terminal gets content only, rather
/// than losing its lone row to chrome; `rows` in `2..=3` gets the bottom
/// row alone (1 — the status line, the progress row while a scan is
/// pending, or the command line while composing one; 5a's original
/// threshold, unchanged); `rows >= 4` gets the hint bar too (2), so content
/// never drops below two rows on the hint bar's account. `content_rows`
/// below is `rows - chrome_rows(rows)`. Every caller that needs to know in
/// advance whether there is a bottom row to paint at all goes through
/// `has_bottom_row`, and every caller that needs the full picture — what
/// occupies it, whether the hint bar or the Help overlay or the scrollbar
/// gutter show too — goes through `ChromeLayout::of`, both built on this
/// one function so neither can drift from the reservation `content_rows`
/// actually applies.
fn chrome_rows(rows: u16) -> u16 {
    match rows {
        0 | 1 => 0,
        2 | 3 => 1,
        _ => 2,
    }
}
/// Reserves the terminal's bottom rows for chrome (see `chrome_rows`):
/// content gets the rest. Applied both where terminal rows enter the loop
/// (`run`'s size handling, for `apply_nav`/`handle_resize`/the viewport
/// fetch) and again against the live frame area inside `draw`'s sync
/// closure, mirroring the existing split between the pre-draw `rows`
/// estimate and `f.area()`.
fn content_rows(rows: u16) -> u16 {
    rows - chrome_rows(rows)
}
/// The content area's text width once the scrollbar gutter (Task 4) has
/// taken its column: `cols` unchanged when there is no gutter to make room
/// for, `cols - 1` when there is. A gutter needs the same preconditions
/// `scrollbar_thumb` needs to draw anything — a non-empty file (`size >
/// 0`), at least one content row (`rows > 0`) — plus one of its own: at
/// least one column left over for text once the gutter's column is spoken
/// for (`cols >= 2`), so a one-column terminal keeps its lone column for
/// text rather than losing it to a gutter nobody could read text next to
/// anyway. Threaded exactly like `content_rows`: called once against the
/// pre-draw `cols` estimate ahead of the viewport fetch, and again against
/// the live frame's `area.width` inside `draw`'s sync closure.
fn content_cols(size: u64, rows: u16, cols: u16) -> u16 {
    if has_gutter(size, rows, cols) {
        cols - 1
    } else {
        cols
    }
}
/// Whether `chrome_rows` reserves a bottom row at all — the one
/// precondition the command line, the progress row, and the status line
/// all share; they differ only in precedence once a bottom row exists
/// (see `ChromeLayout::of`), not in whether one exists at all.
fn has_bottom_row(rows: u16) -> bool {
    chrome_rows(rows) > 0
}
/// Whether `render_help_overlay` would draw an INTERIOR, not just a
/// bordered, titled box with nothing inside it. The panel itself is sized
/// `min(dim - 2, cap)` (a 1-cell margin on each side so it never touches
/// the screen edge); `Block::bordered()`'s `inner()` then eats one more
/// cell off each of the panel's four sides for the border (confirmed
/// against `ratatui`'s own `inner()`, not assumed: `Borders::ALL`
/// subtracts 1 for `LEFT`, 1 for `RIGHT`, and — since the block also has
/// a top title — 1 for `TOP`, 1 for `BOTTOM`; no padding is set, so
/// nothing else shrinks it further). Two layers of "subtract 2, floor at
/// 0" compose into "subtract 4 total, floor at 0" — so a nonzero PANEL
/// (the old bound: `dim >= 3`) is not enough; the INTERIOR needs `dim -
/// 2 - 2 >= 1`, i.e. `dim >= 5`, on both axes. Below this a `q`/any-key
/// still closes it, but nothing was ever visible to read — the
/// useless-mode cousin of the invisible-mode class `can_enter` exists to
/// rule out entirely. Empirically confirmed (not just derived) by
/// probing `render_help_overlay` directly at the boundary: at
/// `content_rows(rows) == 4` (raw `rows == 6`) no keymap line renders;
/// at `content_rows(rows) == 5` (raw `rows == 7`) one does; at `cols ==
/// 4` the computed `inner.width == 0`, at `cols == 5` it's `1`. The
/// single source for the Help bound in both dimensions — asked by
/// `ChromeLayout::of` (the `overlay` field), `ChromeLayout::can_enter`,
/// and, through it, both the F1 entry gate and `reconcile_mode`.
/// Command's bottom row has no ANALOGOUS width floor — no bordered panel,
/// no border to eat cells for — but it is not floor-free either: see
/// `ChromeLayout::can_enter`'s `Command` arm and `render_command_line`'s
/// own doc comment for the much weaker, but nonzero, bound it actually
/// needs (`cols >= 3`, not Help's `cols >= 5`).
fn help_overlay_fits(rows: u16, cols: u16) -> bool {
    content_rows(rows) >= 5 && cols >= 5
}
/// Whether the scrollbar gutter has a column to draw into: a non-empty
/// file, at least one content row, and at least one column left over for
/// text once the gutter's own column is spoken for. `content_rows` is the
/// caller's *already-reduced* row count (as `content_cols` above expects),
/// not the raw terminal height.
fn has_gutter(size: u64, content_rows: u16, cols: u16) -> bool {
    size > 0 && content_rows > 0 && cols >= 2
}
/// The single source of truth for what the terminal shows this frame:
/// every renderer draws exactly what it says, every repaint gate asks it,
/// and the mode machine reconciles against it — a mode whose surface
/// cannot render does not persist, so no key can ever route to an
/// invisible prompt or panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChromeLayout {
    pub bottom: BottomRow,
    pub hint_bar: bool,
    pub overlay: bool,
    pub gutter: bool,
    /// Whether Help would actually render if entered right now — mode-
    /// independent, unlike `overlay` (which additionally requires `mode
    /// == Help`). The hint bar reads this to decide whether advertising
    /// `F1 help` would be honest.
    pub help_fits: bool,
}
/// What occupies the terminal's single bottom chrome row this frame, in
/// `ChromeLayout::of`'s precedence order (`Command` beats `Notice` beats
/// `Progress` beats `Status`), or `None` when the terminal has no bottom row
/// to spare at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BottomRow {
    Command,
    /// A transient notice (`App.notice`) — outranks `Progress` (batch 3 (2026-07-23), finding
    /// #13, swapped from the OLD order where `Progress` won: a notice is the direct response to
    /// the user's LAST action -- typically committing a search -- and clears on the very next
    /// key, at which point a still-running scan's progress row re-shows underneath it; the old
    /// order instead hid a fresh regex-error notice behind an unrelated, older pending nav's
    /// progress row while that nav kept moving the viewport underneath it, leaving a bad
    /// pattern effectively invisible) and `Status` (there is something more urgent to say than
    /// the ordinary status line), but still yields to `Command`: composing a command stays
    /// uninterrupted either way.
    ///
    /// Traced edge, accepted rather than chased: `run`'s own periodic redraw tick gates a
    /// pending nav's refresh on `progress_visible()` (see that method's own doc comment), which
    /// is now false for the whole time a notice covers the row, so an UNRELATED still-running
    /// pending nav shows no live progress update on screen until the next key -- unbounded, if
    /// the user simply walks away. This is still strictly better than the invisible-error bug
    /// this precedence swap fixes; auto-expiring notices (so a stale one cannot suppress
    /// progress indefinitely) would close it properly, but that is a deeper UX change and
    /// future work, not this fix's job.
    Notice,
    Progress,
    Status,
    None,
}
impl ChromeLayout {
    /// Derives this frame's layout from the terminal's raw dimensions —
    /// `rows`/`cols` are the full terminal size, not already-reduced
    /// content dimensions; `chrome_rows`/`content_rows`/`has_gutter` apply
    /// that reduction internally, exactly as every other caller of those
    /// functions already does. `notice` is `app.notice.is_some()` — a plain
    /// bool, the same shape `pending` already is, since this type has no
    /// access to `App` itself.
    pub fn of(
        mode: Mode,
        pending: bool,
        notice: bool,
        rows: u16,
        cols: u16,
        size: u64,
    ) -> ChromeLayout {
        let bottom = if !has_bottom_row(rows) {
            BottomRow::None
        } else if matches!(mode, Mode::Command(_)) {
            BottomRow::Command
        } else if notice {
            BottomRow::Notice
        } else if pending {
            BottomRow::Progress
        } else {
            BottomRow::Status
        };
        let fits = help_overlay_fits(rows, cols);
        ChromeLayout {
            bottom,
            hint_bar: chrome_rows(rows) == 2,
            overlay: mode == Mode::Help && fits,
            gutter: has_gutter(size, content_rows(rows), cols),
            help_fits: fits,
        }
    }
    /// Whether `mode` can actually render something at `rows`x`cols` — the
    /// bound `App`'s `:`/F1 entry gates ask before switching into a mode,
    /// and `reconcile_mode` asks after a resize to decide whether to
    /// switch back out of one, so neither can drift from the other or
    /// from what `of` above computes for `bottom`/`overlay`. `Command`'s
    /// width floor is far weaker than Help's — no bordered panel, no
    /// border to eat cells for — but it is not zero either: `cols >= 3`
    /// is the smallest width where TYPING is meaningfully visible, not
    /// just where the row itself isn't blank. `render_command_line` now
    /// left-truncates `:{buffer}_` to its own last `cols` characters (see
    /// its doc comment), so at `cols == 2` the very first digit typed
    /// already sacrifices the leading `:` (`":_"` fits at 2, but `":1_"`
    /// — 3 characters — does not, and left-truncates to `"1_"`); `cols ==
    /// 3` is the first width where `:` plus one digit plus the cursor —
    /// `":1_"` — all fit at once, confirmed directly against that
    /// function's own windowing formula, not assumed.
    fn can_enter(mode: Mode, rows: u16, cols: u16) -> bool {
        match mode {
            Mode::Normal => true,
            // one shared floor for every `CommandKind` — `/`/`?` compose exactly as narrow a
            // line as `:` does, so there is no reason for either to need a different bound.
            Mode::Command(_) => has_bottom_row(rows) && cols >= 3,
            Mode::Help => help_overlay_fits(rows, cols),
        }
    }
    /// Whether the status line specifically — not the command line, not
    /// the progress row — occupies the bottom row this frame.
    pub fn status_visible(self) -> bool {
        matches!(self.bottom, BottomRow::Status)
    }
    /// Whether a pending navigation's progress row occupies the bottom row
    /// this frame — false while `Mode::Command`'s line OR a notice (batch 3
    /// (2026-07-23), finding #13's precedence swap: `BottomRow::Notice`'s own
    /// doc comment) covers it, even if a navigation is in fact pending
    /// underneath.
    pub fn progress_visible(self) -> bool {
        matches!(self.bottom, BottomRow::Progress)
    }
    /// Whether a fresh `SearchSummary` tick has anything to actually change on screen this
    /// frame: the status line's own search segment (`status_visible()`) OR the scrollbar
    /// gutter's match-density ticks (`gutter`) — a sweep tick feeds BOTH independently (`run`'s
    /// own search-summary `select!` arm), and either alone is reason enough to wake and mark
    /// the frame dirty. `status_visible()` ALONE under-gated this (search final review): a
    /// Notice or the command line can cover the status segment while the gutter, a SEPARATE
    /// column, stays fully visible, so a sweep completing then would leave stale ticks on
    /// screen until the next key with the narrower gate.
    pub fn search_summary_visible(self) -> bool {
        self.status_visible() || self.gutter
    }
}
/// The command line's prompt character for the given (already-confirmed-`Command`) mode: `:`
/// for `CommandKind::Colon`, `/`/`?` for `CommandKind::Search` depending on direction. The
/// catch-all is genuinely unreachable, not merely unexpected: this is only ever called from
/// `paint_frame`'s `BottomRow::Command` arm, which `ChromeLayout::of` only ever produces from a
/// `Mode::Command(_)` — the very same `app.mode` this function receives.
fn command_prompt(mode: Mode) -> char {
    match mode {
        Mode::Command(CommandKind::Colon) => ':',
        Mode::Command(CommandKind::Search { backward: false }) => '/',
        Mode::Command(CommandKind::Search { backward: true }) => '?',
        _ => unreachable!("BottomRow::Command implies Mode::Command(_)"),
    }
}
/// The buckets this frame's scrollbar ticks are based on: `None` when there is no active search
/// at all, or when `summary`'s own generation doesn't match `search`'s — a snapshot left over
/// from a SUPERSEDED sweep must never paint ticks for a pattern that isn't the current one any
/// more (the identical generation-stamping discipline `search_status_suffix`'s own doc comment
/// establishes for the status line's search suffix, `render.rs`). A pure function of its two
/// arguments — no `Document`/`doc.search_summary()` borrow inside — so the generation-match and
/// -mismatch cases, and the no-active-search case, can each be pinned directly, the same way
/// `search_status_suffix`'s own generation guard already is.
fn gated_search_buckets(
    search: Option<&ActiveSearch>,
    summary: &ress_core::search::SearchSummary,
) -> Option<std::sync::Arc<[u16; 1024]>> {
    let search = search?;
    (summary.generation == search.generation).then_some(summary.buckets.clone())
}
/// Draws the current screen: fetches the viewport for the terminal size,
/// then hands everything paint-related to `paint_frame` inside the sync
/// draw closure, which derives its OWN `ChromeLayout` from `f.area()` — the
/// terminal's actual size at the moment of painting, never the size queried
/// earlier in this function (see the comment on `real`/`layout` below for
/// why those two can legitimately differ, and `paint_frame`'s own doc
/// comment for the bug that split existed to close). Viewport I/O happens
/// before `draw` because the draw closure is sync; the status line's
/// queries sit right next to it for the same reason, though they are
/// themselves synchronous and infallible — `request_line_number` is nothing
/// but a `watch` send and `line_number` nothing but a `watch` borrow, so
/// this can never stall on a cold read or kill the pager on a read error
/// the way a foreground read on this path once could. Whether the status
/// line keeps converging is no longer this function's concern to report
/// back: the status worker publishes on its own schedule, and the run
/// loop's `status_snapshots` arm notices every change independently, so a
/// painted `L?` needs no help from this call's return value to eventually
/// repaint as the real number.
async fn draw(
    term: &mut crate::terminal::Term,
    doc: &Document,
    app: &App,
    pending: Option<&PendingNav>,
    name: &str,
    cols: u16,
    rows: u16,
) -> anyhow::Result<()> {
    // the pre-draw estimate of the scrollbar gutter's effect on text width
    // (see content_cols) — narrowed here so the fetched rows are already
    // laid out to the width text will actually occupy, mirroring how `rows`
    // above is already content rows rather than raw terminal height. A
    // shrink between this fetch and the paint below means fewer of these
    // rows get painted than were fetched (render_viewport's own clamp-by-
    // breaking drops the rest, harmlessly); a grow means the paint has more
    // room than this fetch filled, so the extra rows stay blank for this
    // one frame — the resize event that caused the grow triggers its own
    // subsequent `draw` call with a freshly fetched, full-sized viewport
    // right behind it, so this never persists past a single frame. A
    // mismatched `fetch_cols` vs. the paint's own live width is similarly
    // harmless: `render_viewport` re-clamps every line to whatever width
    // it's actually given via `set_stringn`, regardless of what width the
    // line was laid out for.
    let fetch_cols = content_cols(doc.size(), rows, cols);
    let view = doc
        .viewport(
            app.top,
            rows as usize,
            fetch_cols as usize,
            app.hscroll,
            app.search.as_ref().map(|s| (&*s.pattern, s.current)),
        )
        .await?;
    let indicator = pending.map(|p| {
        let prog = *p.progress.borrow();
        (p.label, prog.percent())
    });
    // an ADVISORY size query: used only to decide, just below, whether the
    // status line is even worth computing — never to decide what to paint
    // or where (see `paint_frame`, which the actual painting goes through).
    // An earlier version of this comment claimed that because nothing
    // between this line and the `draw` call further down awaits, the two
    // must observe the same terminal size — that reasoning doesn't hold: a
    // resize is delivered to the process as a signal (`SIGWINCH`),
    // asynchronously, at whatever wall-clock moment the kernel chooses,
    // entirely independent of where this task's own `.await` points are.
    // Two plain, back-to-back syscalls can straddle a resize exactly as
    // easily as two syscalls separated by an await can. `Terminal::draw`
    // below runs its own `autoresize()` against the backend's CURRENT size
    // immediately before invoking its closure, so `f.area()` inside that
    // closure — not this `real` — is the single, authoritative, paint-time
    // size.
    let real = term.inner_mut().size()?;
    let layout = ChromeLayout::of(
        app.mode,
        indicator.is_some(),
        app.notice.is_some(),
        real.height,
        real.width,
        doc.size(),
    );
    // while a scan is pending its progress row claims the bottom row
    // instead, and composing a `:` command claims it ahead of both, so the
    // status line's own budgeted queries would go unused either way; a
    // terminal with no row to spare for chrome hides the status line the
    // same way — either way the result would go unused, so skip computing
    // it rather than spend an extra worker request on a number nobody will
    // see this frame. Using the advisory `layout` here rather than a fresh,
    // paint-time one is deliberate and harmless either way it could
    // disagree with the paint: computing `status` when the paint-time
    // layout turns out NOT to show it just wastes this one query (the
    // worker keeps converging regardless — see `Document::status_snapshots`
    // in `run`); not computing it when the paint-time layout WOULD have
    // shown it just skips painting the status line for this one frame
    // (`paint_frame`'s own `Some`/`None` check never panics either way),
    // self-correcting on the very next draw.
    let status = if layout.status_visible() {
        doc.request_line_number(app.top);
        let snap = doc.line_number();
        // the engine is the truth for what each outcome means, so the
        // mapping here is display-only: `Known` shows its number, every
        // other case (not yet answered for this anchor, `Converging`, or
        // `Unavailable`) shows the formatter's uncovered/unresolved `L?`
        // logic — see `ress_core::document::LineNumber`. A snapshot whose
        // anchor does not match `app.top` is simply stale (the worker
        // has not caught up to the latest request yet, indistinguishable
        // from `Converging` here); the run loop's `status_snapshots` arm
        // is what repaints once a fresher one lands, not this mapping.
        let line = (snap.anchor == app.top.offset())
            .then_some(match snap.line {
                ress_core::document::LineNumber::Known(n) => Some(n),
                _ => None,
            })
            .flatten();
        let total = doc.index_total_lines();
        let frontier = *doc.index_frontier().borrow();
        // u128: an exact percent for huge (sparse) files; display-only.
        let pct = (app.top.offset() as u128 * 100 / (doc.size().max(1) as u128)) as u64;
        let mut text = crate::render::status_text(
            name,
            line,
            total,
            frontier.done,
            frontier.lines_so_far,
            pct,
        );
        // the search segment, appended when a search is active — `None` (no suffix) both when
        // there's simply no active search and, transiently, when the latest published summary's
        // generation hasn't caught up to it yet (self-correcting on the very next publish, which
        // the run loop's own search-summary select! arm wakes a repaint for).
        if let Some(search) = app.search.as_ref() {
            let summary = doc.search_summary().borrow().clone();
            let search_pct =
                (summary.scanned_up_to as u128 * 100 / (doc.size().max(1) as u128)) as u64;
            if let Some(suffix) = crate::render::search_status_suffix(
                &search.pattern.raw,
                &summary,
                search.generation,
                search_pct,
            ) {
                text.push_str(&suffix);
            }
        }
        Some(text)
    } else {
        None
    };
    // the scrollbar's own match-tick overlay: `gated_search_buckets` decides whether the
    // freshest published summary actually applies (see its own doc comment); `paint_frame` turns
    // whatever it returns into actual per-row ticks itself, against the live paint-time row
    // count -- never this advisory `rows` -- for the identical resize-race reason
    // `content_area`/`text_area` are always derived from the closure's own `f.area()` rather than
    // a value queried before it (see `paint_frame`'s own doc comment below).
    let summary = doc.search_summary().borrow().clone();
    let search_buckets = gated_search_buckets(app.search.as_ref(), &summary);
    term.inner_mut().draw(|f| {
        let area = f.area();
        paint_frame(
            area,
            f.buffer_mut(),
            &view,
            app,
            indicator,
            status.as_deref(),
            doc.size(),
            search_buckets.as_deref(),
        );
    })?;
    Ok(())
}
/// Paints one frame into `buf` at `area`: the viewport, then chrome exactly
/// as a `ChromeLayout` derived from THIS `area` says to — the hint bar iff
/// `hint_bar`, the bottom row's occupant per `bottom` (`Command`,
/// `Progress`, or `Status`, in that precedence, or nothing at all), the
/// scrollbar gutter iff `gutter`, and the help overlay iff `overlay`. The
/// caller MUST pass the terminal's actual current frame area (ratatui's
/// `f.area()`, from inside `Terminal::draw`'s closure) — never an earlier
/// size query — because every decision this function makes is derived from
/// `area` alone, with nothing captured from an outer scope that could have
/// gone stale.
///
/// This closes a real bug: `draw` used to compute one `ChromeLayout`,
/// before the sync draw closure ran, from a size query taken earlier in
/// the function — call it `real` — and use THAT layout's flags inside the
/// closure, even though the closure's own `content_area`/`text_area` were
/// always keyed off the closure's OWN, separately-queried `f.area()`. A
/// resize landing in the gap between the two queries (always possible —
/// see `draw`'s comment on `real` for why the lack of an `.await` there
/// doesn't rule it out) meant the two could disagree. Concretely: shrink a
/// wide terminal to one column between the two queries, and the stale
/// `layout.gutter == true` would still call `render_scrollbar`, which
/// paints its `█`/`│` glyph over the LAST column of `content_area` — but
/// at the live, narrower width, `content_cols` (already always keyed off
/// the live `f.area()`, even before this fix) had decided there was no
/// spare column for a gutter at all, so that last column was the SAME one
/// `render_viewport` had just written real file content into: the gutter
/// glyph overwrote it. The row-axis analogue: shrink to one row, and a
/// stale `layout.bottom == Status`/`hint_bar == true` paints the status
/// line or hint bar over the terminal's only content row, for the same
/// reason. Deriving everything from one `area`, in one place, makes that
/// disagreement structurally impossible rather than merely unlikely: there
/// is no second, separately-queried size left anywhere in this function
/// for the paint decisions to disagree WITH.
///
/// `search_buckets` follows the identical discipline: `draw` hands over only the raw bucket
/// snapshot (already gated on the active search's own generation, which needs `doc` and so can't
/// happen down here), and this function turns it into actual per-row ticks itself
/// (`crate::render::tick_rows`), against `content_area.height` -- THIS call's own, area-derived
/// row count, never a count `draw` might have computed against a since-resized `area` -- right
/// before handing them to `render_scrollbar`.
// eight parameters, all independently sourced upstream in `draw` and none derivable from another
// (see the doc comment above) -- plain data, not injectable test seams like
// `poll_predicate_with_waiter`'s own (`ress-perf`'s `runner.rs`, the only other `#[allow]` of
// this lint in the workspace, for a different reason). For a function with exactly one
// production call site, grouping any subset of these behind a struct would only silence the
// lint, not reduce any real complexity.
#[allow(clippy::too_many_arguments)]
fn paint_frame(
    area: Rect,
    buf: &mut ratatui::buffer::Buffer,
    view: &ress_core::document::ViewportRender,
    app: &App,
    indicator: Option<(&str, u64)>,
    status: Option<&str>,
    size: u64,
    search_buckets: Option<&[u16; 1024]>,
) {
    let layout = ChromeLayout::of(
        app.mode,
        indicator.is_some(),
        app.notice.is_some(),
        area.height,
        area.width,
        size,
    );
    let content_area = Rect {
        height: content_rows(area.height),
        ..area
    };
    let text_cols = content_cols(size, content_area.height, area.width);
    let text_area = Rect {
        width: text_cols,
        ..content_area
    };
    crate::render::render_viewport(view, text_area, buf);
    if layout.gutter {
        let ticks = search_buckets
            .map(|buckets| crate::render::tick_rows(buckets, size, content_area.height));
        crate::render::render_scrollbar(
            app.top.offset(),
            size,
            content_area,
            buf,
            ticks.as_deref(),
        );
    }
    if layout.hint_bar {
        crate::render::render_hint_bar(app.mode, layout.help_fits, area, buf);
    }
    match layout.bottom {
        BottomRow::Command => {
            crate::render::render_command_line(command_prompt(app.mode), &app.command, area, buf)
        }
        BottomRow::Progress => {
            if let Some((label, pct)) = indicator {
                // Esc only cancels the scan in Mode::Normal — Mode::Help's
                // any-key-closes contract means it just closes Help there
                // instead (mode == Command is impossible here, since
                // BottomRow::Command always outranks Progress).
                let esc_cancels = app.mode == Mode::Normal;
                crate::render::render_progress_row(label, pct, esc_cancels, area, buf);
            }
        }
        BottomRow::Notice => {
            if let Some(notice) = app.notice.as_deref() {
                crate::render::render_notice(notice, area, buf);
            }
        }
        BottomRow::Status => {
            if let Some(status) = status {
                crate::render::render_status_line(status, area, buf);
            }
        }
        BottomRow::None => {}
    }
    if layout.overlay {
        crate::render::render_help_overlay(content_area, buf);
    }
}
/// Applies a navigation intent. A pending operation is superseded by any new
/// motion; an intent that cannot resolve within the interactive budget
/// becomes the new pending operation.
async fn apply_nav(
    doc: &Document,
    app: &mut App,
    pending: &mut Option<ActiveNav>,
    nav: Nav,
    rows: u16,
) -> anyhow::Result<()> {
    // dropping the previous slot cancels its scan.
    *pending = None;
    let page = rows.max(1) as i64;
    let resolution = match nav {
        Nav::Lines(n) => Some(doc.scroll_lines(app.top, n).await?),
        Nav::HalfPage(d) => Some(
            doc.scroll_lines(app.top, d as i64 * (page / 2).max(1))
                .await?,
        ),
        Nav::Page(d) => Some(doc.scroll_lines(app.top, d as i64 * page).await?),
        Nav::Top => {
            app.top = doc.goto_top();
            None
        }
        Nav::Bottom => Some(doc.goto_end(rows as usize).await?),
        Nav::Line(n) => Some(doc.goto_line(n).await?),
        Nav::Percent(p) => Some(doc.goto_percent(p).await?),
        Nav::Horizontal(n) => {
            app.hscroll = app.hscroll.shift(n);
            None
        }
        Nav::SearchNext | Nav::SearchPrev => match app.search.as_ref() {
            None => {
                app.set_notice("no previous search");
                None
            }
            Some(search) => {
                let forward = effective_search_forward(search.pattern.backward, nav);
                // forward: `current + 1` skips past a known match (INCLUSIVE origin — a match
                // starting exactly there must still be found, see `search_next`'s own doc
                // comment); no current yet -> the viewport's own top. backward: `current`
                // UNADJUSTED (no `- 1`) -- `search_next`'s backward leg treats its `origin` as an
                // EXCLUSIVE upper bound already, so the self-hit exclusion falls out of THAT, not
                // of any arithmetic here (pinned directly by
                // `apply_nav_search_prev_after_a_backward_pattern_finds_the_earlier_match_without_a_self_hit`,
                // below).
                let origin = if forward {
                    search.current.map(|m| m + 1).unwrap_or(app.top.offset())
                } else {
                    search.current.unwrap_or(app.top.offset())
                };
                let pattern = search.pattern.clone();
                Some(doc.search_next(origin, &pattern, forward).await?)
            }
        },
    };
    match resolution {
        Some(Resolution::Ready(outcome)) => apply_outcome(app, outcome),
        Some(Resolution::Pending(p)) => *pending = Some(ActiveNav { nav, p }),
        None => {}
    }
    Ok(())
}
/// The effective scan direction for a search-navigation key: `Nav::SearchNext` (`n`) repeats the
/// ORIGINAL search's own direction; `Nav::SearchPrev` (`N`) reverses it -- vim's algebra,
/// independent of which prompt (`/` or `?`) produced the active pattern. Pure XOR, no Document/
/// IO needed to exercise it -- `apply_nav`'s `SearchNext`/`SearchPrev` arm is its only caller.
fn effective_search_forward(pattern_backward: bool, nav: Nav) -> bool {
    !pattern_backward ^ (nav == Nav::SearchPrev)
}
/// Extracts the informative line from a failed pattern compile's own `Display` text, for the
/// "bad pattern: ..." notice (batch 3 (2026-07-23), finding #13b): regex's own pretty
/// multi-line syntax errors put the actual reason on their LAST non-empty line, never the
/// first -- the first line is always the generic "regex parse error:" header, useless on its
/// own (probe-verified against three real shapes: `(` -> "unclosed group", `a{` -> "unclosed
/// counted repetition", a `\p{L}` pattern under this crate's own `unicode(false)` ruling, #8,
/// with no `(?u)` opt-in -> "Unicode not allowed here"). `.unwrap_or(msg)` falls back to the
/// whole message on a hypothetical empty one -- unreachable in practice (`to_string()` on any
/// real error is non-empty), but costs nothing to keep this total rather than panicking.
/// Truncation to the notice row's own width happens downstream, in `render_notice`'s existing
/// `set_stringn` clamp -- not this function's job.
fn last_notice_line(msg: &str) -> &str {
    msg.lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(msg)
}
/// The one place a completed navigation outcome mutates `App` — shared by the interactive path
/// above and the run loop's pending-completion arm so wrap/exhausted notices and the
/// current-match bookkeeping (while a search is active, `search.current` — both the next
/// search's own origin AND the render-facing highlight target, `ActiveSearch`'s own doc
/// comment) can never diverge between them.
fn apply_outcome(app: &mut App, outcome: NavOutcome) {
    match outcome {
        NavOutcome::At(a) => app.top = a,
        NavOutcome::FoundMatch {
            top,
            match_at,
            wrapped,
        } => {
            app.top = top;
            if let Some(search) = app.search.as_mut() {
                search.current = Some(match_at);
            }
            if wrapped {
                app.set_notice("search wrapped");
            }
        }
        NavOutcome::Exhausted => app.set_notice("pattern not found"),
    }
}
/// Restarts any pending operation against the new height: row-dependent
/// jumps (G) captured the old one, and restarting the rest from the unmoved
/// anchor is harmless and simpler than classifying.
async fn handle_resize(
    doc: &Document,
    app: &mut App,
    pending: &mut Option<ActiveNav>,
    rows: u16,
) -> anyhow::Result<()> {
    if let Some(active) = pending.take() {
        let nav = active.nav;
        drop(active);
        apply_nav(doc, app, pending, nav, rows).await?;
    }
    Ok(())
}
/// Whether a mouse wheel tick means a navigation right now — `Some` only in
/// `Mode::Normal`, matching where `j`/`k` mean one too: `handle_key`'s own
/// per-mode dispatch already confines line-at-a-time keys to `Normal`
/// (`Command` and `Help` each consult their own table, neither of which
/// binds a motion), so this is that same confinement applied to the one
/// input source that bypasses `handle_key` entirely. `apply_nav`'s first
/// act is dropping whatever navigation is already pending — exactly right
/// while composing a motion in `Normal`, since a wheel tick there
/// supersedes a running scan the same way a fresh `j`/`k`/`G` would — but
/// wrong in `Command` or `Help`: a wheel tick under either is not a key at
/// all, so it must not close `Help` (only a real key does that), abort the
/// command being composed, touch a pending scan, or repaint, any more than
/// an arriving frontier update does. Returns `None` for that case, and for
/// any `MouseEventKind` that isn't a wheel tick, so `run`'s arm below has a
/// single boolean-shaped question to ask instead of two.
fn wheel_nav(mode: Mode, kind: MouseEventKind) -> Option<Nav> {
    if mode != Mode::Normal {
        return None;
    }
    match kind {
        MouseEventKind::ScrollDown => Some(Nav::Lines(3)),
        MouseEventKind::ScrollUp => Some(Nav::Lines(-3)),
        _ => None,
    }
}
/// Re-samples the terminal's live size and reconciles `app`'s mode against
/// it — the shared first step `Event::Key` and `Event::Mouse` below both
/// take in `run`, so neither ever dispatches against whatever `cols`/
/// `raw_rows` some earlier event happened to leave behind. `crossterm::
/// terminal::size()` is a bare ioctl (see the comment above `run`'s own
/// first call to it) — nothing here waits on `EventStream` or depends on
/// what order the tty fd and the SIGWINCH pipe deliver in. Returns the RAW
/// row count, matching `crossterm::terminal::size()`'s own first call in
/// `run` — the caller still derives `content_rows` from it, exactly as
/// that first call does. The one place `crossterm`'s `(cols, rows)` return
/// swaps into this file's own `rows, cols` order: every other function
/// here — `ChromeLayout::of`, `can_enter`, `reconcile_mode` among them —
/// takes them in that order, so doing the swap anywhere else would just be
/// a second, differently-ordered convention for a caller to keep straight.
fn resample_size(app: &mut App) -> anyhow::Result<(u16, u16)> {
    let (cols, raw_rows) = crossterm::terminal::size()?;
    app.reconcile_mode(raw_rows, cols);
    Ok((raw_rows, cols))
}
/// Runs the event loop until the user quits. Pending navigation resolves in
/// the background: a tick keeps the progress row fresh, completion moves the
/// anchor, Esc cancels, and any new motion supersedes. The background index
/// frontier, the status worker's snapshots (`ress_core::status`), and the
/// search sweep's own `SearchSummary` snapshots are all watched separately
/// from all of that, in a related but not identical shape: each arm only
/// marks the frame `dirty`. The frontier and status arms gate on a freshly
/// computed `ChromeLayout` saying the status line would actually be visible
/// this frame (`layout.status_visible()`) — this one check absorbs both what
/// used to be two separate conditions (a hidden row has nothing to repaint
/// for; a `Mode::Command` line always wins the bottom row's precedence over
/// the status line, so composing a command covers it exactly as a
/// chromeless terminal would) because `ChromeLayout::of` already encodes
/// both. The search-summary arm gates on the wider
/// `ChromeLayout::search_summary_visible()` instead — `status_visible() ||
/// gutter` — since a sweep tick can move the scrollbar gutter's
/// match-density ticks even while a notice or the command line covers the
/// status segment itself; unlike the other two, it can stay armed straight
/// through `Mode::Command` whenever the gutter alone is visible. `Mode::
/// Help` keeps all three arms armed — its overlay paints only over the
/// content area, so the chrome rows stay visible underneath it (see
/// `draw`), unlike Command's. A watch receiver that goes unpolled while its
/// own gate is disarmed does not lose the update: `watch` retains only the
/// latest value, not a queue, so the receiver simply sees it on the next
/// poll instead — for the frontier and status arms specifically, the first
/// one after `layout.status_visible()` next reads true, which happens no
/// later than the `Action::Redraw`/`Cancel` a Command-mode Esc or Enter
/// always produces, and that redraw's own draw call reads the frontier and
/// the status snapshot fresh regardless of `dirty`. The seed behind the
/// status worker's convergence, `draw`'s own `request_line_number` call, is
/// gated on that same `ChromeLayout::status_visible` condition (see
/// `draw`), so leaving Command mode always re-seeds a live request rather
/// than resuming a stale one. The tick is the sole point that turns a dirty
/// flag into a repaint, so a burst of index, status, or search-summary
/// progress between two ticks still costs at most one extra draw. The
/// tick's other reason to redraw — a pending navigation's progress row
/// needs refreshing — asks the same `layout` for `progress_visible()`
/// instead: unlike an earlier version of this gate, there is no exception
/// for `Mode::Command` here anymore, because asking the model costs nothing
/// extra (it is already computed for the three arms above) and a progress
/// row `Mode::Command`'s line is covering has nothing to repaint for,
/// exactly like the status line in that mode. The one exception to all of
/// this is the pending-completion arm: when a background scan finishes, the
/// anchor itself moves, so its draw stays unconditional — visible content
/// changed regardless of whether there is a chrome row to show progress in,
/// or what mode covered it. `name` is the status line's display name for
/// the file.
pub async fn run(doc: Document, name: String) -> anyhow::Result<()> {
    let mut term = crate::terminal::Term::new()?;
    let mut app = App::new();
    let mut pending: Option<ActiveNav> = None;
    // the stream exists before the first paint: a resize during it is
    // buffered, not lost. Constructing `EventStream` is what lazily
    // registers crossterm's SIGWINCH handler for the whole process (via
    // `signal_hook`'s self-pipe, wired into an `mio` poll registry inside
    // crossterm's internal, process-global event reader) — confirmed by
    // reading crossterm 0.29's source, not assumed: `EventStream::new`
    // locks that global reader, which lazily constructs it on first use,
    // which is exactly where the `SIGWINCH` registration happens.
    // Nothing before this point touches that machinery — the
    // `crossterm::terminal::size()` call just below is a direct ioctl,
    // wired to nothing signal-related — so a resize delivered before this
    // line runs falls back to `SIGWINCH`'s default disposition (`Ignore`
    // on Unix) and is dropped by the kernel with no handler to catch it:
    // not deferred, not queued anywhere, genuinely gone, leaving `cols`/
    // `rows` below stale until some LATER resize happens to correct them.
    // Once the handler is registered, `mio`'s epoll-backed readiness
    // tracking starts from THIS registration, not from the first time
    // anything actually calls `poll()` — so a resize landing anywhere
    // between here and this loop's first `events.next()` poll is
    // captured (queued in the signal pipe, not lost) and delivered as a
    // proper `Resize` event, built from a freshly re-queried size rather
    // than the one sampled below, the moment that first poll happens.
    //
    // What that registration-ordering argument does NOT establish —
    // corrected here after once overclaiming it — is anything about the
    // ORDER two already-buffered events come off the stream in. Crossterm's
    // `EventStream` polls the tty fd and the SIGWINCH signal pipe as two
    // separate `mio` registrations in one `Poll::poll()` call; nothing in
    // `mio` or `signal_hook`'s design gives one priority over the other
    // when both are ready at once, so a key typed in the same instant as a
    // resize can just as easily be delivered first, not second. That is why
    // mode-gating no longer depends on stream order at all: every
    // `Event::Key`/`Event::Mouse` arm below calls `resample_size` before
    // doing anything else, which re-samples `crossterm::terminal::size()`
    // — the same bare ioctl as the call just below, wired to nothing
    // signal-related — fresh, so `reconcile_mode` and the `:`/F1 entry
    // gates always judge the terminal as it is at THIS dispatch, never as
    // some earlier event left it. `Event::Resize` still updates `cols`/
    // `rows` and repaints; it is now an idempotent confirmation of a size
    // the next Key/Mouse arm would have re-derived anyway, not the sole
    // source of truth for either of them.
    //
    // The residual gap that remains, re-derived honestly rather than
    // assumed away: `cols`/`rows` sampled just below, and the initial
    // `draw` call further down, can still reflect the PRE-resize size for
    // that one call, since the buffered event hasn't been pulled off the
    // stream yet — and the same is true of the draws the pending-
    // completion, frontier, status, and tick arms issue later, since none
    // of them resample (none of them route through `handle_key`, so none
    // of them gate a mode either). Both cases can fetch a viewport sized
    // for a terminal that has already changed, which is a cosmetic,
    // self-correcting lag — the next Key, Mouse, or Resize event refreshes
    // `cols`/`rows` regardless of which arm needed the fix — and never an
    // ACTION: nothing in any of those arms consults `can_enter` or opens a
    // mode, so there is no invisible prompt or panel left for a key to
    // route into, only a screen briefly showing one frame's worth of stale
    // line count.
    let mut events = crossterm::event::EventStream::new();
    let (mut cols, mut raw_rows) = crossterm::terminal::size()?;
    let mut rows = content_rows(raw_rows);
    // subscribe before the first paint: a scan finishing (or a status
    // resolution landing) in between must still trigger one repaint.
    // `frontier_done`/`status_closed` seed from the current state rather
    // than a fixed `false` because a tiny file's scan can finish before
    // this line ever runs, and each arm below must start disarmed in that
    // case rather than polling a channel that is (or is about to be)
    // closed or permanently quiescent.
    let mut frontier = doc.index_frontier();
    let mut frontier_done = frontier.borrow().done;
    let mut status_rx = doc.status_snapshots();
    let mut status_closed = false;
    // the status line's search twin: same shape, same seeding rationale, and disarmed the same
    // way once a search has genuinely stopped publishing (a teardown race only, `doc` outlives
    // this loop) — see `status_closed`'s own comment just above.
    let mut search_summary_rx = doc.search_summary();
    let mut search_summary_closed = false;
    let mut dirty = false;
    draw(
        &mut term,
        &doc,
        &app,
        pending.as_ref().map(|a| &a.p),
        &name,
        cols,
        rows,
    )
    .await?;
    tracing::info!(target: "perf", "first paint");
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        // one ChromeLayout for the whole iteration, so every gate below
        // that asks "is X visible right now" reads the same answer rather
        // than re-deriving (or drifting from) it independently — see run's
        // doc comment above for how each arm below uses it.
        let layout = ChromeLayout::of(
            app.mode,
            pending.is_some(),
            app.notice.is_some(),
            raw_rows,
            cols,
            doc.size(),
        );
        tokio::select! {
            done = async { (&mut pending.as_mut().unwrap().p.handle).await }, if pending.is_some() => {
                pending = None;
                // an aborted or failed scan leaves the anchor where it was;
                // failures are logged so they stay diagnosable even before
                // the status line exists.
                match done {
                    Ok(Ok(outcome)) => apply_outcome(&mut app, outcome),
                    Ok(Err(e)) => tracing::warn!("background scan failed: {e:#}"),
                    Err(e) if e.is_cancelled() => {}
                    Err(e) => tracing::warn!("background scan panicked: {e:?}"),
                }
                // unconditional: the anchor may have just moved, so visible
                // content can have changed regardless of chrome.
                draw(&mut term, &doc, &app, pending.as_ref().map(|a| &a.p), &name, cols, rows).await?;
                dirty = false;
            }
            changed = frontier.changed(), if !frontier_done && layout.status_visible() => {
                // no draw here: the tick arm below is the sole coalescing
                // point, so a burst of frontier updates between two ticks
                // still costs at most one extra repaint. Disarmed whenever
                // the status line isn't this frame's bottom row occupant —
                // see run's doc comment for why a disarmed poll cannot lose
                // an update.
                match changed {
                    Ok(()) => {
                        dirty = true;
                        frontier_done = frontier.borrow().done;
                    }
                    // the scheduler's sender is gone — a teardown race only,
                    // since `doc` outlives this loop; disarm like a done
                    // frontier would, without drawing or exiting.
                    Err(_) => frontier_done = true,
                }
            }
            changed = status_rx.changed(), if !status_closed && layout.status_visible() => {
                // same shape as the frontier arm just above, including the
                // same "teardown race only" reasoning for the Err case —
                // unlike a finished scan, the status worker never reaches a
                // legitimate permanent-done state on its own (a fresh
                // request always wakes it again), so `status_closed` exists
                // purely as that defensive disarm, not a real steady state.
                match changed {
                    Ok(()) => dirty = true,
                    Err(_) => status_closed = true,
                }
            }
            changed = search_summary_rx.changed(), if !search_summary_closed && layout.search_summary_visible() => {
                // the sweep feeds TWO visible things, not one: the status line's own search
                // segment AND the scrollbar gutter's match-density ticks — see
                // `ChromeLayout::search_summary_visible`'s own doc comment for why a bare
                // `status_visible()` gate (this arm's own first version) under-counted, leaving
                // stale gutter ticks on screen until the next key whenever a Notice or the
                // command line covered the status segment while the gutter stayed visible.
                match changed {
                    Ok(()) => dirty = true,
                    Err(_) => search_summary_closed = true,
                }
            }
            _ = tick.tick() => {
                // a pending nav's progress row needing a refresh only
                // matters if it is this frame's bottom row occupant —
                // covered by Mode::Command's line, it is exactly as
                // invisible as the status line is in that mode, and
                // asking `layout` costs nothing extra here since the two
                // arms above already computed it.
                if dirty || (pending.is_some() && layout.progress_visible()) {
                    draw(&mut term, &doc, &app, pending.as_ref().map(|a| &a.p), &name, cols, rows).await?;
                    dirty = false;
                }
            }
            ev = events.next() => {
                let Some(ev) = ev else {
                    // dropping the slot cancels any live scan.
                    pending.take();
                    break;
                };
                // any `?` from here unwinds through `pending`, whose drop
                // cancels a live scan.
                match ev? {
                    Event::Key(key) => {
                        // fresh before this key is dispatched — see
                        // resample_size and run's own doc comment above for
                        // why this, not the Resize arm below, is now the
                        // gates' actual source of truth.
                        (raw_rows, cols) = resample_size(&mut app)?;
                        rows = content_rows(raw_rows);
                        // sampled BEFORE handle_key, whose first act (after reconcile) is
                        // clearing it -- this is the run loop's own half of the "any key clears
                        // the notice" contract: the row must repaint even when the routed key's
                        // own action is Action::None (see the Action::None arm below and
                        // handle_key's own doc comment).
                        let notice_was_cleared = app.notice.is_some();
                        match app.handle_key(key, raw_rows, cols) {
                            Action::Quit => {
                                pending.take();
                                break;
                            }
                            Action::Nav(nav) => {
                                apply_nav(&doc, &mut app, &mut pending, nav, rows).await?;
                                draw(&mut term, &doc, &app, pending.as_ref().map(|a| &a.p), &name, cols, rows).await?;
                                dirty = false;
                            }
                            Action::Cancel => {
                                pending = None;
                                draw(&mut term, &doc, &app, pending.as_ref().map(|a| &a.p), &name, cols, rows).await?;
                                dirty = false;
                            }
                            Action::Redraw => {
                                draw(&mut term, &doc, &app, pending.as_ref().map(|a| &a.p), &name, cols, rows).await?;
                                dirty = false;
                            }
                            Action::CommitSearch { raw, backward } => {
                                // compilation happens HERE, not in handle_key: this is where
                                // Document lives, and handle_key stays IO- and regex-free.
                                match ress_core::search::SearchPattern::compile(&raw, backward) {
                                    Err(e) => {
                                        let msg = e.to_string();
                                        app.set_notice(&format!(
                                            "bad pattern: {}",
                                            last_notice_line(&msg)
                                        ));
                                    }
                                    Ok(p) => {
                                        let pattern = std::sync::Arc::new(p);
                                        let generation = doc.start_search_sweep(pattern.clone());
                                        app.search = Some(ActiveSearch {
                                            pattern,
                                            generation,
                                            current: None,
                                        });
                                        apply_nav(&doc, &mut app, &mut pending, Nav::SearchNext, rows).await?;
                                    }
                                }
                                draw(&mut term, &doc, &app, pending.as_ref().map(|a| &a.p), &name, cols, rows).await?;
                                dirty = false;
                            }
                            Action::None => {
                                // every OTHER arm above already redraws unconditionally, so a
                                // cleared notice needs no special handling there — only here,
                                // where nothing else would otherwise trigger the repaint that
                                // makes the clearing visible.
                                if notice_was_cleared {
                                    draw(&mut term, &doc, &app, pending.as_ref().map(|a| &a.p), &name, cols, rows).await?;
                                    dirty = false;
                                }
                            }
                        }
                    }
                    Event::Mouse(m) => {
                        // same shared resample as Event::Key, above —
                        // freshness here is orthogonal to which mode the
                        // wheel is allowed to navigate in, so it stays
                        // unconditional even though wheel_nav below often
                        // returns None afterward.
                        (raw_rows, cols) = resample_size(&mut app)?;
                        rows = content_rows(raw_rows);
                        // the wheel bypasses handle_key's router entirely —
                        // this was the second hidden-action path the "only
                        // Key's mode-gating matters" framing above missed:
                        // an ungated wheel tick could cancel a pending scan
                        // out from under an open Command or Help, the same
                        // class of bug Pass 7 closed for Key. wheel_nav is
                        // the gate that closes it for Mouse.
                        if let Some(nav) = wheel_nav(app.mode, m.kind) {
                            apply_nav(&doc, &mut app, &mut pending, nav, rows).await?;
                            draw(&mut term, &doc, &app, pending.as_ref().map(|a| &a.p), &name, cols, rows).await?;
                            dirty = false;
                        }
                    }
                    Event::Resize(c, r) => {
                        cols = c;
                        raw_rows = r;
                        rows = content_rows(r);
                        // resample_size's own reconcile_mode call covers
                        // Key/Mouse above; Resize is the one arm that
                        // updates cols/rows without going through it, so it
                        // still calls reconcile_mode directly — a shrink
                        // that removes Command's bottom row, or Help's row
                        // or column headroom, must exit that mode right
                        // here, or the next key routes to a prompt/panel
                        // nobody can see (see reconcile_mode).
                        app.reconcile_mode(raw_rows, cols);
                        handle_resize(&doc, &mut app, &mut pending, rows).await?;
                        draw(&mut term, &doc, &app, pending.as_ref().map(|a| &a.p), &name, cols, rows).await?;
                        dirty = false;
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ress_core::document::MAX_HSCROLL;
    /// The size a test passes to `handle_key` when its behavior has nothing
    /// to do with terminal size — ample on both axes, so every chrome-ladder
    /// gate this file has (`:`, F1, and any that come later) trivially
    /// clears. Also a greppable marker: any test passing `AMPLE` has
    /// declared itself size-indifferent, so a future pass auditing which
    /// tests exercise sizing at all can `grep` for the tests that DON'T.
    const AMPLE: u16 = u16::MAX;
    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn nav_doc() -> Document {
        // 20 one-char lines: line k starts at byte 2k; size 40.
        let mut data = Vec::new();
        for i in 0..20u8 {
            data.push(b'a' + i);
            data.push(b'\n');
        }
        let src = std::sync::Arc::new(ress_core::source::MockSource::new(data));
        Document::new(src, ress_core::Config::default())
    }
    #[test]
    fn chrome_rows_ladder_reserves_zero_one_or_two_rows() {
        // rows <= 1: no room to spare for chrome at all.
        assert_eq!(chrome_rows(0), 0);
        assert_eq!(chrome_rows(1), 0);
        // rows in 2..=3: the bottom row alone (5a's original threshold).
        assert_eq!(chrome_rows(2), 1);
        assert_eq!(chrome_rows(3), 1);
        // rows >= 4: the hint bar joins the bottom row.
        assert_eq!(chrome_rows(4), 2);
        assert_eq!(chrome_rows(5), 2);
        assert_eq!(chrome_rows(24), 2);
    }
    #[test]
    fn content_rows_reserves_the_bottom_row_but_not_below_two() {
        // the reservation formula run() applies where terminal rows enter:
        // apply_nav/handle_resize/draw all consume its result, so a bug here
        // would misplace every row-dependent nav, not just the status line.
        // a tall terminal (rows >= 4) now reserves two rows, the hint bar
        // plus the bottom row — see chrome_rows_ladder_reserves_zero_one_or_two_rows.
        assert_eq!(content_rows(24), 22);
        assert_eq!(content_rows(2), 1);
        // below two rows there is no room to spare for chrome: content gets
        // the lone row (or zero) instead of the status line claiming it.
        assert_eq!(content_rows(1), 1);
        assert_eq!(content_rows(0), 0);
    }
    #[test]
    fn content_cols_reserves_a_column_for_the_gutter_when_shown() {
        assert_eq!(content_cols(100, 10, 80), 79);
    }
    #[test]
    fn content_cols_keeps_full_width_without_a_gutter() {
        assert_eq!(content_cols(0, 10, 80), 80, "an empty file draws no gutter");
        assert_eq!(
            content_cols(100, 0, 80),
            80,
            "no content rows draws no gutter"
        );
        assert_eq!(
            content_cols(100, 10, 1),
            1,
            "a one-column terminal keeps its lone column for text"
        );
        assert_eq!(content_cols(100, 10, 0), 0);
    }
    #[test]
    fn chrome_layout_matrix_covers_every_mode_pending_rows_cols_size_cell() {
        // exhaustive over mode x pending x rows(0..=7) x cols(2,4,5,6,80) x
        // size(0,1). rows 0..=7 spans every rung of the chrome_rows ladder
        // plus the help_overlay_fits row transition at rows=7 — an empty
        // overlay INTERIOR (a bordered, titled box with nothing inside),
        // not just an empty panel, see help_overlay_fits' doc comment;
        // cols {2,4,5,6,80} spans the overlay's width bound (5) on both
        // sides plus a slope check and a normal width — 2 also happens to
        // be the gutter's own bound, so it does double duty. The gutter's
        // cols boundary in isolation gets its own focused test below too.
        for rows in 0u16..=7 {
            for cols in [2u16, 4, 5, 6, 80] {
                for size in [0u64, 1] {
                    for mode in [
                        Mode::Normal,
                        Mode::Command(CommandKind::Colon),
                        Mode::Command(CommandKind::Search { backward: false }),
                        Mode::Command(CommandKind::Search { backward: true }),
                        Mode::Help,
                    ] {
                        for pending in [false, true] {
                            for notice in [false, true] {
                                let layout =
                                    ChromeLayout::of(mode, pending, notice, rows, cols, size);
                                let has_bottom = !matches!(rows, 0 | 1);
                                let expected_bottom = if !has_bottom {
                                    BottomRow::None
                                } else if matches!(mode, Mode::Command(_)) {
                                    BottomRow::Command
                                } else if notice {
                                    BottomRow::Notice
                                } else if pending {
                                    BottomRow::Progress
                                } else {
                                    BottomRow::Status
                                };
                                assert_eq!(
                                    layout.bottom, expected_bottom,
                                    "bottom at rows={rows} cols={cols} size={size} mode={mode:?} pending={pending} notice={notice}"
                                );
                                assert_eq!(layout.hint_bar, rows >= 4, "hint_bar at rows={rows}");
                                let content = content_rows(rows);
                                assert_eq!(
                                    layout.overlay,
                                    mode == Mode::Help && content >= 5 && cols >= 5,
                                    "overlay at rows={rows} cols={cols} mode={mode:?}"
                                );
                                assert_eq!(
                                    layout.gutter,
                                    size > 0 && content > 0 && cols >= 2,
                                    "gutter at rows={rows} cols={cols} size={size}"
                                );
                                assert_eq!(
                                    layout.help_fits,
                                    content >= 5 && cols >= 5,
                                    "help_fits at rows={rows} cols={cols} — mode-independent, unlike overlay"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn chrome_layout_gutter_needs_at_least_two_columns() {
        // a nonzero size and a content row exist at rows=5, so only the
        // cols dimension is under test here.
        assert!(ChromeLayout::of(Mode::Normal, false, false, 5, 2, 1).gutter);
        assert!(!ChromeLayout::of(Mode::Normal, false, false, 5, 1, 1).gutter);
        assert!(!ChromeLayout::of(Mode::Normal, false, false, 5, 0, 1).gutter);
    }
    #[test]
    fn chrome_layout_notice_outranks_status_and_progress_but_not_command() {
        // batch 3 (2026-07-23), finding #13: the precedence rung, pinned directly --
        // Command(_) > Notice > Progress > Status (swapped from Notice/Progress's old order;
        // see BottomRow::Notice's own doc comment for why).
        assert_eq!(
            ChromeLayout::of(Mode::Normal, false, true, 5, 80, 0).bottom,
            BottomRow::Notice
        );
        assert_eq!(
            ChromeLayout::of(Mode::Normal, false, false, 5, 80, 0).bottom,
            BottomRow::Status,
            "no notice: falls through to Status, unchanged"
        );
        assert_eq!(
            ChromeLayout::of(Mode::Normal, true, true, 5, 80, 0).bottom,
            BottomRow::Notice,
            "a notice set while a pending nav still runs must render over its progress row -- \
             the notice is the direct response to the user's LAST action, e.g. a regex compile \
             error from a just-committed search, and must not hide behind an unrelated older \
             nav's progress row while that nav keeps moving the viewport underneath it"
        );
        assert_eq!(
            ChromeLayout::of(Mode::Normal, true, false, 5, 80, 0).bottom,
            BottomRow::Progress,
            "no notice: a pending scan still outranks Status, unchanged"
        );
        assert_eq!(
            ChromeLayout::of(Mode::Command(CommandKind::Colon), true, true, 5, 80, 0).bottom,
            BottomRow::Command,
            "composing a command still outranks both"
        );
    }
    #[test]
    fn search_summary_visible_wakes_on_either_the_status_segment_or_the_gutter() {
        // regression (search final review): a bare `status_visible()` gate on the
        // search-summary select! arm left the gutter's own match-density ticks stale
        // whenever a Notice/the command line covered the status segment but the gutter
        // (a SEPARATE column) stayed visible -- `search_summary_visible` must say `true`
        // whenever EITHER is visible, not just when both/status alone is.
        assert!(
            ChromeLayout::of(Mode::Normal, false, false, 5, 80, 0).search_summary_visible(),
            "status visible, no gutter (empty file): still relevant via status"
        );
        assert!(
            ChromeLayout::of(Mode::Normal, false, true, 5, 80, 1).search_summary_visible(),
            "a Notice hides the status segment, but the gutter (nonzero size) stays visible"
        );
        assert!(
            !ChromeLayout::of(Mode::Normal, false, true, 5, 80, 0).search_summary_visible(),
            "a Notice hides status AND an empty file has no gutter: genuinely nothing to wake for"
        );
        assert!(
            ChromeLayout::of(Mode::Normal, false, false, 5, 80, 1).search_summary_visible(),
            "both visible at once: trivially true"
        );
    }
    #[test]
    fn wheel_nav_navigates_only_in_normal_mode() {
        // the wheel bypasses handle_key's router entirely, so this is the
        // only place mode ever gates it — see wheel_nav's own doc comment
        // for why Command/Help need a true no-op, not just a withheld nav.
        // `wheel_nav` never inspects `CommandKind` (only "is this Normal or
        // not"), so one representative Command variant is enough here.
        for mode in [Mode::Normal, Mode::Command(CommandKind::Colon), Mode::Help] {
            let down = wheel_nav(mode, MouseEventKind::ScrollDown);
            let up = wheel_nav(mode, MouseEventKind::ScrollUp);
            if mode == Mode::Normal {
                assert_eq!(down, Some(Nav::Lines(3)), "mode={mode:?}");
                assert_eq!(up, Some(Nav::Lines(-3)), "mode={mode:?}");
            } else {
                assert_eq!(down, None, "mode={mode:?}");
                assert_eq!(up, None, "mode={mode:?}");
            }
        }
    }
    #[test]
    fn wheel_nav_ignores_non_scroll_mouse_events_in_every_mode() {
        for mode in [Mode::Normal, Mode::Command(CommandKind::Colon), Mode::Help] {
            for kind in [
                MouseEventKind::Moved,
                MouseEventKind::Down(crossterm::event::MouseButton::Left),
                MouseEventKind::ScrollLeft,
                MouseEventKind::ScrollRight,
            ] {
                assert_eq!(wheel_nav(mode, kind), None, "mode={mode:?} kind={kind:?}");
            }
        }
    }
    #[test]
    fn paint_frame_never_lets_the_gutter_overwrite_the_only_text_column() {
        // the P2 regression, pinned at the one seam this refactor can still
        // reach with a unit test: paint_frame derives its layout from its
        // OWN `area` parameter, with no other, possibly-stale size source
        // left in scope to disagree with it — so calling it with a single,
        // internally-consistent area can never reproduce the old
        // stale-vs-live mismatch by construction. What this test pins
        // instead is that the gutter and the content column it can claim
        // are always partitioned consistently AT a given area, across the
        // exact boundary where a gutter starts existing at all.
        let app = App::new();
        let view = ress_core::document::ViewportRender {
            rows: vec!["ab".to_string()],
            marks: vec![ress_core::document::RowMarks::default()],
        };
        // cols=1: has_gutter needs cols>=2, so no gutter at all — the lone
        // column must show the viewport's own content.
        let area = Rect::new(0, 0, 1, 3);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        paint_frame(area, &mut buf, &view, &app, None, None, 100, None);
        assert_eq!(
            buf[(0, 0)].symbol(),
            "a",
            "no gutter at cols=1: the lone column is content"
        );
        // cols=2: the gutter's own bound is now met — it claims the
        // rightmost column, and the viewport's own text narrows to make
        // room for it, both decided from this same, single `area`.
        let area2 = Rect::new(0, 0, 2, 3);
        let mut buf2 = ratatui::buffer::Buffer::empty(area2);
        paint_frame(area2, &mut buf2, &view, &app, None, None, 100, None);
        assert_eq!(
            buf2[(0, 0)].symbol(),
            "a",
            "content still shows in the narrowed text column"
        );
        assert_eq!(
            buf2[(1, 0)].symbol(),
            "█",
            "the rightmost column is now the gutter's thumb, not text"
        );
    }
    #[test]
    fn paint_frame_never_lets_the_bottom_row_overwrite_the_only_content_row() {
        // same shape as the gutter test above, on the row axis: at
        // rows=1, chrome_rows is 0 (no bottom row exists at all), so a
        // `status` that is `Some` must still never appear — there is no
        // bottom row for `paint_frame`'s own, area-derived layout to
        // route it into, regardless of what the caller passed in.
        let app = App::new();
        let view = ress_core::document::ViewportRender {
            rows: vec!["only line".to_string()],
            marks: vec![ress_core::document::RowMarks::default()],
        };
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        paint_frame(
            area,
            &mut buf,
            &view,
            &app,
            None,
            Some("must never show"),
            100,
            None,
        );
        let row: String = (0..20).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert!(
            row.starts_with("only line"),
            "the lone row must stay content, not a status line: {row:?}"
        );
    }
    #[test]
    fn paint_frame_threads_search_buckets_into_scrollbar_ticks() {
        // the call-site wiring: paint_frame must turn whatever buckets draw threads through into
        // actual per-row ticks against ITS OWN live content_area height (never a count the caller
        // might have computed against a since-resized area) before handing them to
        // render_scrollbar.
        let app = App::new();
        let view = ress_core::document::ViewportRender {
            rows: vec!["x".to_string()],
            marks: vec![ress_core::document::RowMarks::default()],
        };
        // rows=5 -> chrome_rows(5) == 2 -> content_area height 3, distinct from the raw 5 so a
        // bug threading area.height instead of content_area.height into tick_rows would show up
        // as a wrongly-placed (or missing) tick rather than passing by accident.
        let area = Rect::new(0, 0, 10, 5);
        let mut buckets = [0u16; 1024];
        buckets[1023] = 1; // last bucket, [1023,1024) of a 1024-byte file
        let mut buf = ratatui::buffer::Buffer::empty(area);
        paint_frame(
            area,
            &mut buf,
            &view,
            &app,
            None,
            None,
            1024,
            Some(&buckets),
        );
        // 3 content rows over 1024 bytes: row 2 spans [682,1024) and so covers byte 1023 -- the
        // anchor is 0 (App::new()'s default top), so the thumb sits at row 0 and cannot mask it.
        assert_eq!(
            buf[(9, 2)].symbol(),
            "·",
            "the last content row must carry the tick for the last bucket"
        );
        assert_eq!(buf[(9, 0)].symbol(), "█", "the thumb still sits at row 0");
    }
    fn search_summary(generation: u64, buckets: [u16; 1024]) -> ress_core::search::SearchSummary {
        ress_core::search::SearchSummary {
            generation,
            matches: 0,
            buckets: std::sync::Arc::new(buckets),
            scanned_up_to: 0,
            done: false,
            failed: false,
        }
    }
    #[test]
    fn gated_search_buckets_is_none_without_an_active_search() {
        let summary = search_summary(1, [1u16; 1024]);
        assert_eq!(gated_search_buckets(None, &summary), None);
    }
    #[test]
    fn gated_search_buckets_returns_the_buckets_when_generations_match() {
        let pattern = std::sync::Arc::new(SearchPattern::compile("needle", false).unwrap());
        let search = ActiveSearch {
            pattern,
            generation: 3,
            current: None,
        };
        let summary = search_summary(3, [7u16; 1024]);
        assert_eq!(
            gated_search_buckets(Some(&search), &summary),
            Some(std::sync::Arc::new([7u16; 1024]))
        );
    }
    #[test]
    fn gated_search_buckets_is_none_on_generation_mismatch() {
        // a snapshot left over from a SUPERSEDED sweep must never be mistaken for describing the
        // current pattern -- mirrors search_status_suffix_is_none_on_generation_mismatch
        // (render.rs), the status line's own identical guard.
        let pattern = std::sync::Arc::new(SearchPattern::compile("needle", false).unwrap());
        let search = ActiveSearch {
            pattern,
            generation: 2,
            current: None,
        };
        let summary = search_summary(1, [9u16; 1024]);
        assert_eq!(gated_search_buckets(Some(&search), &summary), None);
    }
    #[test]
    fn chrome_layout_overlay_needs_at_least_five_columns() {
        // rows=7 alone already satisfies the row half of the bound (an
        // interior height of at least one), so only the cols dimension is
        // under test here.
        assert!(!ChromeLayout::of(Mode::Help, false, false, 7, 4, 0).overlay);
        assert!(ChromeLayout::of(Mode::Help, false, false, 7, 5, 0).overlay);
    }
    #[test]
    fn chrome_layout_can_enter_normal_at_any_size() {
        assert!(ChromeLayout::can_enter(Mode::Normal, 0, 0));
        assert!(ChromeLayout::can_enter(Mode::Normal, u16::MAX, u16::MAX));
    }
    #[test]
    fn chrome_layout_can_enter_command_needs_a_bottom_row_and_three_columns() {
        // one shared floor for every `CommandKind` — pinned with `Colon` here;
        // `search_mode_respects_the_same_size_floor_as_colon` pins the `/`/`?` parity directly.
        assert!(!ChromeLayout::can_enter(
            Mode::Command(CommandKind::Colon),
            1,
            u16::MAX
        ));
        assert!(!ChromeLayout::can_enter(
            Mode::Command(CommandKind::Colon),
            2,
            0
        ));
        assert!(!ChromeLayout::can_enter(
            Mode::Command(CommandKind::Colon),
            2,
            2
        ));
        assert!(ChromeLayout::can_enter(
            Mode::Command(CommandKind::Colon),
            2,
            3
        ));
    }
    #[test]
    fn chrome_layout_can_enter_help_matches_the_overlay_bound_in_both_dimensions() {
        assert!(!ChromeLayout::can_enter(Mode::Help, 6, 80));
        assert!(ChromeLayout::can_enter(Mode::Help, 7, 80));
        assert!(!ChromeLayout::can_enter(Mode::Help, 80, 4));
        assert!(ChromeLayout::can_enter(Mode::Help, 80, 5));
    }
    #[test]
    fn reconcile_mode_exits_command_when_a_resize_removes_the_bottom_row() {
        // the P2 finding's exact repro, driven at the unit level (through
        // handle_key's own reconcile_mode call) rather than through a real
        // Resize event: a shrink below Command's bound must not leave `j`
        // routing into handle_key_command's dead key table.
        let mut app = App::new();
        assert_eq!(app.handle_key(key(':'), 2, AMPLE), Action::Redraw);
        assert_eq!(app.mode, Mode::Command(CommandKind::Colon));
        assert_eq!(
            app.handle_key(key('j'), 1, AMPLE),
            Action::Nav(Nav::Lines(1))
        );
        assert_eq!(app.mode, Mode::Normal);
    }
    #[test]
    fn reconcile_mode_exits_command_when_a_resize_removes_the_third_column() {
        // same discriminator, tripped by cols alone — rows stays at the
        // App::new() default throughout.
        let mut app = App::new();
        assert_eq!(app.handle_key(key(':'), AMPLE, 3), Action::Redraw);
        assert_eq!(app.mode, Mode::Command(CommandKind::Colon));
        assert_eq!(
            app.handle_key(key('j'), AMPLE, 2),
            Action::Nav(Nav::Lines(1))
        );
        assert_eq!(app.mode, Mode::Normal);
    }
    #[test]
    fn reconcile_mode_clears_the_command_buffer_on_exit() {
        let mut app = App::new();
        app.handle_key(key(':'), 2, AMPLE);
        app.handle_key(key('4'), 2, AMPLE);
        assert_eq!(app.command_buffer(), "4");
        app.handle_key(key('j'), 1, AMPLE);
        assert_eq!(app.mode, Mode::Normal);
        app.handle_key(key(':'), 2, AMPLE);
        assert_eq!(
            app.command_buffer(),
            "",
            "a stale buffer from the exited mode must not resurface in the next one"
        );
    }
    #[test]
    fn reconcile_mode_exits_help_when_a_resize_shrinks_below_the_overlay_bound() {
        // q while Help is still validly open closes Help (Redraw); q after
        // reconcile_mode has already exited it back to Normal quits the
        // pager instead — the two outcomes are only distinguishable if
        // reconciliation actually ran.
        let mut app = App::new();
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE), 7, AMPLE),
            Action::Redraw
        );
        assert_eq!(app.mode, Mode::Help);
        assert_eq!(app.handle_key(key('q'), 6, AMPLE), Action::Quit);
    }
    #[test]
    fn reconcile_mode_exits_help_when_a_resize_shrinks_below_the_overlay_width() {
        // the same discriminator as the rows version above, but tripped by
        // cols alone — rows stays comfortably large throughout.
        let mut app = App::new();
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE), AMPLE, 80),
            Action::Redraw
        );
        assert_eq!(app.mode, Mode::Help);
        assert_eq!(app.handle_key(key('q'), AMPLE, 2), Action::Quit);
    }
    #[test]
    fn f1_is_a_no_op_when_the_overlay_has_no_room_to_render_due_to_width() {
        // rows stays at the App::new() default (effectively unbounded);
        // cols alone refuses the overlay here. cols=4 gives a nonzero
        // PANEL but a zero-width INTERIOR (see help_overlay_fits' doc
        // comment) — a true no-op is required there too, not just at an
        // obviously-too-narrow cols=2.
        let mut app = App::new();
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE), AMPLE, 2),
            Action::None
        );
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE), AMPLE, 4),
            Action::None
        );
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE), AMPLE, 5),
            Action::Redraw
        );
        assert_eq!(app.mode, Mode::Help);
    }
    #[test]
    fn q_quits() {
        assert_eq!(App::new().handle_key(key('q'), AMPLE, AMPLE), Action::Quit);
    }
    #[test]
    fn ctrl_c_quits() {
        assert_eq!(App::new().handle_key(ctrl('c'), AMPLE, AMPLE), Action::Quit);
    }
    #[test]
    fn j_scrolls_down_one() {
        assert_eq!(
            App::new().handle_key(key('j'), AMPLE, AMPLE),
            Action::Nav(Nav::Lines(1))
        );
    }
    #[test]
    fn k_scrolls_up_one() {
        assert_eq!(
            App::new().handle_key(key('k'), AMPLE, AMPLE),
            Action::Nav(Nav::Lines(-1))
        );
    }
    #[test]
    fn count_prefix_multiplies_motion() {
        let mut app = App::new();
        assert_eq!(app.handle_key(key('1'), AMPLE, AMPLE), Action::None);
        assert_eq!(app.handle_key(key('2'), AMPLE, AMPLE), Action::None);
        assert_eq!(
            app.handle_key(key('j'), AMPLE, AMPLE),
            Action::Nav(Nav::Lines(12))
        );
    }
    #[test]
    fn count_resets_after_use() {
        let mut app = App::new();
        app.handle_key(key('5'), AMPLE, AMPLE);
        app.handle_key(key('j'), AMPLE, AMPLE);
        assert_eq!(
            app.handle_key(key('j'), AMPLE, AMPLE),
            Action::Nav(Nav::Lines(1))
        );
    }
    #[test]
    fn gg_goes_to_top() {
        let mut app = App::new();
        assert_eq!(app.handle_key(key('g'), AMPLE, AMPLE), Action::None);
        assert_eq!(
            app.handle_key(key('g'), AMPLE, AMPLE),
            Action::Nav(Nav::Top)
        );
    }
    #[test]
    fn ge_goes_to_bottom() {
        let mut app = App::new();
        assert_eq!(app.handle_key(key('g'), AMPLE, AMPLE), Action::None);
        assert_eq!(
            app.handle_key(key('e'), AMPLE, AMPLE),
            Action::Nav(Nav::Bottom)
        );
    }
    #[test]
    fn g_then_other_key_cancels() {
        let mut app = App::new();
        app.handle_key(key('g'), AMPLE, AMPLE);
        assert_eq!(
            app.handle_key(key('j'), AMPLE, AMPLE),
            Action::Nav(Nav::Lines(1))
        );
    }
    #[test]
    fn a_cancelled_chord_drops_the_count() {
        // 5g followed by j: vim aborts the count with the chord, so the
        // j is a single-line motion, not five.
        let mut app = App::new();
        app.handle_key(key('5'), AMPLE, AMPLE);
        app.handle_key(key('g'), AMPLE, AMPLE);
        assert_eq!(
            app.handle_key(key('j'), AMPLE, AMPLE),
            Action::Nav(Nav::Lines(1))
        );
    }
    #[test]
    fn shift_g_goes_to_bottom() {
        assert_eq!(
            App::new().handle_key(key('G'), AMPLE, AMPLE),
            Action::Nav(Nav::Bottom)
        );
    }
    #[test]
    fn count_g_goes_to_line() {
        let mut app = App::new();
        app.handle_key(key('4'), AMPLE, AMPLE);
        app.handle_key(key('2'), AMPLE, AMPLE);
        assert_eq!(
            app.handle_key(key('G'), AMPLE, AMPLE),
            Action::Nav(Nav::Line(42))
        );
    }
    #[test]
    fn bare_g_still_goes_to_bottom() {
        assert_eq!(
            App::new().handle_key(key('G'), AMPLE, AMPLE),
            Action::Nav(Nav::Bottom)
        );
    }
    #[test]
    fn count_gg_goes_to_line() {
        let mut app = App::new();
        app.handle_key(key('7'), AMPLE, AMPLE);
        app.handle_key(key('g'), AMPLE, AMPLE);
        assert_eq!(
            app.handle_key(key('g'), AMPLE, AMPLE),
            Action::Nav(Nav::Line(7))
        );
    }
    #[test]
    fn bare_gg_still_goes_to_top() {
        let mut app = App::new();
        app.handle_key(key('g'), AMPLE, AMPLE);
        assert_eq!(
            app.handle_key(key('g'), AMPLE, AMPLE),
            Action::Nav(Nav::Top)
        );
    }
    #[test]
    fn ge_ignores_a_count() {
        let mut app = App::new();
        app.handle_key(key('9'), AMPLE, AMPLE);
        app.handle_key(key('g'), AMPLE, AMPLE);
        assert_eq!(
            app.handle_key(key('e'), AMPLE, AMPLE),
            Action::Nav(Nav::Bottom)
        );
    }
    #[test]
    fn count_percent_jumps() {
        let mut app = App::new();
        app.handle_key(key('5'), AMPLE, AMPLE);
        app.handle_key(key('0'), AMPLE, AMPLE);
        assert_eq!(
            app.handle_key(key('%'), AMPLE, AMPLE),
            Action::Nav(Nav::Percent(50))
        );
    }
    #[test]
    fn bare_percent_does_nothing() {
        assert_eq!(App::new().handle_key(key('%'), AMPLE, AMPLE), Action::None);
    }
    #[test]
    fn ctrl_d_half_page_down() {
        assert_eq!(
            App::new().handle_key(ctrl('d'), AMPLE, AMPLE),
            Action::Nav(Nav::HalfPage(1))
        );
    }
    #[test]
    fn bare_d_half_page_down() {
        assert_eq!(
            App::new().handle_key(key('d'), AMPLE, AMPLE),
            Action::Nav(Nav::HalfPage(1))
        );
    }
    #[test]
    fn bare_u_half_page_up() {
        assert_eq!(
            App::new().handle_key(key('u'), AMPLE, AMPLE),
            Action::Nav(Nav::HalfPage(-1))
        );
    }
    #[test]
    fn ctrl_f_page_down() {
        assert_eq!(
            App::new().handle_key(ctrl('f'), AMPLE, AMPLE),
            Action::Nav(Nav::Page(1))
        );
    }
    #[test]
    fn l_scrolls_right() {
        assert_eq!(
            App::new().handle_key(key('l'), AMPLE, AMPLE),
            Action::Nav(Nav::Horizontal(1))
        );
    }
    #[test]
    fn ctrl_l_redraws() {
        assert_eq!(
            App::new().handle_key(ctrl('l'), AMPLE, AMPLE),
            Action::Redraw
        );
    }
    #[test]
    fn esc_clears_count() {
        let mut app = App::new();
        app.handle_key(key('9'), AMPLE, AMPLE);
        assert_eq!(
            app.handle_key(
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                AMPLE,
                AMPLE
            ),
            Action::Cancel
        );
        assert_eq!(
            app.handle_key(key('j'), AMPLE, AMPLE),
            Action::Nav(Nav::Lines(1))
        );
    }
    #[test]
    fn esc_requests_cancel() {
        assert_eq!(
            App::new().handle_key(
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                AMPLE,
                AMPLE
            ),
            Action::Cancel
        );
    }
    #[test]
    fn other_keys_do_nothing() {
        assert_eq!(App::new().handle_key(key('z'), AMPLE, AMPLE), Action::None);
    }
    #[test]
    fn colon_enters_command_mode() {
        let mut app = App::new();
        assert_eq!(app.handle_key(key(':'), AMPLE, AMPLE), Action::Redraw);
        assert_eq!(app.mode, Mode::Command(CommandKind::Colon));
    }
    #[test]
    fn colon_is_a_no_op_without_a_bottom_chrome_row() {
        // chrome_rows(0) == chrome_rows(1) == 0: no row exists for the
        // command line to paint into, so `:` must not compose an
        // invisible buffer.
        let mut app = App::new();
        assert_eq!(app.handle_key(key(':'), 0, AMPLE), Action::None);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.handle_key(key(':'), 1, AMPLE), Action::None);
        assert_eq!(app.mode, Mode::Normal);
    }
    #[test]
    fn colon_enters_command_mode_at_the_smallest_terminal_with_a_bottom_row() {
        // chrome_rows(2) == 1: the smallest size with a bottom row at all.
        let mut app = App::new();
        assert_eq!(app.handle_key(key(':'), 2, AMPLE), Action::Redraw);
        assert_eq!(app.mode, Mode::Command(CommandKind::Colon));
    }
    #[test]
    fn colon_is_a_no_op_below_three_columns() {
        // a bottom row can exist (rows stays at the App::new() default)
        // while still being too narrow for typing to be meaningfully
        // visible on it: at cols=0 render_command_line's clear-then-write
        // loop is an empty range (nothing shows at all — the same
        // invisible-mode failure as the rows-only gate above, on the
        // other axis); at cols=1 or 2 something shows, but not enough for
        // `:` plus a digit plus the cursor to fit together (see
        // render_command_line's own doc comment for the exact windowing).
        let mut app = App::new();
        for cols in [0u16, 1, 2] {
            assert_eq!(
                app.handle_key(key(':'), AMPLE, cols),
                Action::None,
                "cols={cols}"
            );
            assert_eq!(app.mode, Mode::Normal, "cols={cols}");
        }
        assert_eq!(app.handle_key(key(':'), AMPLE, 3), Action::Redraw);
        assert_eq!(app.mode, Mode::Command(CommandKind::Colon));
    }
    #[test]
    fn colon_too_small_is_a_true_no_op_and_preserves_a_pending_count() {
        // unlike the keys that intentionally abort a count (the
        // catch-all, Ctrl-l, a `:` that does open Command), a key that
        // does nothing at all because the terminal cannot show its
        // effect must not disturb state the user is still composing.
        let mut app = App::new();
        app.handle_key(key('5'), 1, AMPLE);
        assert_eq!(app.handle_key(key(':'), 1, AMPLE), Action::None);
        assert_eq!(
            app.handle_key(key('j'), 1, AMPLE),
            Action::Nav(Nav::Lines(5))
        );
    }
    #[test]
    fn a_refused_colon_mid_chord_is_as_if_unpressed() {
        // the acceptance test: a refused `:` is as if unpressed, so a
        // pending `g`-chord AND the count armed for it both survive it
        // intact — the NEXT `g` completes the ORIGINAL chord (counted gg),
        // rather than the refused `:` having aborted it and started a
        // fresh one.
        let mut app = App::new();
        app.handle_key(key('5'), 1, AMPLE);
        app.handle_key(key('g'), 1, AMPLE);
        assert_eq!(app.handle_key(key(':'), 1, AMPLE), Action::None);
        assert_eq!(
            app.handle_key(key('g'), 1, AMPLE),
            Action::Nav(Nav::Line(5))
        );
    }
    #[test]
    fn colon_gate_forgets_nothing_because_it_remembers_nothing() {
        // pass 7's structural fix: handle_key takes the live size as a
        // parameter instead of reading a field it cached from some earlier
        // call, so nothing from a previous call can leak into this one's
        // decision — there is no field left for it to leak through. A
        // large size on an unrelated key first (pre-fix, the moment
        // `self.terminal_rows`/`terminal_cols` would have been cached) has
        // zero effect on a `:` that follows with its own, much smaller
        // size — this call's arguments are the only place that size can
        // come from.
        let mut app = App::new();
        app.handle_key(key('j'), 80, 80);
        assert_eq!(app.handle_key(key(':'), 1, 1), Action::None);
        assert_eq!(app.mode, Mode::Normal);
    }
    #[test]
    fn command_digits_accumulate_and_enter_jumps() {
        let mut app = App::new();
        app.handle_key(key(':'), AMPLE, AMPLE);
        app.handle_key(key('1'), AMPLE, AMPLE);
        app.handle_key(key('5'), AMPLE, AMPLE);
        app.handle_key(key('0'), AMPLE, AMPLE);
        assert_eq!(
            app.handle_key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                AMPLE,
                AMPLE
            ),
            Action::Nav(Nav::Line(150))
        );
        assert_eq!(app.mode, Mode::Normal);
    }
    #[test]
    fn command_ctrl_j_jumps_exactly_like_enter() {
        // Ctrl+J is Enter's alias here for one specific reason: a physical Enter typed before
        // Term::new()'s enable_raw_mode() call arrives while the tty is still cooked, where
        // POSIX ICRNL translates the Enter key's CR byte to LF as it's received; crossterm
        // only decodes a bare LF as Enter when raw mode is NOT active at parse time (it always
        // is, by the time ress's loop starts parsing), so that LF decodes as Ctrl+J instead
        // (see docs/architecture.md's event loop section and issue #30's investigation report
        // for the full mechanism). This test pins the App-level half of the fix; the pty-level
        // regression lives in ress/tests/smoke.rs.
        let mut app = App::new();
        app.handle_key(key(':'), AMPLE, AMPLE);
        app.handle_key(key('1'), AMPLE, AMPLE);
        app.handle_key(key('5'), AMPLE, AMPLE);
        app.handle_key(key('0'), AMPLE, AMPLE);
        assert_eq!(
            app.handle_key(ctrl('j'), AMPLE, AMPLE),
            Action::Nav(Nav::Line(150))
        );
        assert_eq!(app.mode, Mode::Normal);
    }
    #[test]
    fn command_backspace_edits_and_esc_cancels() {
        let mut app = App::new();
        app.handle_key(key(':'), AMPLE, AMPLE);
        app.handle_key(key('4'), AMPLE, AMPLE);
        app.handle_key(key('2'), AMPLE, AMPLE);
        app.handle_key(
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            AMPLE,
            AMPLE,
        );
        app.handle_key(key('7'), AMPLE, AMPLE);
        assert_eq!(app.command_buffer(), "47");
        assert_eq!(
            app.handle_key(
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                AMPLE,
                AMPLE
            ),
            Action::Redraw
        );
        assert_eq!(app.mode, Mode::Normal);
        app.handle_key(key(':'), AMPLE, AMPLE);
        assert_eq!(app.command_buffer(), "", "a new command starts empty");
    }
    #[test]
    fn command_empty_enter_is_a_cancel() {
        let mut app = App::new();
        app.handle_key(key(':'), AMPLE, AMPLE);
        assert_eq!(
            app.handle_key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                AMPLE,
                AMPLE
            ),
            Action::Redraw
        );
        assert_eq!(app.mode, Mode::Normal);
    }
    #[test]
    fn command_overflowing_number_clamps_to_the_end() {
        // capped at 20 digits, parses to MAX. Each of the first 20 grows
        // the buffer and repaints; every digit past the cap is dropped —
        // the buffer is unchanged, so it must not repaint either.
        let mut app = App::new();
        app.handle_key(key(':'), AMPLE, AMPLE);
        for _ in 0..20 {
            assert_eq!(app.handle_key(key('9'), AMPLE, AMPLE), Action::Redraw);
        }
        assert_eq!(app.command_buffer().len(), 20);
        for _ in 0..5 {
            assert_eq!(app.handle_key(key('9'), AMPLE, AMPLE), Action::None);
        }
        assert_eq!(app.command_buffer().len(), 20);
        assert_eq!(
            app.handle_key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                AMPLE,
                AMPLE
            ),
            Action::Nav(Nav::Line(u64::MAX))
        );
    }
    #[test]
    fn command_lone_zero_jumps_to_the_top_on_enter() {
        // a '0' on an empty buffer now pushes (round 1's drop-it rule is
        // refuted): ":0" no longer cancels. goto_line's vim-style clamp
        // treats line 0 the same as line 1, so this is the jump-to-top
        // spelling working again.
        let mut app = App::new();
        app.handle_key(key(':'), AMPLE, AMPLE);
        assert_eq!(app.handle_key(key('0'), AMPLE, AMPLE), Action::Redraw);
        assert_eq!(app.command_buffer(), "0");
        assert_eq!(
            app.handle_key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                AMPLE,
                AMPLE
            ),
            Action::Nav(Nav::Line(0))
        );
        assert_eq!(app.mode, Mode::Normal);
    }
    #[test]
    fn command_leading_zeros_collapse_to_one_so_0000042_jumps_to_42() {
        // the finding's exact repro: pre-round-1-fix, the 20-char cap kept
        // the FIRST 20 chars typed, so twenty '0's then "42" retained only
        // zeros and parsed to line 1, not 42. Each digit typed while the
        // buffer is exactly "0" now REPLACES that zero rather than
        // accumulating alongside it, so the buffer never grows past one
        // character while still all-zero: ":0000042" types as "42".
        let mut app = App::new();
        app.handle_key(key(':'), AMPLE, AMPLE);
        assert_eq!(app.handle_key(key('0'), AMPLE, AMPLE), Action::Redraw);
        for _ in 0..4 {
            assert_eq!(app.handle_key(key('0'), AMPLE, AMPLE), Action::None);
        }
        assert_eq!(app.command_buffer(), "0");
        assert_eq!(app.handle_key(key('4'), AMPLE, AMPLE), Action::Redraw);
        assert_eq!(app.command_buffer(), "4");
        assert_eq!(app.handle_key(key('2'), AMPLE, AMPLE), Action::Redraw);
        assert_eq!(app.command_buffer(), "42");
        assert_eq!(
            app.handle_key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                AMPLE,
                AMPLE
            ),
            Action::Nav(Nav::Line(42))
        );
    }
    #[test]
    fn command_a_run_of_zeros_longer_than_the_cap_still_collapses_to_one() {
        // the buffer never grows past "0" while still all-zero, so even a
        // run past the 20-char cap collapses harmlessly — unlike round 1's
        // drop-the-zero rule, which was only ever vulnerable to exactly
        // this shape of input.
        let mut app = App::new();
        app.handle_key(key(':'), AMPLE, AMPLE);
        for _ in 0..23 {
            app.handle_key(key('0'), AMPLE, AMPLE);
        }
        assert_eq!(app.command_buffer(), "0");
        assert_eq!(
            app.handle_key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                AMPLE,
                AMPLE
            ),
            Action::Nav(Nav::Line(0))
        );
    }
    #[test]
    fn command_zero_after_a_nonzero_digit_is_kept() {
        // the replace-on-collapse rule only applies while the buffer is
        // EXACTLY "0"; a zero once the buffer holds a significant digit is
        // a normal digit, same as any other.
        let mut app = App::new();
        app.handle_key(key(':'), AMPLE, AMPLE);
        assert_eq!(app.handle_key(key('1'), AMPLE, AMPLE), Action::Redraw);
        assert_eq!(app.handle_key(key('0'), AMPLE, AMPLE), Action::Redraw);
        assert_eq!(app.command_buffer(), "10");
        assert_eq!(
            app.handle_key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                AMPLE,
                AMPLE
            ),
            Action::Nav(Nav::Line(10))
        );
    }
    #[test]
    fn command_ignores_non_digits() {
        let mut app = App::new();
        app.handle_key(key(':'), AMPLE, AMPLE);
        assert_eq!(app.handle_key(key('x'), AMPLE, AMPLE), Action::None);
        assert_eq!(app.command_buffer(), "");
    }
    #[test]
    fn command_digit_and_backspace_repaint_the_row() {
        // regression: these returned Action::None, so the run loop's
        // no-op treatment of None left the command row frozen on screen
        // while keystrokes kept registering in the buffer underneath it.
        let mut app = App::new();
        app.handle_key(key(':'), AMPLE, AMPLE);
        assert_eq!(app.handle_key(key('1'), AMPLE, AMPLE), Action::Redraw);
        assert_eq!(
            app.handle_key(
                KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
                AMPLE,
                AMPLE
            ),
            Action::Redraw
        );
    }
    #[test]
    fn command_backspace_on_an_empty_buffer_repaints_nothing() {
        // the third member of the same None family as the self-replacing
        // lone zero and the capped 21st digit above: pop() on an empty
        // buffer removes nothing, so there is nothing on screen to
        // repaint. Two keys still exit Command from here, both of them
        // cancels rather than executions: Esc, and Enter on the
        // still-empty buffer (handle_key_command's own Enter arm). This
        // deliberately does not adopt a cancel-on-empty-backspace
        // binding; that is a keymap question, not this contract's.
        let mut app = App::new();
        app.handle_key(key(':'), AMPLE, AMPLE);
        assert_eq!(
            app.handle_key(
                KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
                AMPLE,
                AMPLE
            ),
            Action::None
        );
        assert_eq!(app.command_buffer(), "");
        assert_eq!(app.mode, Mode::Command(CommandKind::Colon));
    }
    #[test]
    fn command_ignores_ctrl_digit() {
        // Normal's digit arm requires a bare (non-ctrl) char for the same
        // reason: Ctrl+digit is not a digit.
        let mut app = App::new();
        app.handle_key(key(':'), AMPLE, AMPLE);
        assert_eq!(app.handle_key(ctrl('5'), AMPLE, AMPLE), Action::None);
        assert_eq!(app.command_buffer(), "");
    }
    #[test]
    fn command_kind_colon_still_parses_digits_only() {
        // regression: introducing CommandKind must not change Colon's own grammar one bit.
        let mut app = App::new();
        app.handle_key(key(':'), AMPLE, AMPLE);
        assert_eq!(app.mode, Mode::Command(CommandKind::Colon));
        assert_eq!(app.handle_key(key('x'), AMPLE, AMPLE), Action::None);
        assert_eq!(
            app.command_buffer(),
            "",
            "non-digit chars are still rejected in Colon mode"
        );
        app.handle_key(key('4'), AMPLE, AMPLE);
        app.handle_key(key('2'), AMPLE, AMPLE);
        assert_eq!(
            app.handle_key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                AMPLE,
                AMPLE
            ),
            Action::Nav(Nav::Line(42))
        );
    }
    // ---- search input mode: `/` `?` `n` `N` (search v1, task 6) ----
    #[test]
    fn slash_enters_search_mode_and_question_mark_enters_backward() {
        let mut app = App::new();
        assert_eq!(app.handle_key(key('/'), AMPLE, AMPLE), Action::Redraw);
        assert_eq!(
            app.mode,
            Mode::Command(CommandKind::Search { backward: false })
        );
        let mut app2 = App::new();
        assert_eq!(app2.handle_key(key('?'), AMPLE, AMPLE), Action::Redraw);
        assert_eq!(
            app2.mode,
            Mode::Command(CommandKind::Search { backward: true })
        );
    }
    #[test]
    fn search_mode_accepts_arbitrary_printable_chars_and_backspace() {
        let mut app = App::new();
        app.handle_key(key('/'), AMPLE, AMPLE);
        assert_eq!(app.handle_key(key('a'), AMPLE, AMPLE), Action::Redraw);
        assert_eq!(app.handle_key(key('.'), AMPLE, AMPLE), Action::Redraw);
        assert_eq!(app.handle_key(key('*'), AMPLE, AMPLE), Action::Redraw);
        assert_eq!(
            app.command_buffer(),
            "a.*",
            "regex metacharacters and digits are ordinary buffer chars here, unlike Colon's grammar"
        );
        assert_eq!(
            app.handle_key(
                KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
                AMPLE,
                AMPLE
            ),
            Action::Redraw
        );
        assert_eq!(app.command_buffer(), "a.");
    }
    #[test]
    fn search_mode_q_is_an_ordinary_character_not_a_quit_key() {
        // "q stays quit in Normal only": inside Search input, typing 'q' as part of a pattern
        // must push it into the buffer, not quit the pager.
        let mut app = App::new();
        app.handle_key(key('/'), AMPLE, AMPLE);
        assert_eq!(app.handle_key(key('q'), AMPLE, AMPLE), Action::Redraw);
        assert_eq!(app.command_buffer(), "q");
        assert_eq!(
            app.mode,
            Mode::Command(CommandKind::Search { backward: false }),
            "must still be composing the search, not quit"
        );
    }
    #[test]
    fn search_mode_backspace_on_an_empty_buffer_repaints_nothing() {
        // the same None-family member Colon's own backspace-on-empty has.
        let mut app = App::new();
        app.handle_key(key('/'), AMPLE, AMPLE);
        assert_eq!(
            app.handle_key(
                KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
                AMPLE,
                AMPLE
            ),
            Action::None
        );
        assert_eq!(app.command_buffer(), "");
        assert_eq!(
            app.mode,
            Mode::Command(CommandKind::Search { backward: false })
        );
    }
    #[test]
    fn search_mode_rejects_control_chars() {
        // defensive: a real terminal never delivers a control-char `c` through KeyCode::Char at
        // all (raw-mode decoding maps those bytes to their own dedicated KeyCodes, or to
        // Char(<letter>) + CONTROL) -- this pins the guard against synthetic/pasted input anyway.
        let mut app = App::new();
        app.handle_key(key('/'), AMPLE, AMPLE);
        let bel = KeyEvent::new(KeyCode::Char('\u{7}'), KeyModifiers::NONE);
        assert_eq!(app.handle_key(bel, AMPLE, AMPLE), Action::None);
        assert_eq!(app.command_buffer(), "");
    }
    #[test]
    fn search_mode_ctrl_j_commits_like_enter_not_a_stray_j() {
        // issue #30's ICRNL-then-raw-mode-misparse mechanism is mode-agnostic (see Colon's own
        // alias arm's doc comment): a `/pattern<CR>` typed before first paint gets its Enter
        // delivered as Ctrl+J exactly like a `:N<CR>` would. Worse than Colon's merely-stuck-
        // buffer symptom, an unguarded push arm here would silently splice a stray 'j' into the
        // pattern with no visible sign anything went wrong -- Ctrl+J must commit, like Enter.
        let mut app = App::new();
        app.handle_key(key('/'), AMPLE, AMPLE);
        app.handle_key(key('a'), AMPLE, AMPLE);
        app.handle_key(key('b'), AMPLE, AMPLE);
        assert_eq!(
            app.handle_key(ctrl('j'), AMPLE, AMPLE),
            Action::CommitSearch {
                raw: "ab".to_string(),
                backward: false,
            }
        );
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.command_buffer(), "", "the buffer resets once committed");
    }
    #[test]
    fn search_mode_other_ctrl_chords_do_not_touch_the_buffer() {
        // a general guard, not a Ctrl+J special case: NO ctrl-modified char is ever pattern
        // text, mirroring Colon's own digit arm's ctrl guard ("Ctrl+digit is not a digit").
        let mut app = App::new();
        app.handle_key(key('/'), AMPLE, AMPLE);
        app.handle_key(key('a'), AMPLE, AMPLE);
        assert_eq!(app.handle_key(ctrl('a'), AMPLE, AMPLE), Action::None);
        assert_eq!(
            app.command_buffer(),
            "a",
            "unchanged -- Ctrl+a must not push 'a' into the pattern"
        );
        assert_eq!(
            app.mode,
            Mode::Command(CommandKind::Search { backward: false }),
            "still composing, not committed or cancelled"
        );
    }
    #[test]
    fn search_mode_esc_cancels_clearing_the_buffer() {
        let mut app = App::new();
        app.handle_key(key('/'), AMPLE, AMPLE);
        app.handle_key(key('a'), AMPLE, AMPLE);
        assert_eq!(
            app.handle_key(
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                AMPLE,
                AMPLE
            ),
            Action::Redraw
        );
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.command_buffer(), "");
    }
    #[test]
    fn search_enter_with_empty_buffer_just_closes() {
        // mirrors Colon's own empty-Enter cancel: no Action::CommitSearch for a search nobody
        // actually typed.
        let mut app = App::new();
        app.handle_key(key('/'), AMPLE, AMPLE);
        assert_eq!(
            app.handle_key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                AMPLE,
                AMPLE
            ),
            Action::Redraw
        );
        assert_eq!(app.mode, Mode::Normal);
    }
    #[test]
    fn search_enter_with_text_commits() {
        let mut app = App::new();
        app.handle_key(key('?'), AMPLE, AMPLE);
        app.handle_key(key('a'), AMPLE, AMPLE);
        app.handle_key(key('b'), AMPLE, AMPLE);
        assert_eq!(
            app.handle_key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                AMPLE,
                AMPLE
            ),
            Action::CommitSearch {
                raw: "ab".to_string(),
                backward: true,
            }
        );
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.command_buffer(), "", "the buffer resets once committed");
    }
    #[test]
    fn search_mode_respects_the_same_size_floor_as_colon() {
        // chrome-gate parity: `/`/`?` share `:`'s exact cols >= 3 floor, refused identically
        // below it and entering identically at it.
        let mut app = App::new();
        assert_eq!(app.handle_key(key('/'), AMPLE, 2), Action::None);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.handle_key(key('?'), AMPLE, 2), Action::None);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.handle_key(key('/'), AMPLE, 3), Action::Redraw);
        assert_eq!(
            app.mode,
            Mode::Command(CommandKind::Search { backward: false })
        );
        // `?` gets the identical floor treatment -- pinned on its own fresh App (the shared
        // `app` above already left Normal mode) so the "parity" this test's name promises
        // covers the SUCCEEDS-at-3 half too, not just the refused-at-2 half already above.
        let mut app2 = App::new();
        assert_eq!(app2.handle_key(key('?'), AMPLE, 2), Action::None);
        assert_eq!(app2.mode, Mode::Normal);
        assert_eq!(app2.handle_key(key('?'), AMPLE, 3), Action::Redraw);
        assert_eq!(
            app2.mode,
            Mode::Command(CommandKind::Search { backward: true })
        );
    }
    #[test]
    fn any_key_clears_the_notice_first() {
        let mut app = App::new();
        app.notice = Some("pattern not found".to_string());
        assert_eq!(
            app.handle_key(key('j'), AMPLE, AMPLE),
            Action::Nav(Nav::Lines(1)),
            "the key that clears the notice still acts normally"
        );
        assert_eq!(app.notice, None);
    }
    #[test]
    fn n_maps_to_search_next_and_capital_n_maps_to_search_prev() {
        // handle_key's own mapping is search-blind: it returns the Nav variant regardless of
        // whether `app.search` is set at all -- that check, and the direction algebra, are both
        // apply-time concerns (see `search_direction_algebra_repeats_for_n_and_flips_for_capital_n`
        // and `n_and_n_without_a_search_are_notice_no_ops`, below).
        assert_eq!(
            App::new().handle_key(key('n'), AMPLE, AMPLE),
            Action::Nav(Nav::SearchNext)
        );
        assert_eq!(
            App::new().handle_key(key('N'), AMPLE, AMPLE),
            Action::Nav(Nav::SearchPrev)
        );
    }
    #[test]
    fn n_consumes_a_pending_count_like_other_non_count_motions() {
        let mut app = App::new();
        app.handle_key(key('5'), AMPLE, AMPLE);
        assert_eq!(
            app.handle_key(key('n'), AMPLE, AMPLE),
            Action::Nav(Nav::SearchNext)
        );
        assert_eq!(
            app.handle_key(key('j'), AMPLE, AMPLE),
            Action::Nav(Nav::Lines(1)),
            "the count must not have survived onto the next motion"
        );
    }
    #[test]
    fn search_direction_algebra_repeats_for_n_and_flips_for_capital_n() {
        // vim's algebra: `n` repeats the ORIGINAL search's own direction; `N` reverses it --
        // pinned directly against the pure formula, both pattern directions, independent of
        // Document/apply_nav.
        // pattern from `/` (backward=false): n stays forward, N flips backward.
        assert!(effective_search_forward(false, Nav::SearchNext));
        assert!(!effective_search_forward(false, Nav::SearchPrev));
        // pattern from `?` (backward=true): n stays backward, N flips forward.
        assert!(!effective_search_forward(true, Nav::SearchNext));
        assert!(effective_search_forward(true, Nav::SearchPrev));
    }
    #[test]
    fn last_notice_line_extracts_the_reason_not_the_generic_header() {
        // batch 3 (2026-07-23), finding #13b: regex's own pretty multi-line parse errors put
        // the actual reason on their LAST line, not the first ("regex parse error:", useless on
        // its own) -- pinned against three real error shapes, the third exercising #8's own
        // unicode(false) ruling directly (a `\p{L}` class needs an explicit `(?u)` opt-in or
        // fails to compile at all under it).
        // `.err().unwrap()`, not `.unwrap_err()`: `SearchPattern` (the `Ok` type) has no `Debug`
        // impl, which `Result::unwrap_err` needs (to print it if the result were actually
        // `Ok`) but `Option::unwrap` (after `.err()` converts away the `Ok` side entirely) does
        // not.
        let unclosed_group = SearchPattern::compile("(", false)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(last_notice_line(&unclosed_group), "error: unclosed group");
        let unclosed_repetition = SearchPattern::compile("a{", false)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(
            last_notice_line(&unclosed_repetition),
            "error: unclosed counted repetition"
        );
        let unicode_class = SearchPattern::compile(r"\p{L}", false)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(
            last_notice_line(&unicode_class),
            "error: Unicode not allowed here"
        );
    }
    #[tokio::test]
    async fn n_and_n_without_a_search_are_notice_no_ops() {
        let doc = nav_doc();
        let mut app = App::new();
        let mut pending = None;
        apply_nav(&doc, &mut app, &mut pending, Nav::SearchNext, 10)
            .await
            .unwrap();
        assert_eq!(app.notice.as_deref(), Some("no previous search"));
        assert_eq!(
            app.top,
            Anchor::TOP,
            "no search means no navigation happens"
        );
        app.notice = None;
        apply_nav(&doc, &mut app, &mut pending, Nav::SearchPrev, 10)
            .await
            .unwrap();
        assert_eq!(app.notice.as_deref(), Some("no previous search"));
        assert_eq!(app.top, Anchor::TOP);
    }
    #[tokio::test]
    async fn apply_nav_search_next_with_no_prior_match_starts_from_the_current_top() {
        // no `current` yet -> forward origin is app.top.offset(), per apply_nav's own doc
        // comment -- the common case, the very first jump after committing a search.
        let data = b"xxx\nneedle\n".to_vec();
        let src = std::sync::Arc::new(ress_core::source::MockSource::new(data));
        let doc = Document::new(src, ress_core::Config::default());
        let mut app = App::new();
        let pattern = std::sync::Arc::new(SearchPattern::compile("needle", false).unwrap());
        app.search = Some(ActiveSearch {
            pattern,
            generation: 1,
            current: None,
        });
        let mut pending = None;
        apply_nav(&doc, &mut app, &mut pending, Nav::SearchNext, 10)
            .await
            .unwrap();
        assert_eq!(app.top.offset(), 4, "the line start of the only match");
        assert_eq!(
            app.search.as_ref().unwrap().current,
            Some(4),
            "current updates to the byte offset of the match just found"
        );
    }
    #[tokio::test]
    async fn apply_nav_search_next_advances_past_the_current_match_without_a_self_hit() {
        // forward origin = current + 1 (INCLUSIVE search_next origin): parked exactly on the
        // first "needle" (byte 0), n must land on the SECOND one (byte 7), not re-find the first.
        let data = b"needle\nneedle\n".to_vec();
        let src = std::sync::Arc::new(ress_core::source::MockSource::new(data));
        let doc = Document::new(src, ress_core::Config::default());
        let mut app = App::new();
        let pattern = std::sync::Arc::new(SearchPattern::compile("needle", false).unwrap());
        app.search = Some(ActiveSearch {
            pattern,
            generation: 1,
            current: Some(0),
        });
        let mut pending = None;
        apply_nav(&doc, &mut app, &mut pending, Nav::SearchNext, 10)
            .await
            .unwrap();
        assert_eq!(app.search.as_ref().unwrap().current, Some(7));
        assert_eq!(app.top.offset(), 7);
    }
    #[tokio::test]
    async fn apply_nav_search_prev_after_a_backward_pattern_finds_the_earlier_match_without_a_self_hit()
     {
        // mirrors document.rs's own search_next_backward_self_hit_finds_the_earlier_match_instead
        // fixture and origin (11) exactly: backward origin = current UNADJUSTED (no `- 1`) --
        // search_next's own EXCLUSIVE `hi` bound is what skips the self-hit, not arithmetic here.
        // `?` (pattern.backward = true) + Nav::SearchNext ("n") = effective backward, per the
        // algebra (n repeats the original search's own direction).
        let data = b"needle\nxxx\nneedle\n".to_vec();
        let src = std::sync::Arc::new(ress_core::source::MockSource::new(data));
        let doc = Document::new(src, ress_core::Config::default());
        let mut app = App::new();
        let pattern = std::sync::Arc::new(SearchPattern::compile("needle", true).unwrap());
        app.search = Some(ActiveSearch {
            pattern,
            generation: 1,
            current: Some(11),
        });
        let mut pending = None;
        apply_nav(&doc, &mut app, &mut pending, Nav::SearchNext, 10)
            .await
            .unwrap();
        assert_eq!(
            app.search.as_ref().unwrap().current,
            Some(0),
            "must skip the match AT current (11), landing on the earlier one"
        );
        assert_eq!(app.top.offset(), 0);
    }
    #[tokio::test]
    async fn apply_outcome_found_match_also_updates_the_active_search_current() {
        // apply_outcome is the shared consumer both apply_nav and the run loop's pending-
        // completion arm call -- this pins its OWN mutation of `search.current` directly,
        // independent of either caller.
        let doc = nav_doc();
        let anchor = some_anchor(&doc).await;
        let mut app = App::new();
        let pattern = std::sync::Arc::new(SearchPattern::compile("x", false).unwrap());
        app.search = Some(ActiveSearch {
            pattern,
            generation: 3,
            current: Some(1),
        });
        apply_outcome(
            &mut app,
            NavOutcome::FoundMatch {
                top: anchor,
                match_at: 99,
                wrapped: false,
            },
        );
        assert_eq!(app.search.as_ref().unwrap().current, Some(99));
        assert_eq!(
            app.search.as_ref().unwrap().generation,
            3,
            "generation is untouched by an outcome"
        );
    }
    #[test]
    fn f1_enters_help_and_any_key_leaves() {
        let mut app = App::new();
        assert_eq!(
            app.handle_key(
                KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE),
                AMPLE,
                AMPLE
            ),
            Action::Redraw
        );
        assert_eq!(app.mode, Mode::Help);
        assert_eq!(app.handle_key(key('j'), AMPLE, AMPLE), Action::Redraw);
        assert_eq!(app.mode, Mode::Normal);
    }
    #[test]
    fn f1_is_a_no_op_when_the_overlay_has_no_room_to_render() {
        // render_help_overlay draws an empty PANEL — a bordered, titled
        // box with no keymap line inside it — well before its INTERIOR
        // clamp reaches zero (see help_overlay_fits' doc comment): the
        // panel alone needs content_rows(rows) >= 3, but the interior
        // (one more "subtract 2, floor at 0" layer, for the border) needs
        // content_rows(rows) >= 5. rows=3,4 fail even the panel's own
        // bound (content_rows 2,2); rows=5,6 clear the panel's bound
        // (content_rows 3,4) but still leave a zero-row interior — a
        // no-op is required at all four, or F1 opens a useless Help that
        // still swallows the next key to close it.
        let mut app = App::new();
        for rows in [3u16, 4, 5, 6] {
            assert_eq!(
                app.handle_key(
                    KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE),
                    rows,
                    AMPLE
                ),
                Action::None,
                "rows={rows}"
            );
            assert_eq!(app.mode, Mode::Normal, "rows={rows}");
        }
    }
    #[test]
    fn f1_enters_help_at_the_smallest_terminal_that_can_show_it() {
        // content_rows(7) == 5 (chrome_rows(7) == 2, 7 - 2 == 5): the
        // first size where the overlay's INTERIOR — not just its panel —
        // is nonzero (content_rows(6) == 4 still leaves a zero-row
        // interior; see f1_is_a_no_op_when_the_overlay_has_no_room_to_render
        // and help_overlay_fits' doc comment).
        let mut app = App::new();
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE), 7, AMPLE),
            Action::Redraw
        );
        assert_eq!(app.mode, Mode::Help);
    }
    #[test]
    fn f1_entry_gate_reads_rows_and_cols_without_swapping_them() {
        // every other test pins the floor on ONE axis while padding the
        // other with AMPLE — which cannot catch rows and cols being
        // swapped somewhere in the plumbing, since AMPLE clears either
        // threshold regardless of which one it lands on. Help's floor
        // (rows >= 7, cols >= 5) is asymmetric on purpose here: both
        // arguments sit at their OWN real minimum, at once, so a swap
        // changes the outcome instead of hiding behind a value large
        // enough to pass either check.
        let mut app = App::new();
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE), 6, 5),
            Action::None,
            "rows one below its own floor must refuse even though cols alone clears its floor"
        );
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE), 7, 4),
            Action::None,
            "cols one below its own floor must refuse even though rows alone clears its floor"
        );
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE), 7, 5),
            Action::Redraw,
            "both floors cleared at once, at their own true (different) minimums"
        );
        assert_eq!(app.mode, Mode::Help);
    }
    #[test]
    fn f1_too_small_is_a_true_no_op_and_preserves_a_pending_count() {
        // a true no-op: unlike a fired F1 (which does abort a pending
        // count — vim-first ruling, see the F1 arm), one that does
        // nothing at all because the terminal cannot show help must not
        // disturb state the user is still composing.
        let mut app = App::new();
        app.handle_key(key('5'), 3, AMPLE);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE), 3, AMPLE),
            Action::None
        );
        assert_eq!(
            app.handle_key(key('j'), 3, AMPLE),
            Action::Nav(Nav::Lines(5))
        );
    }
    #[test]
    fn a_refused_f1_mid_chord_is_as_if_unpressed() {
        // same acceptance test as the `:` version above, refused on rows.
        let mut app = App::new();
        app.handle_key(key('5'), 3, AMPLE);
        app.handle_key(key('g'), 3, AMPLE);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE), 3, AMPLE),
            Action::None
        );
        assert_eq!(
            app.handle_key(key('g'), 3, AMPLE),
            Action::Nav(Nav::Line(5))
        );
    }
    #[test]
    fn a_refused_f1_mid_chord_due_to_width_is_as_if_unpressed() {
        // same, refused on cols instead of rows — proves the `refused`
        // check is bound-agnostic, not accidentally rows-specific.
        let mut app = App::new();
        app.handle_key(key('5'), AMPLE, 2);
        app.handle_key(key('g'), AMPLE, 2);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE), AMPLE, 2),
            Action::None
        );
        assert_eq!(
            app.handle_key(key('g'), AMPLE, 2),
            Action::Nav(Nav::Line(5))
        );
    }
    #[test]
    fn help_mode_swallows_quit_keys_back_to_normal() {
        // q in help closes help, it must not quit the pager.
        let mut app = App::new();
        app.handle_key(
            KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE),
            AMPLE,
            AMPLE,
        );
        assert_eq!(app.handle_key(key('q'), AMPLE, AMPLE), Action::Redraw);
        assert_eq!(app.mode, Mode::Normal);
    }
    #[test]
    fn an_unmapped_key_aborts_the_count() {
        // vim aborts a pending count on unmapped input; 5z then j must be
        // a single-line motion.
        let mut app = App::new();
        app.handle_key(key('5'), AMPLE, AMPLE);
        app.handle_key(key('z'), AMPLE, AMPLE);
        assert_eq!(
            app.handle_key(key('j'), AMPLE, AMPLE),
            Action::Nav(Nav::Lines(1))
        );
    }
    #[test]
    fn ctrl_l_aborts_the_count() {
        let mut app = App::new();
        app.handle_key(key('5'), AMPLE, AMPLE);
        assert_eq!(app.handle_key(ctrl('l'), AMPLE, AMPLE), Action::Redraw);
        assert_eq!(
            app.handle_key(key('j'), AMPLE, AMPLE),
            Action::Nav(Nav::Lines(1))
        );
    }
    #[test]
    fn entering_command_mode_aborts_the_count() {
        // 5: means "start a command", not "command with 5 armed".
        let mut app = App::new();
        app.handle_key(key('5'), AMPLE, AMPLE);
        app.handle_key(key(':'), AMPLE, AMPLE);
        app.handle_key(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            AMPLE,
            AMPLE,
        );
        assert_eq!(
            app.handle_key(key('j'), AMPLE, AMPLE),
            Action::Nav(Nav::Lines(1))
        );
    }
    #[test]
    fn entering_help_aborts_the_count() {
        // 5 then F1: help neither builds nor consumes the count, so it
        // must abort like any other non-consuming key.
        let mut app = App::new();
        app.handle_key(key('5'), AMPLE, AMPLE);
        app.handle_key(
            KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE),
            AMPLE,
            AMPLE,
        );
        app.handle_key(key('j'), AMPLE, AMPLE); // closes help
        assert_eq!(
            app.handle_key(key('j'), AMPLE, AMPLE),
            Action::Nav(Nav::Lines(1))
        );
    }
    #[test]
    fn entering_command_mode_clears_a_pending_g_chord() {
        // g then : then Esc then gg: a stale chord must not survive into
        // (or past) Command mode — the second gg here must be a fresh
        // chord reaching Top, not a lone second `g` finishing a stale one.
        let mut app = App::new();
        app.handle_key(key('g'), AMPLE, AMPLE);
        app.handle_key(key(':'), AMPLE, AMPLE);
        app.handle_key(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            AMPLE,
            AMPLE,
        );
        assert_eq!(app.handle_key(key('g'), AMPLE, AMPLE), Action::None);
        assert_eq!(
            app.handle_key(key('g'), AMPLE, AMPLE),
            Action::Nav(Nav::Top)
        );
    }
    #[tokio::test]
    async fn apply_nav_page_advances_full_screen() {
        let doc = nav_doc();
        let mut app = App::new();
        let mut pending = None;
        apply_nav(&doc, &mut app, &mut pending, Nav::Page(1), 10)
            .await
            .unwrap();
        assert_eq!(app.top.offset(), 20);
    }
    #[tokio::test]
    async fn apply_nav_half_page_advances_half_screen() {
        let doc = nav_doc();
        let mut app = App::new();
        let mut pending = None;
        apply_nav(&doc, &mut app, &mut pending, Nav::HalfPage(1), 10)
            .await
            .unwrap();
        assert_eq!(app.top.offset(), 10);
    }
    #[tokio::test]
    async fn apply_nav_bottom_puts_last_line_on_bottom_row() {
        let doc = nav_doc();
        let mut app = App::new();
        let mut pending = None;
        apply_nav(&doc, &mut app, &mut pending, Nav::Bottom, 10)
            .await
            .unwrap();
        assert_eq!(app.top.offset(), 20);
    }
    #[tokio::test]
    async fn apply_nav_line_jumps_to_the_line_start() {
        // a cold index always pends (the scanner task has not been polled
        // when the first coverage check runs), so warm it to done first: a
        // past-the-end jump can only resolve once total_lines is known.
        let doc = nav_doc();
        let mut app = App::new();
        let mut pending = None;
        apply_nav(&doc, &mut app, &mut pending, Nav::Line(9999), 10)
            .await
            .unwrap();
        let mut active = pending.take().expect("a cold-index jump must pend");
        let _ = (&mut active.p.handle).await;
        drop(active);
        apply_nav(&doc, &mut app, &mut pending, Nav::Line(3), 10)
            .await
            .unwrap();
        assert!(
            pending.is_none(),
            "a warmed index must resolve synchronously"
        );
        assert_eq!(app.top.offset(), 4, "line 3 of two-byte lines");
    }
    #[tokio::test]
    async fn apply_nav_horizontal_clamps_at_zero() {
        let doc = nav_doc();
        let mut app = App::new();
        let mut pending = None;
        apply_nav(&doc, &mut app, &mut pending, Nav::Horizontal(5), 10)
            .await
            .unwrap();
        assert_eq!(app.hscroll.columns(), 5);
        apply_nav(&doc, &mut app, &mut pending, Nav::Horizontal(-99), 10)
            .await
            .unwrap();
        assert_eq!(app.hscroll.columns(), 0);
    }
    #[tokio::test]
    async fn apply_nav_horizontal_huge_count_saturates() {
        // two max-count scrolls must saturate at the cap, not overflow (debug) or
        // wrap to 0 (release); a max-count scroll back down still clamps at zero.
        let doc = nav_doc();
        let mut app = App::new();
        let mut pending = None;
        apply_nav(&doc, &mut app, &mut pending, Nav::Horizontal(i64::MAX), 10)
            .await
            .unwrap();
        apply_nav(&doc, &mut app, &mut pending, Nav::Horizontal(i64::MAX), 10)
            .await
            .unwrap();
        assert_eq!(app.hscroll.columns(), MAX_HSCROLL);
        apply_nav(&doc, &mut app, &mut pending, Nav::Horizontal(i64::MIN), 10)
            .await
            .unwrap();
        assert_eq!(app.hscroll.columns(), 0);
    }
    /// A real, non-TOP anchor via actual navigation. `Anchor`'s constructor
    /// is crate-private to ress-core by design (only `Document` navigation
    /// or `Anchor::TOP` can produce one — see its own doc comment), so these
    /// tests get a genuine anchor the same way production code does: nav_doc's
    /// tiny fixture makes scroll_lines resolve synchronously, well within the
    /// default 8 MiB nav budget, so the `else` branch below is unreachable in
    /// practice and exists only as a diagnostic guard.
    async fn some_anchor(doc: &Document) -> Anchor {
        let Resolution::Ready(NavOutcome::At(a)) = doc.scroll_lines(Anchor::TOP, 1).await.unwrap()
        else {
            panic!("nav_doc's tiny fixture always resolves synchronously to At");
        };
        a
    }
    // `apply_outcome` is the shared consumer both `apply_nav` (interactive)
    // and the run loop's pending-completion arm call — these pin its own
    // mutation logic directly, independent of either caller, so a future
    // divergence between the two call sites would show up here first.
    /// A minimal `ActiveSearch` for seeding `app.search` in the `apply_outcome` tests below --
    /// pattern content is irrelevant to `apply_outcome` itself (it never inspects it), only
    /// `current`'s own before/after value matters.
    fn some_search(current: Option<u64>) -> ActiveSearch {
        ActiveSearch {
            pattern: std::sync::Arc::new(SearchPattern::compile("x", false).unwrap()),
            generation: 1,
            current,
        }
    }
    #[tokio::test]
    async fn apply_outcome_at_sets_the_anchor_and_touches_nothing_else() {
        let doc = nav_doc();
        let anchor = some_anchor(&doc).await;
        let mut app = App::new();
        app.search = Some(some_search(Some(5)));
        apply_outcome(&mut app, NavOutcome::At(anchor));
        assert_eq!(app.top, anchor);
        assert_eq!(app.notice, None);
        assert_eq!(
            app.search.as_ref().unwrap().current,
            Some(5),
            "At does not touch an active search's own current match"
        );
    }
    #[tokio::test]
    async fn apply_outcome_found_match_sets_top_and_search_current_without_a_notice() {
        let doc = nav_doc();
        let anchor = some_anchor(&doc).await;
        let mut app = App::new();
        app.search = Some(some_search(None));
        apply_outcome(
            &mut app,
            NavOutcome::FoundMatch {
                top: anchor,
                match_at: 7,
                wrapped: false,
            },
        );
        assert_eq!(app.top, anchor);
        assert_eq!(app.search.as_ref().unwrap().current, Some(7));
        assert_eq!(
            app.notice, None,
            "an unwrapped jump must not raise a notice"
        );
    }
    #[tokio::test]
    async fn apply_outcome_found_match_wrapped_also_sets_the_wrap_notice() {
        let doc = nav_doc();
        let anchor = some_anchor(&doc).await;
        let mut app = App::new();
        app.search = Some(some_search(None));
        apply_outcome(
            &mut app,
            NavOutcome::FoundMatch {
                top: anchor,
                match_at: 9,
                wrapped: true,
            },
        );
        assert_eq!(app.top, anchor);
        assert_eq!(app.search.as_ref().unwrap().current, Some(9));
        assert_eq!(app.notice.as_deref(), Some("search wrapped"));
    }
    #[tokio::test]
    async fn apply_outcome_exhausted_sets_the_notice_and_leaves_the_anchor() {
        let doc = nav_doc();
        let anchor = some_anchor(&doc).await;
        let mut app = App::new();
        app.top = anchor;
        app.search = Some(some_search(Some(3)));
        apply_outcome(&mut app, NavOutcome::Exhausted);
        assert_eq!(app.top, anchor, "the anchor never moves on Exhausted");
        assert_eq!(app.notice.as_deref(), Some("pattern not found"));
        assert_eq!(
            app.search.as_ref().unwrap().current,
            Some(3),
            "Exhausted does not touch an active search's own current match"
        );
    }
    #[tokio::test]
    async fn new_motion_supersedes_a_pending_operation() {
        let mut data = Vec::new();
        for _ in 0..100 {
            data.extend_from_slice(b"y\n");
        }
        data.extend_from_slice(&[b'x'; 8192]);
        let src = std::sync::Arc::new(ress_core::source::MockSource::new(data));
        let doc = Document::new(
            src,
            ress_core::Config {
                block_size: 64,
                nav_scan_budget: 256,
                prefetch_depth: 0,
                ..ress_core::Config::default()
            },
        );
        let mut app = App::new();
        let mut pending = None;
        apply_nav(&doc, &mut app, &mut pending, Nav::Percent(90), 10)
            .await
            .unwrap();
        assert!(pending.is_some(), "expected the percent jump to pend");
        assert!(
            matches!(
                pending,
                Some(ActiveNav {
                    nav: Nav::Percent(90),
                    ..
                })
            ),
            "slot must carry the originating intent"
        );
        assert_eq!(app.top, Anchor::TOP, "anchor must not move while pending");
        apply_nav(&doc, &mut app, &mut pending, Nav::Lines(1), 10)
            .await
            .unwrap();
        assert!(
            pending.is_none(),
            "new motion must supersede the pending op"
        );
        assert_eq!(app.top.offset(), 2);
    }
    fn sparse_doc() -> Document {
        // 100 short lines then a giant unterminated tail; tiny budget so
        // row-dependent jumps pend.
        let mut data = Vec::new();
        for _ in 0..100 {
            data.extend_from_slice(b"y\n");
        }
        data.extend_from_slice(&[b'x'; 8192]);
        let src = std::sync::Arc::new(ress_core::source::MockSource::new(data));
        Document::new(
            src,
            ress_core::Config {
                block_size: 64,
                nav_scan_budget: 256,
                prefetch_depth: 0,
                ..ress_core::Config::default()
            },
        )
    }
    #[tokio::test]
    async fn handle_resize_restarts_the_pending_intent_against_new_rows() {
        // Bottom captured rows=10 when it pended; a resize to 3 rows must
        // cancel and re-dispatch the SAME intent so the answer reflects the
        // new height (goto_end(3) here: tail start 200, two lines up = 196).
        let doc = sparse_doc();
        let mut app = App::new();
        let mut pending = None;
        apply_nav(&doc, &mut app, &mut pending, Nav::Bottom, 10)
            .await
            .unwrap();
        assert!(matches!(
            pending,
            Some(ActiveNav {
                nav: Nav::Bottom,
                ..
            })
        ));
        handle_resize(&doc, &mut app, &mut pending, 3)
            .await
            .unwrap();
        let mut active = pending.take().expect("resize must re-pend the same intent");
        assert!(matches!(active.nav, Nav::Bottom));
        let NavOutcome::At(a) = (&mut active.p.handle).await.unwrap().unwrap() else {
            panic!("plain navigation always resolves At");
        };
        assert_eq!(a.offset(), 196);
    }
    #[tokio::test]
    async fn line_jump_beyond_the_frontier_pends_with_the_jump_label() {
        let mut data = Vec::new();
        for i in 0..4000u32 {
            data.extend_from_slice(format!("line {i:04}\n").as_bytes());
        }
        // no injected latency: nav_scan_budget (256 bytes) is what forces Pending here,
        // deterministically -- a byte budget, not a timing race against the background scan
        // (U-delete; verified against resolve.rs/document.rs's own scan-budget plumbing before
        // dropping this test's own with_latency call).
        let src = std::sync::Arc::new(ress_core::source::MockSource::new(data));
        let doc = Document::new(
            src,
            ress_core::Config {
                block_size: 256,
                nav_scan_budget: 256,
                prefetch_depth: 0,
                ..ress_core::Config::default()
            },
        );
        let mut app = App::new();
        let mut pending = None;
        apply_nav(&doc, &mut app, &mut pending, Nav::Line(3500), 10)
            .await
            .unwrap();
        let mut active = pending.take().expect("a jump past the frontier must pend");
        assert!(matches!(active.nav, Nav::Line(3500)));
        assert_eq!(active.p.label, "jumping to line");
        let NavOutcome::At(a) = (&mut active.p.handle).await.unwrap().unwrap() else {
            panic!("plain navigation always resolves At");
        };
        assert_eq!(a.offset(), 34990);
    }
    // `ress_core::source::wait_for_count` is `pub(crate)` to that crate, invisible from here --
    // this is its own minimal, local equivalent (U-delete), same shape: a bounded diagnostic
    // ceiling around the predicate wait, not a coordination window.
    async fn wait_for_count(
        rx: &mut tokio::sync::watch::Receiver<u64>,
        pred: impl Fn(u64) -> bool,
    ) {
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.wait_for(|&n| pred(n)))
            .await
            .expect("the watched event count never satisfied the expected condition within 5s")
            .expect("the MockSource sender lives alongside its receivers");
    }
    #[tokio::test]
    async fn dropping_the_pending_slot_aborts_the_scan() {
        // the run loop's error-`?` paths rely on drop, not explicit cancel;
        // once the task is aborted its progress sender is destroyed.
        let mut data = Vec::new();
        for _ in 0..100 {
            data.extend_from_slice(b"y\n");
        }
        data.extend_from_slice(&[b'x'; 8192]);
        let src = std::sync::Arc::new(ress_core::source::MockSource::new(data).with_gate());
        let doc = Document::new(
            src.clone(),
            ress_core::Config {
                block_size: 64,
                nav_scan_budget: 256,
                prefetch_depth: 0,
                ..ress_core::Config::default()
            },
        );
        let mut app = App::new();
        let mut pending = None;
        apply_nav(&doc, &mut app, &mut pending, Nav::Percent(90), 10)
            .await
            .unwrap();
        let active = pending.take().expect("expected the percent jump to pend");
        let mut rx = active.p.progress.clone();
        // armed only now, after the interactive attempt (bounded by nav_scan_budget) has
        // already consumed whatever reads it legitimately needed -- the pending nav's own
        // background walk, continuing from there, hits this on its very next read regardless
        // of whether it has been polled even once yet (U-delete; the gate replaces a fixed
        // 2ms-per-read latency, which could complete and free itself on its own timeline,
        // independent of when this test gets around to observing it).
        src.arm_gate();
        let mut started = src.started_events();
        let started_baseline = *started.borrow();
        wait_for_count(&mut started, |n| n > started_baseline).await;
        let mut cancelled = src.cancelled_events();
        let cancelled_baseline = *cancelled.borrow();
        drop(active);
        // POSITIVE proof: the in-flight read's own future was torn down, not left running or
        // let finish on its own.
        wait_for_count(&mut cancelled, |n| n > cancelled_baseline).await;
        // the direct discriminator this test's own name claims: the progress channel closing
        // proves the background task's own sender -- and by extension the task itself -- is
        // gone, not merely that the one read it was parked on was.
        assert!(
            rx.changed().await.is_err(),
            "the pending nav's progress sender must be gone once its task is aborted"
        );
    }
}
