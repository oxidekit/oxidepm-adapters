//! `oxidepm monit` against the real `oxidepmd` and `oxidepm` of the core repository. The terminal
//! is a pseudo-terminal whose screen the tests read; the dashboard runs as a session adapter,
//! started by the daemon under its sandbox on that terminal.

mod support;

use std::process::{Child, Stdio};

use serde_json::json;
use support::*;

/// `oxidepm monit` running on a terminal.
struct Monit {
    screen: Screen,
    child: Child,
}

impl Monit {
    fn start(env: &Env) -> Monit {
        Monit::start_with(env, 100, 30, &[])
    }

    fn start_with(env: &Env, width: u16, height: u16, extra_env: &[(&str, &str)]) -> Monit {
        let mut screen = Screen::new(width, height);
        let mut command = env.cli_command(&["monit"]);
        for (k, v) in extra_env {
            command.env(k, v);
        }
        let child = command
            .stdin(screen.stdio())
            .stdout(screen.stdio())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run oxidepm monit");
        screen.release_slave();
        Monit { screen, child }
    }

    fn wait_exit(&mut self) -> i32 {
        wait_child(&mut self.child, 30).expect("oxidepm monit did not end")
    }

    fn stderr(&mut self) -> String {
        let mut text = String::new();
        if let Some(mut pipe) = self.child.stderr.take() {
            let _ = std::io::Read::read_to_string(&mut pipe, &mut text);
        }
        text
    }
}

