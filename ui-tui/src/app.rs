//! What the dashboard knows and what a key does. Nothing here draws or touches the terminal, so
//! all of it is tested with a fake daemon.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use oxidepm_core::AppInfo;
use serde_json::{json, Value};

use crate::daemon::Fail;

/// How many log lines are asked for (the view shows as many as fit and scrolls through the rest).
pub const LOG_LINES: usize = 200;

/// How far PageUp and PageDown scroll the log.
const PAGE: usize = 10;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Processes,
    Details,
    Logs,
}

impl Tab {
    pub const ALL: [Tab; 3] = [Tab::Processes, Tab::Details, Tab::Logs];

    pub fn index(self) -> usize {
        Tab::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }

    fn next(self) -> Tab {
        Tab::ALL[(self.index() + 1) % Tab::ALL.len()]
    }

    fn previous(self) -> Tab {
        Tab::ALL[(self.index() + Tab::ALL.len() - 1) % Tab::ALL.len()]
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Stop,
    Restart,
}

impl Action {
    /// The request type, and the word for the prompt.
    fn word(self) -> &'static str {
        match self {
            Action::Stop => "stop",
            Action::Restart => "restart",
        }
    }

    fn past(self) -> &'static str {
        match self {
            Action::Stop => "stopped",
            Action::Restart => "restarted",
        }
    }
}

/// A stop or restart waiting for `y`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Confirm {
    pub action: Action,
    pub id: u32,
    pub name: String,
}

impl Confirm {
    pub fn request(&self) -> Value {
        json!({ "type": self.action.word(), "selector": { "by": "id", "value": self.id } })
    }
}

/// The result of the last thing the person did, shown until the next key.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Note {
    Info(String),
    Error(String),
}

pub struct App {
    pub processes: Vec<AppInfo>,
    pub selected: usize,
    pub tab: Tab,
    /// The log of the selected app, oldest first.
    pub logs: Vec<String>,
    /// How many lines the view is scrolled up from the newest.
    pub log_scroll: usize,
    pub confirm: Option<Confirm>,
    pub note: Option<Note>,
    /// Why the last refresh failed; empty when it worked.
    pub link_error: Option<String>,
    pub quit: bool,
}

impl Default for App {
    fn default() -> Self {
        App::new()
    }
}

/// Text that came from the daemon or from an app, made safe for the terminal: a control
/// character (an escape sequence in a log line, say) is replaced, so what an app prints can never
/// move the cursor or change the screen.
pub fn clean(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\t' => ' ',
            c if c.is_control() => '\u{FFFD}',
            c => c,
        })
        .collect()
}

/// The words of an `error` response.
fn error_text(response: &Value) -> String {
    let message = response["message"].as_str().unwrap_or("no message");
    match response["code"].as_str() {
        Some(code) => clean(&format!("{code}: {message}")),
        None => clean(message),
    }
}

impl App {
    pub fn new() -> App {
        App {
            processes: Vec::new(),
            selected: 0,
            tab: Tab::Processes,
            logs: Vec::new(),
            log_scroll: 0,
            confirm: None,
            note: None,
            link_error: None,
            quit: false,
        }
    }

    pub fn selected_app(&self) -> Option<&AppInfo> {
        self.processes.get(self.selected)
    }

