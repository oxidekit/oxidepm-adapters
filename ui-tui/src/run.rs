//! The dashboard's loop: draw, then wait for a key or the next tick.

use std::io;
use std::time::Duration;

use crossterm::event::{Event, KeyEventKind};
use ratatui::backend::Backend;
use ratatui::Terminal;
use serde_json::json;
use tokio::sync::mpsc;

use crate::app::{App, Confirm, Note};
use crate::daemon::Daemon;
use crate::term::Guard;
use crate::view;

/// How often the list (and the log, when it is on screen) is asked for again.
pub const TICK: Duration = Duration::from_secs(1);

/// Draw until the person quits. `Err` when the terminal cannot be drawn on, or its input ended.
pub async fn run_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    daemon: &dyn Daemon,
    events: &mut mpsc::UnboundedReceiver<Event>,
) -> io::Result<()>
where
    B::Error: Send + Sync + 'static,
{
    let status_request = || daemon.call(json!({ "type": "status" }));
    let action_request = |confirm: Confirm| async move {
        let answer = daemon.call(confirm.request()).await;
        (confirm, answer)
    };
    let mut status = Some(status_request());
    let mut logs = None;
    let mut action = None;
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + TICK, TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        terminal.draw(|f| view::ui(f, app)).map_err(|error| {
            io::Error::other(error)
                .downcast::<io::Error>()
                .unwrap_or_else(|error| error)
        })?;
        if app.quit {
            return Ok(());
        }
        tokio::select! {
            biased;
            (confirm, answer) = async { action.as_mut().unwrap().await }, if action.is_some() => {
                action = None;
                app.apply_action(confirm, answer);
                status = Some(status_request());
                logs = None;
            }
            answer = async { status.as_mut().unwrap().await }, if status.is_some() => {
                status = None;
                let selected = app.selected_app().map(|a| a.spec.id);
                app.apply_status(answer);
                if logs.is_none() || selected != app.selected_app().map(|a| a.spec.id) {
                    logs = app.log_request().map(|request| daemon.call(request));
                }
            }
            answer = async { logs.as_mut().unwrap().await }, if logs.is_some() => {
                logs = None;
                app.apply_logs(answer);
            }
            event = events.recv() => match event {
                Some(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                    let selected = app.selected_app().map(|a| a.spec.id);
                    let tab = app.tab;
                    if let Some(confirm) = app.handle_key(key) {
                        if action.is_none() {
                            action = Some(Box::pin(action_request(confirm)));
                        } else {
                            app.note = Some(Note::Info("a stop or restart is already in progress".to_string()));
                        }
                    } else if selected != app.selected_app().map(|a| a.spec.id) {
                        logs = None;
                        status = Some(status_request());
                    } else if app.tab != tab {
                        logs = app.log_request().map(|request| daemon.call(request));
                    }
                }
                // a new size is drawn at once by the next pass
                Some(_) => {}
                None => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the terminal input ended")),
            },
            _ = tick.tick(), if status.is_none() && logs.is_none() => status = Some(status_request()),
        }
    }
}