impl Drop for Monit {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn the_dashboard_shows_the_apps_on_the_terminal_and_q_leaves_it_clean() {
    let env = Env::new();
    env.start_app("web", "sleep 300");
    env.start_app("api", "sleep 300");
    env.wait_running("web");
    env.wait_running("api");
    let mut monit = Monit::start(&env);
    assert!(
        monit.screen.wait_for_text("OxidePM Monitor", 30),
        "{}\n{}",
        monit.screen.text(),
        env.daemon_stderr()
    );
    assert!(
        monit.screen.wait_for_text("api", 10) && monit.screen.wait_for_text("web", 10),
        "{}",
        monit.screen.text()
    );
    assert!(
        monit.screen.text().contains("running"),
        "{}",
        monit.screen.text()
    );
    // the session is a process of the daemon's, not the CLI's
    assert!(
        !env.adapter_pids().is_empty(),
        "the adapter runs while the dashboard is open"
    );
    monit.screen.type_keys("q");
    assert_eq!(monit.wait_exit(), 0, "{}", monit.stderr());
    let raw = monit.screen.raw_text();
    let leave = raw
        .rfind("\x1b[?1049l")
        .expect("the alternate screen was left");
    assert!(
        raw[leave..].contains("\x1b[?25h"),
        "the cursor is shown again"
    );
    assert!(
        wait_for(20, || env.adapter_pids().is_empty()),
        "the adapter ends with the session"
    );
    assert!(monit.screen.raw_text().contains("\x1b[?1049h"));
}

#[test]
fn the_terminal_settings_are_as_they_were_after_the_dashboard() {
    let env = Env::new();
    let mut monit = Monit::start(&env);
    // the CLI holds the terminal; compare through the descriptor the test kept for itself
    assert!(
        monit.screen.wait_for_text("OxidePM Monitor", 30),
        "{}",
        env.daemon_stderr()
    );
    monit.screen.type_keys("q");
    assert_eq!(monit.wait_exit(), 0);
    // the slave side is gone with the CLI; the master's settings are the pair's settings
    let after = termios_of(monit.screen.master_fd());
    assert!(
        after.c_lflag & libc::ICANON != 0 && after.c_lflag & libc::ECHO != 0,
        "the terminal is cooked again, not raw"
    );
}

#[test]
fn stop_asks_first_and_stops_only_after_y() {
    let env = Env::new();
    env.start_app("web", "sleep 300");
    let pid = env.wait_running("web");
    let mut monit = Monit::start(&env);
    assert!(
        monit.screen.wait_for_text("web", 30),
        "{}",
        monit.screen.text()
    );
    monit.screen.type_keys("s");
    assert!(
        monit.screen.wait_for_text("Stop 'web'? y/n", 10),
        "{}",
        monit.screen.text()
    );
    assert!(pid_alive(pid as i32), "the key alone stopped nothing");
    // no answers it
    monit.screen.type_keys("n");
    assert!(
        monit.screen.wait_for_text("cancelled", 10),
        "{}",
        monit.screen.text()
    );
    assert_eq!(
        env.app_state("web").map(|s| s.0).as_deref(),
        Some("running")
    );
    // yes does it
    monit.screen.type_keys("s");
    assert!(monit.screen.wait_for_text("Stop 'web'? y/n", 10));
    monit.screen.type_keys("y");
    assert!(
        monit.screen.wait_for_text("stopped 'web'", 10),
        "{}",
        monit.screen.text()
    );
    assert!(
        wait_for(20, || !pid_alive(pid as i32)),
        "the app was stopped"
    );
    assert_eq!(
        env.app_state("web").map(|s| s.0).as_deref(),
        Some("stopped")
    );
    monit.screen.type_keys("q");
    assert_eq!(monit.wait_exit(), 0);
}

#[test]
fn restart_asks_first_and_gives_the_app_a_new_process() {
    let env = Env::new();
    env.start_app("web", "sleep 300");
    let pid = env.wait_running("web");
    let mut monit = Monit::start(&env);
    assert!(monit.screen.wait_for_text("web", 30));
    monit.screen.type_keys("r");
    assert!(
        monit.screen.wait_for_text("Restart 'web'? y/n", 10),
        "{}",
        monit.screen.text()
    );
    assert!(pid_alive(pid as i32));
    monit.screen.type_keys("y");
    assert!(
        monit.screen.wait_for_text("restarted 'web'", 10),
        "{}",
        monit.screen.text()
    );
    assert!(
        wait_for(20, || env
            .app_state("web")
            .and_then(|s| s.1)
            .is_some_and(|p| p != pid)),
        "a new process"
    );
    monit.screen.type_keys("q");
    assert_eq!(monit.wait_exit(), 0);
}

#[test]
fn what_the_daemon_refuses_is_shown_in_the_status_line() {
    let env = Env::new();
    env.start_app("web", "sleep 300");
    let pid = env.wait_running("web");
    let marked = env.cli(&["policy", "set", "web", "--protected"]);
    assert_eq!(marked.code, 0, "{}", marked.all());
    let mut monit = Monit::start(&env);
    assert!(monit.screen.wait_for_text("web", 30));
    monit.screen.type_keys("s");
    assert!(monit.screen.wait_for_text("Stop 'web'? y/n", 10));
    monit.screen.type_keys("y");
    assert!(
        monit.screen.wait_for_text("E_PROTECTED", 10),
        "the refusal is on screen:\n{}",
        monit.screen.text()
    );
    assert!(
        monit.screen.text().contains("stop 'web'"),
        "{}",
        monit.screen.text()
    );
    assert!(
        pid_alive(pid as i32),
        "a protected app is not stopped by the dashboard"
    );
    monit.screen.type_keys("q");
    assert_eq!(monit.wait_exit(), 0);
}

#[test]
fn the_dashboard_acts_as_the_adapter_and_the_audit_log_says_so() {
    let env = Env::new();
    env.start_app("web", "sleep 300");
    let pid = env.wait_running("web");
    let mut monit = Monit::start(&env);
    assert!(monit.screen.wait_for_text("web", 30));
    monit.screen.type_keys("s");
    assert!(monit.screen.wait_for_text("Stop 'web'? y/n", 10));
    monit.screen.type_keys("y");
    assert!(wait_for(20, || !pid_alive(pid as i32)));
    let audit = env.audit_text();
    let line = audit
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|l| l["op"] == "stop" && l["actor"]["label"] == "ui-tui")
        .unwrap_or_else(|| panic!("a stop by the adapter ui-tui in\n{audit}"));
    assert_eq!(line["actor"]["class"], "adapter", "{line}");
    assert_eq!(
        line["actor"]["via"], "adapter_session",
        "the channel is a session: {line}"
    );
    assert_eq!(line["decision"], "allow", "{line}");
    monit.screen.type_keys("q");
    assert_eq!(monit.wait_exit(), 0);
}

