//! Drawing. Every size is computed with saturating arithmetic: a terminal can be any size, down
//! to nothing, and the dashboard must draw something (or nothing) rather than panic.

use oxidepm_core::AppStatus;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Table, Tabs},
    Frame,
};

use crate::app::{clean, App, Note, Tab};

pub fn ui(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(3), // tabs
            Constraint::Min(3),    // main content
            Constraint::Length(3), // status line and help
        ])
        .split(f.area());

    let titles = vec!["Processes", "Details", "Logs"];
    let tabs = Tabs::new(titles)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("OxidePM Monitor"),
        )
        .select(app.tab.index())
        .style(Style::default().fg(Color::White))
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    f.render_widget(tabs, chunks[0]);

    match app.tab {
        Tab::Processes => render_processes(f, app, chunks[1]),
        Tab::Details => render_details(f, app, chunks[1]),
        Tab::Logs => render_logs(f, app, chunks[1]),
    }

    let (text, style) = bottom_line(app);
    let bar = Paragraph::new(text)
        .style(style)
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(bar, chunks[2]);
}

/// What the bottom line says: the question being asked, else the result of the last action, else
/// the last problem talking to the daemon, else the keys.
pub fn bottom_line(app: &App) -> (String, Style) {
    if let Some(confirm) = &app.confirm {
        let text = format!("{} '{}'? y/n", capitalised(confirm.action), confirm.name);
        return (
            text,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    }
    match &app.note {
        Some(Note::Info(text)) => return (text.clone(), Style::default().fg(Color::Green)),
        Some(Note::Error(text)) => return (text.clone(), Style::default().fg(Color::Red)),
        None => {}
    }
    if let Some(error) = &app.link_error {
        return (error.clone(), Style::default().fg(Color::Red));
    }
    let help = match app.tab {
        Tab::Processes => "↑/↓: Select | s: Stop | r: Restart | l: Logs | Tab: Switch | q: Quit",
        Tab::Details => "↑/↓: Select | s: Stop | r: Restart | Tab: Switch | q: Quit",
        Tab::Logs => "↑/↓ PgUp/PgDn: Scroll | End: Latest | Tab: Switch | q: Quit",
    };
    (help.to_string(), Style::default().fg(Color::DarkGray))
}

fn capitalised(action: crate::app::Action) -> &'static str {
    match action {
        crate::app::Action::Stop => "Stop",
        crate::app::Action::Restart => "Restart",
    }
}

fn render_processes(f: &mut Frame, app: &App, area: Rect) {
    let header_cells = [
        "ID", "Name", "Runtime", "PID", "↺", "Status", "CPU", "Mem", "Uptime", "Port",
    ]
    .iter()
    .map(|h| {
        Cell::from(*h).style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    });
    let header = Row::new(header_cells).height(1).bottom_margin(1);

    let rows: Vec<Row> = app
        .processes
        .iter()
        .enumerate()
        .map(|(i, info)| {
            let status_color = match info.state.status {
                AppStatus::Running => Color::Green,
                AppStatus::Stopped | AppStatus::Errored | AppStatus::Unhealthy => Color::Red,
                AppStatus::Starting
                | AppStatus::Building
                | AppStatus::Waiting
                | AppStatus::Backoff
                | AppStatus::Stopping => Color::Yellow,
            };
            let cells = vec![
                Cell::from(info.spec.id.to_string()),
                Cell::from(clean(&info.spec.name)),
                Cell::from(clean(&info.spec.mode.to_string())),
                Cell::from(
                    info.state
                        .pid
                        .map(|p| p.to_string())
                        .unwrap_or_else(|| "-".to_string()),
                ),
                Cell::from(info.state.restarts.to_string()),
                Cell::from(info.state.status.as_str()).style(Style::default().fg(status_color)),
                Cell::from(format!("{:.1}%", info.state.cpu_percent)),
                Cell::from(format_bytes(info.state.memory_bytes)),
                Cell::from(format_duration(info.state.uptime_secs)),
                Cell::from(
                    info.state
                        .port
                        .map(|p| p.to_string())
                        .unwrap_or_else(|| "-".to_string()),
                ),
            ];
            let style = if i == app.selected {
                Style::default().bg(Color::DarkGray)
            } else {
                Style::default()
            };
            Row::new(cells).style(style)
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(4),  // ID
            Constraint::Min(15),    // Name
            Constraint::Length(7),  // Runtime
            Constraint::Length(7),  // PID
            Constraint::Length(3),  // Restarts
            Constraint::Length(10), // Status
            Constraint::Length(7),  // CPU
            Constraint::Length(8),  // Mem
            Constraint::Length(8),  // Uptime
            Constraint::Length(6),  // Port
        ],
    )
    .header(header)
    .block(Block::default().borders(Borders::ALL).title("Processes"));
    f.render_widget(table, area);
}