    /// Apply the status reply, retaining the selected app when it is still present.
    pub fn apply_status(&mut self, answer: Result<Value, Fail>) {
        let answer = match answer {
            Ok(answer) => answer,
            Err(fail) => {
                self.link_error = Some(clean(&format!("daemon: {fail}")));
                return;
            }
        };
        match answer["type"].as_str() {
            Some("status") => {
                match serde_json::from_value::<Vec<AppInfo>>(answer["apps"].clone()) {
                    Ok(apps) => {
                        let id = self.selected_app().map(|a| a.spec.id);
                        self.processes = apps;
                        self.selected = id
                            .and_then(|id| self.processes.iter().position(|a| a.spec.id == id))
                            .unwrap_or(self.selected);
                        self.selected = self.selected.min(self.processes.len().saturating_sub(1));
                        if self.selected_app().map(|a| a.spec.id) != id {
                            self.logs.clear();
                            self.log_scroll = 0;
                        }
                        self.link_error = None;
                    }
                    Err(e) => {
                        self.link_error =
                            Some(clean(&format!("the list of apps could not be read: {e}")))
                    }
                }
            }
            Some("error") => self.link_error = Some(error_text(&answer)),
            _ => {
                self.link_error =
                    Some("the daemon sent an answer this dashboard does not know".to_string())
            }
        }
    }

    pub fn log_request(&self) -> Option<Value> {
        if self.tab != Tab::Logs {
            return None;
        }
        let id = self.selected_app()?.spec.id;
        Some(json!({
            "type": "logs",
            "selector": { "by": "id", "value": id },
            "lines": LOG_LINES,
            "follow": false,
            "stdout": true,
            "stderr": true,
        }))
    }

    pub fn apply_logs(&mut self, answer: Result<Value, Fail>) {
        match answer {
            Err(fail) => self.link_error = Some(clean(&format!("logs: {fail}"))),
            Ok(answer) => match answer["type"].as_str() {
                Some("log_lines") => {
                    self.logs = answer["lines"]
                        .as_array()
                        .map(|l| l.iter().filter_map(Value::as_str).map(clean).collect())
                        .unwrap_or_default();
                    self.log_scroll = self.log_scroll.min(self.logs.len());
                }
                Some("error") => self.link_error = Some(error_text(&answer)),
                _ => {
                    self.link_error =
                        Some("the daemon sent an answer this dashboard does not know".to_string())
                }
            },
        }
    }

    fn select(&mut self, forward: bool) {
        if self.processes.is_empty() {
            return;
        }
        let last = self.processes.len() - 1;
        self.selected = match (forward, self.selected) {
            (true, i) if i >= last => 0,
            (true, i) => i + 1,
            (false, 0) => last,
            (false, i) => i - 1,
        };
        // the log on screen belonged to the app that was selected
        self.logs.clear();
        self.log_scroll = 0;
    }

    fn scroll_older(&mut self, lines: usize) {
        self.log_scroll = self.log_scroll.saturating_add(lines).min(self.logs.len());
    }