#[test]
fn the_list_follows_the_daemon_without_a_key() {
    let env = Env::new();
    let mut monit = Monit::start(&env);
    assert!(monit.screen.wait_for_text("OxidePM Monitor", 30));
    assert!(!monit.screen.text().contains("late"));
    env.start_app("late", "sleep 300");
    assert!(
        monit.screen.wait_for_text("late", 10),
        "the next tick shows the new app:\n{}",
        monit.screen.text()
    );
    monit.screen.type_keys("q");
    assert_eq!(monit.wait_exit(), 0);
}

#[test]
fn the_log_tab_follows_the_log_on_every_tick() {
    let env = Env::new();
    let script = "echo first; sleep 3; echo second; sleep 300";
    env.start_app("web", script);
    env.wait_running("web");
    let mut monit = Monit::start(&env);
    assert!(monit.screen.wait_for_text("web", 30));
    monit.screen.type_keys("l");
    assert!(
        monit.screen.wait_for_text("Logs - web", 10),
        "{}",
        monit.screen.text()
    );
    assert!(
        monit.screen.wait_for_text("first", 10),
        "{}",
        monit.screen.text()
    );
    // no key from here on: the line that the app writes later appears by itself
    assert!(
        monit.screen.wait_for_text("second", 15),
        "the log refreshes on the tick:\n{}",
        monit.screen.text()
    );
    monit.screen.type_keys("q");
    assert_eq!(monit.wait_exit(), 0);
}

#[test]
fn the_page_keys_scroll_the_log_and_end_shows_the_latest() {
    let env = Env::new();
    env.start_app(
        "web",
        "i=1; while [ $i -le 80 ]; do echo \"row $i\"; i=$((i+1)); done; sleep 300",
    );
    env.wait_running("web");
    let mut monit = Monit::start_with(&env, 100, 20, &[]);
    assert!(monit.screen.wait_for_text("web", 30));
    monit.screen.type_keys("l");
    assert!(
        monit.screen.wait_for_text("row 80", 10),
        "{}",
        monit.screen.text()
    );
    assert!(
        !monit.screen.text().contains("row 70 "),
        "only what fits shows"
    );
    monit.screen.type_keys("\x1b[5~"); // page up
    assert!(
        monit.screen.wait_for_text("scrolled", 10),
        "{}",
        monit.screen.text()
    );
    assert!(
        !monit.screen.text().contains("row 80"),
        "{}",
        monit.screen.text()
    );
    monit.screen.type_keys("\x1b[F"); // end
    assert!(
        monit.screen.wait_for_text("row 80", 10),
        "{}",
        monit.screen.text()
    );
    monit.screen.type_keys("q");
    assert_eq!(monit.wait_exit(), 0);
}

#[test]
fn what_an_app_prints_cannot_move_the_cursor_or_change_the_screen() {
    let env = Env::new();
    env.start_app(
        "web",
        "printf 'safe\\033[2J\\033[1;1Hgone\\033]0;title\\007 end\\n'; sleep 300",
    );
    env.wait_running("web");
    let mut monit = Monit::start(&env);
    assert!(monit.screen.wait_for_text("web", 30));
    monit.screen.type_keys("l");
    assert!(
        monit.screen.wait_for_text("safe", 10),
        "{}",
        monit.screen.text()
    );
    let screen = monit.screen.text();
    assert!(
        screen.contains("OxidePM Monitor"),
        "the screen was not cleared by the app:\n{screen}"
    );
    assert!(
        !monit.screen.raw_text().contains("\x1b]0;title"),
        "the title sequence never reaches the terminal"
    );
    monit.screen.type_keys("q");
    assert_eq!(monit.wait_exit(), 0);
}

#[test]
fn a_new_window_size_is_drawn() {
    let env = Env::new();
    let mut monit = Monit::start_with(&env, 80, 24, &[]);
    assert!(monit.screen.wait_for_text("OxidePM Monitor", 30));
    monit.screen.resize(100, 40);
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(monit.child.id() as i32),
        nix::sys::signal::Signal::SIGWINCH,
    )
    .unwrap();
    // the help bar is the last block: with a margin of one it stands on the third row from the bottom
    assert!(
        wait_for(15, || {
            let rows = monit.screen.rows();
            rows.get(37).is_some_and(|r| r.contains("q: Quit"))
        }),
        "the dashboard fills the new size:\n{}",
        monit.screen.text()
    );
    monit.screen.type_keys("q");
    assert_eq!(monit.wait_exit(), 0);
}