fn render_details(f: &mut Frame, app: &App, area: Rect) {
    let Some(info) = app.selected_app() else {
        let paragraph = Paragraph::new("No processes")
            .block(Block::default().borders(Borders::ALL).title("Details"));
        f.render_widget(paragraph, area);
        return;
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8), // info
            Constraint::Length(4), // CPU gauge
            Constraint::Length(4), // memory gauge
            Constraint::Min(3),    // environment
        ])
        .split(area);

    let info_text = vec![
        Line::from(vec![
            Span::raw("Name: "),
            Span::styled(clean(&info.spec.name), Style::default().fg(Color::Cyan)),
        ]),
        Line::from(vec![
            Span::raw("ID: "),
            Span::styled(info.spec.id.to_string(), Style::default().fg(Color::Yellow)),
        ]),
        Line::from(vec![
            Span::raw("Runtime: "),
            Span::raw(clean(&info.spec.mode.to_string())),
        ]),
        Line::from(vec![
            Span::raw("Command: "),
            Span::raw(clean(&info.spec.command)),
        ]),
        Line::from(vec![
            Span::raw("CWD: "),
            Span::raw(clean(&info.spec.cwd.display().to_string())),
        ]),
        Line::from(vec![
            Span::raw("Instances: "),
            Span::raw(info.spec.instances.to_string()),
        ]),
    ];
    f.render_widget(
        Paragraph::new(info_text).block(Block::default().borders(Borders::ALL).title("Info")),
        chunks[0],
    );

    let cpu = info.state.cpu_percent;
    let cpu_gauge = Gauge::default()
        .block(Block::default().borders(Borders::ALL).title("CPU"))
        .gauge_style(Style::default().fg(Color::Green))
        .percent(if cpu.is_finite() {
            cpu.clamp(0.0, 100.0) as u16
        } else {
            0
        })
        .label(format!("{cpu:.1}%"));
    f.render_widget(cpu_gauge, chunks[1]);

    // against one gibibyte, as a rough scale
    let mem_percent =
        ((info.state.memory_bytes as f64 / (1024.0 * 1024.0 * 1024.0)) * 100.0).min(100.0) as u16;
    let mem_gauge = Gauge::default()
        .block(Block::default().borders(Borders::ALL).title("Memory"))
        .gauge_style(Style::default().fg(Color::Blue))
        .percent(mem_percent)
        .label(format_bytes(info.state.memory_bytes));
    f.render_widget(mem_gauge, chunks[2]);

    // the daemon shows environment values as a marker; the names are what is useful here
    let mut env: Vec<(&String, &String)> = info.spec.env.iter().collect();
    env.sort();
    let lines: Vec<Line> = env
        .into_iter()
        .take(10)
        .map(|(k, v)| Line::from(clean(&format!("{k}={v}"))))
        .collect();
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("Environment")),
        chunks[3],
    );
}

/// The lines of `logs` that fit in `height` rows when the view is `scroll` lines above the newest.
pub fn log_window(logs: &[String], height: usize, scroll: usize) -> &[String] {
    let scroll = scroll.min(logs.len().saturating_sub(height));
    let end = logs.len().saturating_sub(scroll);
    let start = end.saturating_sub(height);
    &logs[start..end]
}

fn render_logs(f: &mut Frame, app: &App, area: Rect) {
    let rows = usize::from(area.height.saturating_sub(2));
    let lines: Vec<Line> = log_window(&app.logs, rows, app.log_scroll)
        .iter()
        .map(|line| Line::from(line.as_str()))
        .collect();

    let mut title = match app.selected_app() {
        Some(info) => format!("Logs - {}", clean(&info.spec.name)),
        None => "Logs".to_string(),
    };
    if app.log_scroll > 0 {
        title.push_str(" (scrolled; End shows the latest)");
    }
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
}

pub fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1}G", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1}M", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.0}K", bytes as f64 / 1024.0)
    } else {
        format!("{bytes}B")
    }
}

