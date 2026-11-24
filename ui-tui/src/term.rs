//! The real terminal: raw mode and the alternate screen, put back however the dashboard ends.
//!
//! The terminal is this process's standard input and output (the CLI handed them over). A guard
//! puts it back when the dashboard returns, fails or panics inside its task, and a panic hook does
//! the same for a panic anywhere else, before the message is printed.

use std::io::{self, Stdout, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossterm::cursor::Show;
use crossterm::event::{self, Event};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

/// Runs `restore` once when it goes out of scope.
pub struct Guard<F: FnMut()> {
    restore: Option<F>,
}

impl<F: FnMut()> Guard<F> {
    pub fn new(restore: F) -> Guard<F> {
        Guard {
            restore: Some(restore),
        }
    }
}

impl<F: FnMut()> Drop for Guard<F> {
    fn drop(&mut self) {
        if let Some(mut restore) = self.restore.take() {
            restore();
        }
    }
}

/// Put the terminal back: leave the alternate screen, show the cursor, leave raw mode. Every step
/// is tried even if an earlier one fails, and it is harmless to call twice.
pub fn leave() {
    let mut out = io::stdout();
    leave_screen(&mut out);
    let _ = disable_raw_mode();
}

/// What `leave` writes: the main screen back, and the cursor.
fn leave_screen(out: &mut impl Write) {
    let _ = execute!(out, LeaveAlternateScreen, Show);
    let _ = out.flush();
}

/// Switch to raw mode and the alternate screen. A panic from now on restores the terminal first.
pub fn enter() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        leave();
        previous(info);
    }));
    enable_raw_mode()?;
    let mut out = io::stdout();
    if let Err(e) = execute!(out, EnterAlternateScreen) {
        leave();
        return Err(e);
    }
    match Terminal::new(CrosstermBackend::new(out)) {
        Ok(terminal) => Ok(terminal),
        Err(e) => {
            leave();
            Err(e)
        }
    }
}

/// Reads terminal events on a thread of its own (reading blocks) and hands them over. It stops
/// when `stop` is set, within a tenth of a second, or when nobody listens any more.
pub fn read_events(
    tx: mpsc::UnboundedSender<Event>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            match event::poll(Duration::from_millis(100)) {
                Ok(true) => match event::read() {
                    Ok(ev) => {
                        if tx.send(ev).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) => {}
                Err(_) => break,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn leaving_writes_the_main_screen_and_the_cursor() {
        let mut written = Vec::new();
        leave_screen(&mut written);
        let text = String::from_utf8(written).unwrap();
        assert!(text.contains("\x1b[?1049l"), "{text:?}");
        assert!(text.contains("\x1b[?25h"), "{text:?}");
    }

    #[test]
    fn the_guard_restores_once_when_it_goes_out_of_scope() {
        let count = Arc::new(AtomicUsize::new(0));
        {
            let seen = count.clone();
            let _guard = Guard::new(move || {
                seen.fetch_add(1, Ordering::SeqCst);
            });
            assert_eq!(count.load(Ordering::SeqCst), 0);
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn the_guard_restores_while_a_panic_unwinds() {
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = Guard::new(move || {
                seen.fetch_add(1, Ordering::SeqCst);
            });
            panic!("boom");
        }));
        assert!(result.is_err());
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
