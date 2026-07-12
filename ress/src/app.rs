//! App state and the event loop. `handle_key` is the pure, testable decision;
//! `run` is the thin async driver over crossterm events.
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEventKind};
use futures::StreamExt;
use ress_core::document::{Anchor, Document, HScroll};
use ress_core::resolve::{PendingNav, Resolution};
/// What the loop should do after handling an input event.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Quit,
    Nav(Nav),
    Redraw,
    Cancel,
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
/// Pager state: the byte anchor of the top line, the horizontal column offset,
/// and the in-progress count prefix / `g` chord.
pub struct App {
    pub top: Anchor,
    pub hscroll: HScroll,
    count: Option<usize>,
    pending_g: bool,
}
impl App {
    pub fn new() -> Self {
        Self {
            top: Anchor::TOP,
            hscroll: HScroll::ZERO,
            count: None,
            pending_g: false,
        }
    }
    fn take_count(&mut self) -> usize {
        self.count.take().unwrap_or(1)
    }
    fn take_count_i64(&mut self) -> i64 {
        self.take_count().min(i64::MAX as usize) as i64
    }
    /// Decides what a key press means, consuming any pending count/`g` state.
    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
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
            (KeyCode::Char('l'), true) => Action::Redraw,
            (KeyCode::Esc, _) => {
                self.count = None;
                self.pending_g = false;
                Action::Cancel
            }
            _ => Action::None,
        }
    }
}
/// Draws the current screen: compute the viewport for the terminal size, blit
/// it, and overlay the transient progress row on the bottom line while a scan
/// is pending. Viewport I/O happens before `draw` because the draw closure is
/// sync.
async fn draw(
    term: &mut crate::terminal::Term,
    doc: &Document,
    app: &App,
    pending: Option<&PendingNav>,
    cols: u16,
    rows: u16,
) -> anyhow::Result<()> {
    let view = doc
        .viewport(app.top, rows as usize, cols as usize, app.hscroll)
        .await?;
    let indicator = pending.map(|p| {
        let prog = *p.progress.borrow();
        (p.label, prog.percent())
    });
    term.inner_mut().draw(|f| {
        crate::render::render_viewport(&view, f.area(), f.buffer_mut());
        if let Some((label, pct)) = indicator {
            crate::render::render_progress_row(label, pct, f.area(), f.buffer_mut());
        }
    })?;
    Ok(())
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
    };
    match resolution {
        Some(Resolution::Ready(a)) => app.top = a,
        Some(Resolution::Pending(p)) => *pending = Some(ActiveNav { nav, p }),
        None => {}
    }
    Ok(())
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
/// Runs the event loop until the user quits. Pending navigation resolves in
/// the background: a tick keeps the progress row fresh, completion moves the
/// anchor, Esc cancels, and any new motion supersedes.
pub async fn run(doc: Document) -> anyhow::Result<()> {
    let mut term = crate::terminal::Term::new()?;
    let mut app = App::new();
    let mut pending: Option<ActiveNav> = None;
    let (mut cols, mut rows) = crossterm::terminal::size()?;
    draw(
        &mut term,
        &doc,
        &app,
        pending.as_ref().map(|a| &a.p),
        cols,
        rows,
    )
    .await?;
    let mut events = crossterm::event::EventStream::new();
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            done = async { (&mut pending.as_mut().unwrap().p.handle).await }, if pending.is_some() => {
                pending = None;
                // an aborted or failed scan leaves the anchor where it was;
                // failures are logged so they stay diagnosable even before
                // the status line exists.
                match done {
                    Ok(Ok(anchor)) => app.top = anchor,
                    Ok(Err(e)) => tracing::warn!("background scan failed: {e:#}"),
                    Err(e) if e.is_cancelled() => {}
                    Err(e) => tracing::warn!("background scan panicked: {e:?}"),
                }
                draw(&mut term, &doc, &app, pending.as_ref().map(|a| &a.p), cols, rows).await?;
            }
            _ = tick.tick(), if pending.is_some() => {
                draw(&mut term, &doc, &app, pending.as_ref().map(|a| &a.p), cols, rows).await?;
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
                    Event::Key(key) => match app.handle_key(key) {
                        Action::Quit => {
                            pending.take();
                            break;
                        }
                        Action::Nav(nav) => {
                            apply_nav(&doc, &mut app, &mut pending, nav, rows).await?;
                            draw(&mut term, &doc, &app, pending.as_ref().map(|a| &a.p), cols, rows).await?;
                        }
                        Action::Cancel => {
                            pending = None;
                            draw(&mut term, &doc, &app, pending.as_ref().map(|a| &a.p), cols, rows).await?;
                        }
                        Action::Redraw => draw(&mut term, &doc, &app, pending.as_ref().map(|a| &a.p), cols, rows).await?,
                        Action::None => {}
                    },
                    Event::Mouse(m) => {
                        let nav = match m.kind {
                            MouseEventKind::ScrollDown => Some(Nav::Lines(3)),
                            MouseEventKind::ScrollUp => Some(Nav::Lines(-3)),
                            _ => None,
                        };
                        if let Some(nav) = nav {
                            apply_nav(&doc, &mut app, &mut pending, nav, rows).await?;
                            draw(&mut term, &doc, &app, pending.as_ref().map(|a| &a.p), cols, rows).await?;
                        }
                    }
                    Event::Resize(c, r) => {
                        cols = c;
                        rows = r;
                        handle_resize(&doc, &mut app, &mut pending, rows).await?;
                        draw(&mut term, &doc, &app, pending.as_ref().map(|a| &a.p), cols, rows).await?;
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
    fn q_quits() {
        assert_eq!(App::new().handle_key(key('q')), Action::Quit);
    }
    #[test]
    fn ctrl_c_quits() {
        assert_eq!(App::new().handle_key(ctrl('c')), Action::Quit);
    }
    #[test]
    fn j_scrolls_down_one() {
        assert_eq!(App::new().handle_key(key('j')), Action::Nav(Nav::Lines(1)));
    }
    #[test]
    fn k_scrolls_up_one() {
        assert_eq!(App::new().handle_key(key('k')), Action::Nav(Nav::Lines(-1)));
    }
    #[test]
    fn count_prefix_multiplies_motion() {
        let mut app = App::new();
        assert_eq!(app.handle_key(key('1')), Action::None);
        assert_eq!(app.handle_key(key('2')), Action::None);
        assert_eq!(app.handle_key(key('j')), Action::Nav(Nav::Lines(12)));
    }
    #[test]
    fn count_resets_after_use() {
        let mut app = App::new();
        app.handle_key(key('5'));
        app.handle_key(key('j'));
        assert_eq!(app.handle_key(key('j')), Action::Nav(Nav::Lines(1)));
    }
    #[test]
    fn gg_goes_to_top() {
        let mut app = App::new();
        assert_eq!(app.handle_key(key('g')), Action::None);
        assert_eq!(app.handle_key(key('g')), Action::Nav(Nav::Top));
    }
    #[test]
    fn ge_goes_to_bottom() {
        let mut app = App::new();
        assert_eq!(app.handle_key(key('g')), Action::None);
        assert_eq!(app.handle_key(key('e')), Action::Nav(Nav::Bottom));
    }
    #[test]
    fn g_then_other_key_cancels() {
        let mut app = App::new();
        app.handle_key(key('g'));
        assert_eq!(app.handle_key(key('j')), Action::Nav(Nav::Lines(1)));
    }
    #[test]
    fn a_cancelled_chord_drops_the_count() {
        // 5g followed by j: vim aborts the count with the chord, so the
        // j is a single-line motion, not five.
        let mut app = App::new();
        app.handle_key(key('5'));
        app.handle_key(key('g'));
        assert_eq!(app.handle_key(key('j')), Action::Nav(Nav::Lines(1)));
    }
    #[test]
    fn shift_g_goes_to_bottom() {
        assert_eq!(App::new().handle_key(key('G')), Action::Nav(Nav::Bottom));
    }
    #[test]
    fn count_g_goes_to_line() {
        let mut app = App::new();
        app.handle_key(key('4'));
        app.handle_key(key('2'));
        assert_eq!(app.handle_key(key('G')), Action::Nav(Nav::Line(42)));
    }
    #[test]
    fn bare_g_still_goes_to_bottom() {
        assert_eq!(App::new().handle_key(key('G')), Action::Nav(Nav::Bottom));
    }
    #[test]
    fn count_gg_goes_to_line() {
        let mut app = App::new();
        app.handle_key(key('7'));
        app.handle_key(key('g'));
        assert_eq!(app.handle_key(key('g')), Action::Nav(Nav::Line(7)));
    }
    #[test]
    fn bare_gg_still_goes_to_top() {
        let mut app = App::new();
        app.handle_key(key('g'));
        assert_eq!(app.handle_key(key('g')), Action::Nav(Nav::Top));
    }
    #[test]
    fn ge_ignores_a_count() {
        let mut app = App::new();
        app.handle_key(key('9'));
        app.handle_key(key('g'));
        assert_eq!(app.handle_key(key('e')), Action::Nav(Nav::Bottom));
    }
    #[test]
    fn count_percent_jumps() {
        let mut app = App::new();
        app.handle_key(key('5'));
        app.handle_key(key('0'));
        assert_eq!(app.handle_key(key('%')), Action::Nav(Nav::Percent(50)));
    }
    #[test]
    fn bare_percent_does_nothing() {
        assert_eq!(App::new().handle_key(key('%')), Action::None);
    }
    #[test]
    fn ctrl_d_half_page_down() {
        assert_eq!(
            App::new().handle_key(ctrl('d')),
            Action::Nav(Nav::HalfPage(1))
        );
    }
    #[test]
    fn bare_d_half_page_down() {
        assert_eq!(
            App::new().handle_key(key('d')),
            Action::Nav(Nav::HalfPage(1))
        );
    }
    #[test]
    fn bare_u_half_page_up() {
        assert_eq!(
            App::new().handle_key(key('u')),
            Action::Nav(Nav::HalfPage(-1))
        );
    }
    #[test]
    fn ctrl_f_page_down() {
        assert_eq!(App::new().handle_key(ctrl('f')), Action::Nav(Nav::Page(1)));
    }
    #[test]
    fn l_scrolls_right() {
        assert_eq!(
            App::new().handle_key(key('l')),
            Action::Nav(Nav::Horizontal(1))
        );
    }
    #[test]
    fn ctrl_l_redraws() {
        assert_eq!(App::new().handle_key(ctrl('l')), Action::Redraw);
    }
    #[test]
    fn esc_clears_count() {
        let mut app = App::new();
        app.handle_key(key('9'));
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Action::Cancel
        );
        assert_eq!(app.handle_key(key('j')), Action::Nav(Nav::Lines(1)));
    }
    #[test]
    fn esc_requests_cancel() {
        assert_eq!(
            App::new().handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Action::Cancel
        );
    }
    #[test]
    fn other_keys_do_nothing() {
        assert_eq!(App::new().handle_key(key('z')), Action::None);
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
        let a = (&mut active.p.handle).await.unwrap().unwrap();
        assert_eq!(a.offset(), 196);
    }
    #[tokio::test]
    async fn line_jump_beyond_the_frontier_pends_with_the_jump_label() {
        let mut data = Vec::new();
        for i in 0..4000u32 {
            data.extend_from_slice(format!("line {i:04}\n").as_bytes());
        }
        let src = std::sync::Arc::new(
            ress_core::source::MockSource::new(data)
                .with_latency(std::time::Duration::from_millis(2)),
        );
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
        let a = (&mut active.p.handle).await.unwrap().unwrap();
        assert_eq!(a.offset(), 34990);
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
        let src = std::sync::Arc::new(
            ress_core::source::MockSource::new(data)
                .with_latency(std::time::Duration::from_millis(2)),
        );
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
        let active = pending.take().expect("expected the percent jump to pend");
        let mut rx = active.p.progress.clone();
        drop(active);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while rx.changed().await.is_ok() {}
        })
        .await
        .expect("scan task outlived its drop guard");
    }
}
