//! terminal setup/teardown. `Term` enters the alternate screen + raw mode on
//! construction and restores the terminal on drop, so a normal exit, an error,
//! or a panic never leaves the user's shell wrecked.
use ratatui::backend::CrosstermBackend;
use std::io::Stdout;
/// a ratatui terminal that restores cooked mode + the main screen on drop.
pub struct Term {
    inner: ratatui::Terminal<CrosstermBackend<Stdout>>,
}
impl Term {
    pub fn new() -> anyhow::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        // raw mode is on now; if entering the alt screen or building the terminal
        // fails, restore before returning so the tty is never left in raw mode.
        let inner = match Self::enter() {
            Ok(inner) => inner,
            Err(err) => {
                let _ = restore();
                return Err(err);
            }
        };
        install_panic_hook();
        Ok(Self { inner })
    }
    fn enter() -> anyhow::Result<ratatui::Terminal<CrosstermBackend<Stdout>>> {
        let mut stdout = std::io::stdout();
        crossterm::execute!(
            stdout,
            crossterm::terminal::EnterAlternateScreen,
            crossterm::cursor::Hide
        )?;
        let inner = ratatui::Terminal::new(CrosstermBackend::new(stdout))?;
        Ok(inner)
    }
    /// access the underlying ratatui terminal for drawing.
    pub fn inner_mut(&mut self) -> &mut ratatui::Terminal<CrosstermBackend<Stdout>> {
        &mut self.inner
    }
}
impl Drop for Term {
    fn drop(&mut self) {
        let _ = restore();
    }
}
/// undoes `Term::new` terminal changes; safe to call more than once.
fn restore() -> std::io::Result<()> {
    crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::cursor::Show
    )?;
    crossterm::terminal::disable_raw_mode()
}
/// chains a terminal-restoring hook before the previous panic hook so a panic
/// prints normally instead of into a raw-mode alternate screen.
fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore();
        prev(info);
    }));
}