/// [`run_loop`] with the terminal put back, by `restore`, however it ends: it returns, fails, or
/// panics (the guard runs while the panic unwinds this task).
pub async fn run_guarded<B: Backend>(
    mut terminal: Terminal<B>,
    restore: impl FnMut(),
    daemon: &dyn Daemon,
    events: &mut mpsc::UnboundedReceiver<Event>,
) -> io::Result<()>
where
    B::Error: Send + Sync + 'static,
{
    let _guard = Guard::new(restore);
    let mut app = App::new();
    run_loop(&mut terminal, &mut app, daemon, events).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::*;
    use async_trait::async_trait;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use ratatui::backend::{Backend, ClearType, TestBackend, WindowSize};
    use ratatui::buffer::Cell;
    use ratatui::layout::{Position, Size};
    use serde_json::{json, Value};
    use std::convert::Infallible;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn press(c: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    /// A terminal that cannot be drawn on.
    struct Broken(TestBackend);

    fn as_io_result<T>(result: Result<T, Infallible>) -> io::Result<T> {
        result.map_err(|error| match error {})
    }

    impl Backend for Broken {
        type Error = io::Error;

        fn draw<'a, I>(&mut self, _content: I) -> io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the terminal went away",
            ))
        }
        fn hide_cursor(&mut self) -> io::Result<()> {
            as_io_result(self.0.hide_cursor())
        }
        fn show_cursor(&mut self) -> io::Result<()> {
            as_io_result(self.0.show_cursor())
        }
        fn get_cursor_position(&mut self) -> io::Result<Position> {
            as_io_result(self.0.get_cursor_position())
        }
        fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
            as_io_result(self.0.set_cursor_position(position))
        }
        fn clear(&mut self) -> io::Result<()> {
            as_io_result(self.0.clear())
        }
        fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
            as_io_result(self.0.clear_region(clear_type))
        }
        fn size(&self) -> io::Result<Size> {
            as_io_result(self.0.size())
        }
        fn window_size(&mut self) -> io::Result<WindowSize> {
            Ok(WindowSize {
                columns_rows: Size {
                    width: 80,
                    height: 24,
                },
                pixels: Size {
                    width: 0,
                    height: 0,
                },
            })
        }
        fn flush(&mut self) -> io::Result<()> {
            as_io_result(self.0.flush())
        }
    }

    #[tokio::test]
    async fn the_loop_draws_handles_keys_and_ends_on_q() {
        let fake = Fake::serving(vec![app(7, "web")], vec![]);
        let (tx, mut events) = mpsc::unbounded_channel();
        // a key release is not a key press
        let release = KeyEvent::new_with_kind(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        tx.send(Event::Key(release)).unwrap();
        tx.send(Event::Resize(100, 30)).unwrap();
        tx.send(press('s')).unwrap();
        tx.send(press('y')).unwrap();
        tx.send(press('q')).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut app = App::new();
        run_loop(&mut terminal, &mut app, &fake, &mut events)
            .await
            .unwrap();
        assert_eq!(fake.count("stop"), 1);
        assert!(app.quit);
    }

    struct SlowRefresh<'a> {
        request: &'static str,
        started: &'a tokio::sync::Notify,
        status_calls: AtomicUsize,
    }

    #[async_trait]
    impl Daemon for SlowRefresh<'_> {
        async fn call(&self, request: Value) -> Result<Value, crate::daemon::Fail> {
            if request["type"] == self.request {
                self.started.notify_one();
                std::future::pending().await
            } else {
                if request["type"] == "status" {
                    self.status_calls.fetch_add(1, Ordering::Relaxed);
                }
                Ok(json!({ "type": "status", "apps": [app(7, "web")] }))
            }
        }
    }

    #[tokio::test]
    async fn quit_interrupts_slow_daemon_requests() {
        for (request, keys) in [
            ("status", ""),
            ("logs", "l"),
            ("stop", "sy"),
            ("restart", "ry"),
        ] {
            let started = tokio::sync::Notify::new();
            let daemon = SlowRefresh {
                request,
                started: &started,
                status_calls: AtomicUsize::new(0),
            };
            let (tx, mut events) = mpsc::unbounded_channel();
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            let mut app = App::new();
            for key in keys.chars() {
                tx.send(press(key)).unwrap();
            }
            tokio::time::timeout(Duration::from_secs(1), async {
                let (result, ()) = tokio::join!(
                    run_loop(&mut terminal, &mut app, &daemon, &mut events),
                    async {
                        started.notified().await;
                        tx.send(press('q')).unwrap();
                    },
                );
                result.unwrap();
            })
            .await
            .unwrap_or_else(|_| panic!("quit was blocked by the {request} request"));
            assert!(app.quit);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_list_refreshes_while_a_confirmed_stop_is_pending() {
        let started = tokio::sync::Notify::new();
        let daemon = SlowRefresh {
            request: "stop",
            started: &started,
            status_calls: AtomicUsize::new(0),
        };
        let (tx, mut events) = mpsc::unbounded_channel();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut app = App::new();
        tx.send(press('s')).unwrap();
        tx.send(press('y')).unwrap();
        tokio::time::timeout(TICK * 4, async {
            let (result, ()) = tokio::join!(
                run_loop(&mut terminal, &mut app, &daemon, &mut events),
                async {
                    started.notified().await;
                    tokio::time::sleep(TICK * 2).await;
                    assert!(
                        daemon.status_calls.load(Ordering::Relaxed) > 1,
                        "the list did not refresh"
                    );
                    tx.send(press('q')).unwrap();
                },
            );
            result.unwrap();
        })
        .await
        .expect("a pending stop leaves the dashboard usable");
    }

    #[tokio::test]
    async fn the_tick_asks_for_the_list_again_while_the_loop_waits() {
        tokio::time::pause();
        let fake = Fake::serving(vec![app(7, "web")], vec![]);
        let (tx, mut events) = mpsc::unbounded_channel::<Event>();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut app = App::new();
        let quit = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(3500)).await;
            tx.send(press('q')).unwrap();
            tx
        });
        run_loop(&mut terminal, &mut app, &fake, &mut events)
            .await
            .unwrap();
        let _ = quit.await;
        assert!(
            fake.count("status") >= 4,
            "one at the start and one for each second: {}",
            fake.count("status")
        );
    }

    #[tokio::test]
    async fn the_input_ending_ends_the_loop_with_an_error() {
        let fake = Fake::serving(vec![], vec![]);
        let (tx, mut events) = mpsc::unbounded_channel::<Event>();
        drop(tx);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let err = run_loop(&mut terminal, &mut App::new(), &fake, &mut events)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn the_terminal_is_put_back_when_the_loop_returns() {
        let fake = Fake::serving(vec![], vec![]);
        let (tx, mut events) = mpsc::unbounded_channel();
        tx.send(press('q')).unwrap();
        let restored = Arc::new(AtomicUsize::new(0));
        let count = restored.clone();
        let terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        run_guarded(
            terminal,
            move || {
                count.fetch_add(1, Ordering::SeqCst);
            },
            &fake,
            &mut events,
        )
        .await
        .unwrap();
        assert_eq!(restored.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn the_terminal_is_put_back_when_drawing_fails() {
        let fake = Fake::serving(vec![app(7, "web")], vec![]);
        let (_tx, mut events) = mpsc::unbounded_channel();
        let restored = Arc::new(AtomicUsize::new(0));
        let count = restored.clone();
        let terminal = Terminal::new(Broken(TestBackend::new(80, 24))).unwrap();
        let err = run_guarded(
            terminal,
            move || {
                count.fetch_add(1, Ordering::SeqCst);
            },
            &fake,
            &mut events,
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(restored.load(Ordering::SeqCst), 1, "put back exactly once");
    }

    /// A daemon that panics on the second request.
    struct Panics(AtomicUsize);

    #[async_trait]
    impl Daemon for Panics {
        async fn call(&self, _request: Value) -> Result<Value, crate::daemon::Fail> {
            if self.0.fetch_add(1, Ordering::SeqCst) >= 1 {
                panic!("a bug in the dashboard");
            }
            Ok(json!({ "type": "status", "apps": [app(7, "web")] }))
        }
    }

    #[tokio::test]
    async fn the_terminal_is_put_back_when_the_dashboard_panics() {
        let (tx, mut events) = mpsc::unbounded_channel();
        let restored = Arc::new(AtomicUsize::new(0));
        let count = restored.clone();
        let task = tokio::spawn(async move {
            let daemon = Panics(AtomicUsize::new(0));
            let terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            // the second request is made by the key below
            run_guarded(
                terminal,
                move || {
                    count.fetch_add(1, Ordering::SeqCst);
                },
                &daemon,
                &mut events,
            )
            .await
        });
        tx.send(press('l')).unwrap();
        let outcome = task.await;
        assert!(outcome.is_err() && outcome.unwrap_err().is_panic());
        assert_eq!(restored.load(Ordering::SeqCst), 1);
    }
}