pub fn format_duration(secs: u64) -> String {
    if secs >= 86400 {
        format!("{}d", secs / 86400)
    } else if secs >= 3600 {
        format!("{}h", secs / 3600)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Action, Confirm};
    use crate::testkit::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn draw(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| ui(f, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn busy_app(tab: Tab) -> App {
        let mut app = App::new();
        app.processes = vec![app_info(7, "web"), app_info(8, "api")];
        app.logs = (0..50).map(|i| format!("line {i}")).collect();
        app.tab = tab;
        app
    }

    fn app_info(id: u32, name: &str) -> oxidepm_core::AppInfo {
        let mut info = app(id, name);
        info.spec
            .env
            .insert("TOKEN".to_string(), "<redacted>".to_string());
        info
    }

    #[test]
    fn the_process_table_keeps_its_layout_and_colours_after_resize() {
        let app = busy_app(Tab::Processes);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| ui(frame, &app)).unwrap();
        terminal.backend_mut().resize(40, 14);
        terminal.draw(|frame| ui(frame, &app)).unwrap();

        let buffer = terminal.backend().buffer();
        let (rows, remainder) = buffer.content().as_chunks::<40>();
        assert!(remainder.is_empty());
        let rows: Vec<String> = rows
            .iter()
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect();
        assert_eq!(
            rows.iter().map(|row| row.trim_end()).collect::<Vec<_>>(),
            [
                "",
                " ┌OxidePM Monitor─────────────────────┐",
                " │ Processes │ Details │ Logs         │",
                " └────────────────────────────────────┘",
                " ┌Processes───────────────────────────┐",
                " │ID   Name            R P ↺ S C M U P│",
                " │                                    │",
                " │7    web             c 1 0 r 0 0 0 -│",
                " │8    api             c 1 0 r 0 0 0 -│",
                " └────────────────────────────────────┘",
                " ┌────────────────────────────────────┐",
                " │↑/↓: Select | s: Stop | r: Restart |│",
                " └────────────────────────────────────┘",
                "",
            ]
        );
        assert_eq!(buffer[(3, 2)].fg, Color::Yellow);
        assert_eq!(buffer[(3, 2)].modifier, Modifier::BOLD);
        assert_eq!(buffer[(29, 7)].fg, Color::Green);
        assert_eq!(buffer[(7, 7)].bg, Color::DarkGray);
        assert_eq!(buffer[(7, 8)].bg, Color::Reset);
        assert_eq!(buffer[(2, 11)].fg, Color::DarkGray);
    }

    #[test]
    fn the_logs_tab_draws_in_panes_of_every_small_height_without_panicking() {
        for height in 0..=12u16 {
            for width in [0u16, 1, 2, 5, 20, 80] {
                let app = busy_app(Tab::Logs);
                let _ = draw(&app, width, height);
            }
        }
    }

    #[test]
    fn every_tab_draws_at_every_small_size_without_panicking() {
        for tab in Tab::ALL {
            for height in 0..=14u16 {
                for width in [0u16, 1, 3, 10, 40, 120] {
                    let mut app = busy_app(tab);
                    let _ = draw(&app, width, height);
                    app.processes.clear();
                    app.logs.clear();
                    let _ = draw(&app, width, height);
                }
            }
        }
    }

    #[test]
    fn the_newest_lines_show_and_scrolling_moves_the_window_back() {
        let logs: Vec<String> = (0..10).map(|i| i.to_string()).collect();
        assert_eq!(log_window(&logs, 3, 0), ["7", "8", "9"]);
        assert_eq!(log_window(&logs, 3, 2), ["5", "6", "7"]);
        assert_eq!(
            log_window(&logs, 3, 100),
            ["0", "1", "2"],
            "never past the oldest"
        );
        assert_eq!(
            log_window(&logs, 20, 5),
            logs.as_slice(),
            "all of it when it fits"
        );
        assert!(log_window(&logs, 0, 0).is_empty());
        assert!(log_window(&[], 5, 3).is_empty());
    }

    #[test]
    fn the_logs_tab_shows_the_newest_lines_and_the_scrolled_ones_on_request() {
        let mut app = busy_app(Tab::Logs);
        let screen = draw(&app, 60, 14);
        assert!(
            screen.contains("line 49") && !screen.contains("line 0 "),
            "{screen}"
        );
        assert!(screen.contains("Logs - web"));
        app.log_scroll = 40;
        let screen = draw(&app, 60, 14);
        assert!(
            screen.contains("line 9") && !screen.contains("line 49"),
            "{screen}"
        );
        assert!(screen.contains("scrolled"));
    }

    #[test]
    fn the_question_and_the_errors_take_the_bottom_line() {
        let mut app = busy_app(Tab::Processes);
        assert!(bottom_line(&app).0.contains("s: Stop"));
        app.link_error = Some("daemon: the daemon is gone".to_string());
        assert_eq!(bottom_line(&app).0, "daemon: the daemon is gone");
        app.note = Some(Note::Info("stopped 'web'".to_string()));
        assert_eq!(
            bottom_line(&app).0,
            "stopped 'web'",
            "the last action comes before an old problem"
        );
        app.confirm = Some(Confirm {
            action: Action::Restart,
            id: 7,
            name: "web".to_string(),
        });
        assert_eq!(bottom_line(&app).0, "Restart 'web'? y/n");
        let screen = draw(&app, 80, 20);
        assert!(screen.contains("Restart 'web'? y/n"), "{screen}");
    }

    #[test]
    fn the_details_show_environment_names_with_the_daemons_marker_and_never_a_value() {
        let app = busy_app(Tab::Details);
        let screen = draw(&app, 80, 30);
        assert!(screen.contains("TOKEN=<redacted>"), "{screen}");
        assert!(screen.contains("Runtime:"));
    }

    #[test]
    fn sizes_and_times_read_as_before() {
        assert_eq!(format_bytes(500), "500B");
        assert_eq!(format_bytes(2048), "2K");
        assert_eq!(format_bytes(1_500_000), "1.4M");
        assert_eq!(format_bytes(2_000_000_000), "1.9G");
        assert_eq!(format_duration(30), "30s");
        assert_eq!(format_duration(90), "1m");
        assert_eq!(format_duration(3700), "1h");
        assert_eq!(format_duration(90000), "1d");
    }
}
