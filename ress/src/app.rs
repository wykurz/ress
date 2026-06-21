//! app state and the event loop. `handle_key` is the pure, testable decision;
//! `run` is the thin async driver over crossterm events.
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use futures::StreamExt;
use ress_core::document::Document;
/// what the loop should do after handling an input event.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Quit,
    None,
}
/// pager state. In this plan only the top offset exists (always 0); navigation
/// arrives in Plan 2.
pub struct App {
    pub top: u64,
}
impl App {
    pub fn new() -> Self {
        Self { top: 0 }
    }
    /// decides what a key press means.
    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        match (key.code, key.modifiers) {
            (KeyCode::Char('q'), _) => Action::Quit,
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => Action::Quit,
            _ => Action::None,
        }
    }
}
/// draws the current screen: compute the viewport for the terminal size, then
/// blit it. Viewport I/O happens before `draw` because the draw closure is sync.
async fn draw(
    term: &mut crate::terminal::Term,
    doc: &Document,
    app: &App,
    cols: u16,
    rows: u16,
) -> anyhow::Result<()> {
    let view = doc.viewport(app.top, rows as usize, cols as usize).await?;
    term.inner_mut()
        .draw(|f| crate::render::render_viewport(&view, f.area(), f.buffer_mut()))?;
    Ok(())
}
/// runs the event loop until the user quits.
pub async fn run(doc: Document) -> anyhow::Result<()> {
    let mut term = crate::terminal::Term::new()?;
    let mut app = App::new();
    let (mut cols, mut rows) = crossterm::terminal::size()?;
    draw(&mut term, &doc, &app, cols, rows).await?;
    let mut events = crossterm::event::EventStream::new();
    while let Some(event) = events.next().await {
        match event? {
            Event::Key(key) => match app.handle_key(key) {
                Action::Quit => break,
                Action::None => {}
            },
            Event::Resize(c, r) => {
                cols = c;
                rows = r;
                draw(&mut term, &doc, &app, cols, rows).await?;
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn q_quits() {
        let mut app = App::new();
        let action = app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert_eq!(action, Action::Quit);
    }
    #[test]
    fn ctrl_c_quits() {
        let mut app = App::new();
        let action = app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(action, Action::Quit);
    }
    #[test]
    fn other_keys_do_nothing() {
        let mut app = App::new();
        let action = app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(action, Action::None);
    }
}