#[test]
fn a_very_small_terminal_does_not_stop_the_dashboard() {
    let env = Env::new();
    env.start_app("web", "echo hello; sleep 300");
    env.wait_running("web");
    // two rows are all the margin leaves nothing of: every pane is empty
    let mut monit = Monit::start_with(&env, 20, 2, &[]);
    // keys typed before the dashboard sets the terminal to raw mode are lost; it switches to the
    // alternate screen after that
    assert!(
        wait_for(30, || monit.screen.raw_text().contains("\x1b[?1049h")),
        "no dashboard: {}",
        monit.screen.raw_text()
    );
    monit.screen.type_keys("l");
    std::thread::sleep(std::time::Duration::from_millis(1500));
    assert!(
        monit.child.try_wait().unwrap().is_none(),
        "still running:\n{}",
        env.adapter_stderr()
    );
    assert!(
        env.adapter_stderr().is_empty(),
        "nothing went wrong in the adapter:\n{}",
        env.adapter_stderr()
    );
    monit.screen.type_keys("q");
    assert_eq!(monit.wait_exit(), 0, "{}", env.adapter_stderr());
}

#[test]
fn a_daemon_that_stops_ends_the_session_and_gives_the_terminal_back() {
    let mut env = Env::new();
    let mut monit = Monit::start(&env);
    assert!(monit.screen.wait_for_text("OxidePM Monitor", 30));
    env.shutdown_daemon();
    let code = monit.wait_exit();
    assert_ne!(code, 0, "the session did not end well");
    let after = termios_of(monit.screen.master_fd());
    assert!(
        after.c_lflag & libc::ICANON != 0 && after.c_lflag & libc::ECHO != 0,
        "the terminal is cooked again"
    );
}

#[test]
fn the_dashboard_without_a_terminal_says_so_and_exits_2() {
    let env = Env::new();
    let run = env.cli(&["monit"]);
    assert_eq!(run.code, 2, "{}", run.all());
    assert!(run.all().contains("needs a terminal"), "{}", run.all());
    assert!(
        wait_for(20, || env.adapter_pids().is_empty()),
        "nothing is left running"
    );
}

#[test]
fn the_dashboard_takes_no_arguments() {
    let env = Env::new();
    let run = env.cli(&["monit", "--fast"]);
    assert_eq!(run.code, 2, "{}", run.all());
    assert!(run.all().contains("takes no arguments"), "{}", run.all());
}

#[test]
fn an_ai_claim_cannot_open_the_dashboard() {
    let env = Env::new();
    env.start_app("web", "sleep 300");
    let pid = env.wait_running("web");
    let mut monit = Monit::start_with(&env, 100, 30, &[("OXIDEPM_ACTOR", "ai:agent")]);
    let code = monit.wait_exit();
    assert_ne!(code, 0);
    let said = format!("{}{}", monit.stderr(), monit.screen.text());
    assert!(
        said.contains("E_POLICY_DENIED") || said.contains("denied") || said.contains("human"),
        "{said}"
    );
    assert!(pid_alive(pid as i32));
    assert!(env.adapter_pids().is_empty());
}

#[test]
fn the_adapter_asks_for_the_lifecycle_level_and_nothing_else() {
    let env = Env::new();
    let info = env.adapter_info();
    assert_eq!(info["kind"], "tool", "{info}");
    let granted = &info["grant"]["host"];
    assert_eq!(granted["level"], json!("lifecycle"), "{info}");
    assert_eq!(granted["scope"], json!("all"), "{info}");
    assert_eq!(info["grant"]["network"]["mode"], "none", "{info}");
    assert_eq!(info["grant"]["spawn"]["mode"], "none", "{info}");
    let manifest: serde_json::Value = serde_json::from_str(&manifest_text()).unwrap();
    assert_eq!(manifest["permissions"]["network"]["mode"], "none");
    assert_eq!(manifest["permissions"]["spawn"]["mode"], "none");
    assert_eq!(manifest["provides"]["verbs"][0]["mode"], "session");
}