    fn scroll_newer(&mut self, lines: usize) {
        self.log_scroll = self.log_scroll.saturating_sub(lines);
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Confirm> {
        self.note = None;
        // a stop or a restart waits for y; any other key says no
        if let Some(confirm) = self.confirm.take() {
            if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                return Some(confirm);
            }
            self.note = Some(Note::Info("cancelled".to_string()));
            return None;
        }
        // in raw mode Ctrl-C arrives as a key, and must still end the dashboard
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return None;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Down | KeyCode::Char('j') if self.tab == Tab::Logs => self.scroll_newer(1),
            KeyCode::Up | KeyCode::Char('k') if self.tab == Tab::Logs => self.scroll_older(1),
            KeyCode::PageDown if self.tab == Tab::Logs => self.scroll_newer(PAGE),
            KeyCode::PageUp if self.tab == Tab::Logs => self.scroll_older(PAGE),
            KeyCode::End if self.tab == Tab::Logs => self.log_scroll = 0,
            KeyCode::Home if self.tab == Tab::Logs => self.log_scroll = self.logs.len(),
            KeyCode::Down | KeyCode::Char('j') => self.select(true),
            KeyCode::Up | KeyCode::Char('k') => self.select(false),
            KeyCode::Tab => self.tab = self.tab.next(),
            KeyCode::BackTab => self.tab = self.tab.previous(),
            KeyCode::Char('l') => self.tab = Tab::Logs,
            KeyCode::Char('s') => self.ask(Action::Stop),
            KeyCode::Char('r') => self.ask(Action::Restart),
            _ => {}
        }
        None
    }

    fn ask(&mut self, action: Action) {
        match self.selected_app() {
            Some(app) => {
                self.confirm = Some(Confirm {
                    action,
                    id: app.spec.id,
                    name: clean(&app.spec.name),
                })
            }
            None => self.note = Some(Note::Error("no app is selected".to_string())),
        }
    }

    pub fn apply_action(&mut self, confirm: Confirm, answer: Result<Value, Fail>) {
        let Confirm { action, name, .. } = confirm;
        self.note = Some(match answer {
            Err(fail) => Note::Error(clean(&format!("{} '{name}': {fail}", action.word()))),
            Ok(answer) => self.outcome(action, &name, &answer),
        });
    }

    /// What the daemon's answer to a stop or restart means for the person: a refusal of the whole
    /// request, or of this one app, is an error and is shown as one.
    fn outcome(&self, action: Action, name: &str, answer: &Value) -> Note {
        if answer["type"] == "error" {
            return Note::Error(format!(
                "{} '{name}': {}",
                action.word(),
                error_text(answer)
            ));
        }
        let refused = answer["items"]
            .as_array()
            .and_then(|items| items.iter().find(|i| i["result"]["outcome"] == "err"));
        if let Some(item) = refused {
            let why = clean(&format!(
                "{}: {}",
                item["result"]["code"].as_str().unwrap_or("error"),
                item["result"]["message"].as_str().unwrap_or("no message")
            ));
            return Note::Error(format!("{} '{name}': {why}", action.word()));
        }
        if answer["type"]
            .as_str()
            .is_some_and(|t| t == "stopped" || t == "restarted")
        {
            Note::Info(format!("{} '{name}'", action.past()))
        } else {
            Note::Error(format!(
                "{} '{name}': the daemon sent an answer this dashboard does not know",
                action.word()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::Daemon;
    use crate::testkit::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use serde_json::json;

    impl App {
        async fn refresh(&mut self, daemon: &dyn Daemon) {
            self.apply_status(daemon.call(json!({ "type": "status" })).await);
        }

        async fn refresh_visible_logs(&mut self, daemon: &dyn Daemon) {
            if let Some(request) = self.log_request() {
                self.apply_logs(daemon.call(request).await);
            }
        }

        async fn tick(&mut self, daemon: &dyn Daemon) {
            self.refresh(daemon).await;
            self.refresh_visible_logs(daemon).await;
        }

        async fn on_key(&mut self, daemon: &dyn Daemon, key: KeyEvent) {
            let tab = self.tab;
            if let Some(confirm) = self.handle_key(key) {
                let answer = daemon.call(confirm.request()).await;
                self.apply_action(confirm, answer);
                self.refresh(daemon).await;
            }
            if self.tab != tab {
                self.refresh_visible_logs(daemon).await;
            }
        }
    }

    async fn ready(fake: &Fake) -> App {
        let mut app = App::new();
        app.refresh(fake).await;
        app
    }

    #[tokio::test]
    async fn s_asks_first_and_sends_nothing_until_y() {
        let fake = Fake::serving(vec![app(7, "web"), app(8, "api")], vec![]);
        let mut app = ready(&fake).await;
        app.on_key(&fake, ch('s')).await;
        assert_eq!(
            app.confirm
                .as_ref()
                .map(|c| (c.action, c.id, c.name.as_str())),
            Some((Action::Stop, 7, "web"))
        );
        assert_eq!(fake.count("stop"), 0, "nothing was sent for the key alone");
        app.on_key(&fake, ch('y')).await;
        assert_eq!(
            fake.last("stop").unwrap()["selector"],
            json!({ "by": "id", "value": 7 })
        );
        assert!(app.confirm.is_none());
        assert_eq!(app.note, Some(Note::Info("stopped 'web'".to_string())));
    }

    #[tokio::test]
    async fn r_asks_first_and_restarts_on_y() {
        let fake = Fake::serving(vec![app(7, "web")], vec![]);
        let mut app = ready(&fake).await;
        app.on_key(&fake, ch('r')).await;
        assert_eq!(
            app.confirm.as_ref().map(|c| c.action),
            Some(Action::Restart)
        );
        assert_eq!(fake.count("restart"), 0);
        app.on_key(&fake, ch('y')).await;
        assert_eq!(fake.count("restart"), 1);
        assert_eq!(app.note, Some(Note::Info("restarted 'web'".to_string())));
    }

    #[tokio::test]
    async fn any_other_key_answers_the_question_with_no() {
        let fake = Fake::serving(vec![app(7, "web")], vec![]);
        let mut app = ready(&fake).await;
        for no in [
            ch('n'),
            ch('x'),
            key(KeyCode::Enter),
            key(KeyCode::Esc),
            ch('q'),
        ] {
            app.on_key(&fake, ch('s')).await;
            assert!(app.confirm.is_some());
            app.on_key(&fake, no).await;
            assert!(app.confirm.is_none());
            assert_eq!(app.note, Some(Note::Info("cancelled".to_string())));
            assert!(
                !app.quit,
                "a key that answers the question does nothing else"
            );
        }
        assert_eq!(fake.count("stop") + fake.count("restart"), 0);
    }

    #[tokio::test]
    async fn the_daemons_refusal_is_shown_in_the_status_line() {
        let fake = Fake::new(|request| match request["type"].as_str() {
            Some("status") => Ok(json!({ "type": "status", "apps": [app(7, "web")] })),
            _ => Ok(
                json!({ "type": "error", "code": "E_PROTECTED", "message": "'web' is protected" }),
            ),
        });
        let mut app = ready(&fake).await;
        app.on_key(&fake, ch('s')).await;
        app.on_key(&fake, ch('y')).await;
        let Some(Note::Error(text)) = app.note.clone() else {
            panic!("an error note, got {:?}", app.note)
        };
        assert!(
            text.contains("E_PROTECTED")
                && text.contains("'web' is protected")
                && text.contains("stop"),
            "{text}"
        );
        let (line, _) = crate::view::bottom_line(&app);
        assert_eq!(line, text);
    }

    #[tokio::test]
    async fn a_refusal_of_the_one_app_inside_a_stopped_answer_is_shown() {
        let fake = Fake::new(|request| match request["type"].as_str() {
            Some("status") => Ok(json!({ "type": "status", "apps": [app(7, "web")] })),
            _ => Ok(json!({ "type": "stopped", "count": 0, "items": [
                { "id": 7, "name": "web", "result": { "outcome": "err", "code": "E_POLICY_DENIED", "message": "denied" } } ] })),
        });
        let mut app = ready(&fake).await;
        app.on_key(&fake, ch('s')).await;
        app.on_key(&fake, ch('y')).await;
        let Some(Note::Error(text)) = app.note.clone() else {
            panic!("an error note, got {:?}", app.note)
        };
        assert!(
            text.contains("E_POLICY_DENIED") && text.contains("denied"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_request_the_daemon_never_answers_is_an_error_not_a_success() {
        let fake = Fake::new(|request| match request["type"].as_str() {
            Some("status") => Ok(json!({ "type": "status", "apps": [app(7, "web")] })),
            _ => Err(Fail("the daemon is gone".into())),
        });
        let mut app = ready(&fake).await;
        app.on_key(&fake, ch('r')).await;
        app.on_key(&fake, ch('y')).await;
        assert!(
            matches!(&app.note, Some(Note::Error(t)) if t.contains("the daemon is gone") && t.contains("restart 'web'")),
            "{:?}",
            app.note
        );
    }

    #[tokio::test]
    async fn a_failed_refresh_keeps_the_list_and_says_why_until_one_works() {
        let broken = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = broken.clone();
        let fake = Fake::new(move |_| {
            if flag.load(std::sync::atomic::Ordering::SeqCst) {
                Err(Fail("the daemon did not answer in time".into()))
            } else {
                Ok(json!({ "type": "status", "apps": [app(7, "web")] }))
            }
        });
        let mut app = ready(&fake).await;
        broken.store(true, std::sync::atomic::Ordering::SeqCst);
        app.tick(&fake).await;
        assert_eq!(app.processes.len(), 1, "the last list stays on screen");
        assert!(app
            .link_error
            .as_deref()
            .is_some_and(|e| e.contains("did not answer in time")));
        assert!(crate::view::bottom_line(&app)
            .0
            .contains("did not answer in time"));
        broken.store(false, std::sync::atomic::Ordering::SeqCst);
        app.tick(&fake).await;
        assert!(app.link_error.is_none());
    }

    #[tokio::test]
    async fn an_error_answer_to_the_list_is_shown() {
        let fake = Fake::new(|_| {
            Ok(json!({ "type": "error", "code": "E_POLICY_DENIED", "message": "no read access" }))
        });
        let mut app = App::new();
        app.refresh(&fake).await;
        assert!(app
            .link_error
            .as_deref()
            .is_some_and(|e| e.contains("E_POLICY_DENIED") && e.contains("no read access")));
    }

    #[tokio::test]
    async fn the_log_is_asked_for_again_on_every_tick_while_the_logs_tab_is_open() {
        let fake = Fake::serving(vec![app(7, "web")], vec!["one", "two"]);
        let mut app = ready(&fake).await;
        app.tick(&fake).await;
        assert_eq!(
            fake.count("logs"),
            0,
            "the processes tab does not ask for the log"
        );
        app.on_key(&fake, ch('l')).await;
        assert_eq!(app.tab, Tab::Logs);
        assert_eq!(
            fake.count("logs"),
            1,
            "opening the tab shows the log at once"
        );
        app.tick(&fake).await;
        app.tick(&fake).await;
        assert_eq!(fake.count("logs"), 3);
        assert_eq!(
            fake.last("logs").unwrap()["selector"],
            json!({ "by": "id", "value": 7 })
        );
        app.on_key(&fake, key(KeyCode::Tab)).await; // to the processes tab
        app.tick(&fake).await;
        assert_eq!(fake.count("logs"), 3, "leaving the tab stops it");
    }

    #[tokio::test]
    async fn the_arrow_keys_scroll_the_log_and_do_not_change_the_app() {
        let lines: Vec<&'static str> = vec![
            "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m",
        ];
        let fake = Fake::serving(vec![app(7, "web"), app(8, "api")], lines);
        let mut app = ready(&fake).await;
        app.on_key(&fake, ch('l')).await;
        app.on_key(&fake, key(KeyCode::Up)).await;
        app.on_key(&fake, ch('k')).await;
        assert_eq!((app.log_scroll, app.selected), (2, 0));
        app.on_key(&fake, key(KeyCode::Down)).await;
        assert_eq!(app.log_scroll, 1);
        app.on_key(&fake, key(KeyCode::PageUp)).await;
        assert_eq!(app.log_scroll, 11);
        app.on_key(&fake, key(KeyCode::PageUp)).await;
        assert_eq!(app.log_scroll, 13, "never past the oldest line");
        app.on_key(&fake, key(KeyCode::End)).await;
        assert_eq!(app.log_scroll, 0);
        app.on_key(&fake, key(KeyCode::Home)).await;
        assert_eq!(app.log_scroll, 13);
        app.on_key(&fake, key(KeyCode::PageDown)).await;
        assert_eq!(app.log_scroll, 3);
        // a tick brings new lines and keeps the place
        app.tick(&fake).await;
        assert_eq!(app.log_scroll, 3);
    }

    #[tokio::test]
    async fn the_arrow_keys_select_on_the_other_tabs_and_wrap() {
        let fake = Fake::serving(vec![app(7, "web"), app(8, "api"), app(9, "db")], vec![]);
        let mut app = ready(&fake).await;
        app.on_key(&fake, key(KeyCode::Up)).await;
        assert_eq!(app.selected, 2);
        app.on_key(&fake, key(KeyCode::Down)).await;
        assert_eq!(app.selected, 0);
        app.on_key(&fake, ch('j')).await;
        assert_eq!(app.selected, 1);
        app.on_key(&fake, key(KeyCode::Tab)).await;
        assert_eq!(app.tab, Tab::Details);
        app.on_key(&fake, key(KeyCode::Down)).await;
        assert_eq!(app.selected, 2);
        app.on_key(&fake, key(KeyCode::BackTab)).await;
        assert_eq!(app.tab, Tab::Processes);
    }

    #[tokio::test]
    async fn the_selection_stays_on_its_app_when_the_list_changes() {
        let list = std::sync::Arc::new(std::sync::Mutex::new(vec![
            app(7, "web"),
            app(8, "api"),
            app(9, "db"),
        ]));
        let shared = list.clone();
        let fake =
            Fake::new(move |_| Ok(json!({ "type": "status", "apps": *shared.lock().unwrap() })));
        let mut app = ready(&fake).await;
        app.on_key(&fake, key(KeyCode::Down)).await;
        app.on_key(&fake, key(KeyCode::Down)).await;
        assert_eq!(app.selected_app().map(|a| a.spec.id), Some(9));
        list.lock().unwrap().remove(0);
        app.tick(&fake).await;
        assert_eq!(
            app.selected_app().map(|a| a.spec.id),
            Some(9),
            "same app, new place"
        );
        list.lock().unwrap().clear();
        app.tick(&fake).await;
        assert!(app.selected_app().is_none() && app.selected == 0);
        app.on_key(&fake, ch('s')).await;
        assert!(app.confirm.is_none(), "nothing to stop");
        assert!(matches!(app.note, Some(Note::Error(_))));
    }

    #[tokio::test]
    async fn control_characters_in_logs_names_and_errors_never_reach_the_terminal() {
        let fake = Fake::new(|request| {
            Ok(match request["type"].as_str() {
                Some("status") => {
                    // the daemon validates names, but what comes from it is not trusted with the terminal
                    let mut hostile = app(7, "web");
                    hostile.spec.name = "we\u{1b}[2Jb".to_string();
                    json!({ "type": "status", "apps": [hostile] })
                }
                Some("logs") => {
                    json!({ "type": "log_lines", "lines": ["ok\u{1b}]0;title\u{7}\tx", "plain"] })
                }
                _ => json!({ "type": "error", "code": "E_X", "message": "bad\u{1b}[31m" }),
            })
        });
        let mut app = ready(&fake).await;
        app.on_key(&fake, ch('l')).await;
        assert!(
            app.logs.iter().all(|l| !l.chars().any(char::is_control)),
            "{:?}",
            app.logs
        );
        assert_eq!(app.logs[1], "plain");
        app.on_key(&fake, key(KeyCode::Tab)).await;
        app.on_key(&fake, ch('s')).await;
        assert!(!app
            .confirm
            .as_ref()
            .unwrap()
            .name
            .chars()
            .any(char::is_control));
        app.on_key(&fake, ch('y')).await;
        let Some(Note::Error(text)) = &app.note else {
            panic!("{:?}", app.note)
        };
        assert!(!text.chars().any(char::is_control), "{text:?}");
    }

    #[tokio::test]
    async fn q_escape_and_ctrl_c_quit_when_nothing_is_asked() {
        let fake = Fake::serving(vec![app(7, "web")], vec![]);
        for quit in [
            ch('q'),
            key(KeyCode::Esc),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        ] {
            let mut app = ready(&fake).await;
            app.on_key(&fake, quit).await;
            assert!(app.quit, "{quit:?}");
        }
    }
}
