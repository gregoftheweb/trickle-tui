use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph, Sparkline};

use crate::app::{App, LauncherField, Screen};

pub fn draw(frame: &mut Frame, app: &App) {
    match app.screen {
        Screen::Launcher => draw_launcher(frame, app),
        Screen::Running => draw_running(frame, app),
    }
}

fn labeled_value(label: &str, value: &str, focused: bool) -> Paragraph<'static> {
    let style = if focused {
        Style::default().fg(Color::Black).bg(Color::Cyan)
    } else {
        Style::default()
    };
    Paragraph::new(format!("{value} ")).style(style).block(
        Block::default()
            .borders(Borders::ALL)
            .title(label.to_string()),
    )
}

fn draw_launcher(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // title
            Constraint::Length(3), // interactive toggle
            Constraint::Length(3), // command
            Constraint::Length(3), // upload
            Constraint::Length(3), // download
            Constraint::Length(3), // timed toggle
            Constraint::Length(3), // duration
            Constraint::Length(2), // error / status
            Constraint::Min(0),    // help
        ])
        .split(area);

    frame.render_widget(
        Paragraph::new("trickle-tui -- what app?").style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        chunks[0],
    );

    let f = &app.launcher;
    frame.render_widget(
        labeled_value(
            "Launch as? (space to toggle)",
            if f.interactive {
                "open an app (hands over the terminal, e.g. claude/codex)"
            } else {
                "run a hidden process (background dashboard, e.g. curl/vivaldi)"
            },
            f.focus == LauncherField::Interactive,
        ),
        chunks[1],
    );
    frame.render_widget(
        labeled_value(
            "Command (e.g. `claude`, `codex`, `vivaldi`)",
            &f.command,
            f.focus == LauncherField::Command,
        ),
        chunks[2],
    );
    frame.render_widget(
        labeled_value(
            "Upload limit (KB/s)",
            &f.upload_limit_kbps,
            f.focus == LauncherField::UploadLimit,
        ),
        chunks[3],
    );
    frame.render_widget(
        labeled_value(
            "Download limit (KB/s)",
            &f.download_limit_kbps,
            f.focus == LauncherField::DownloadLimit,
        ),
        chunks[4],
    );
    frame.render_widget(
        labeled_value(
            "Timed? (space to toggle)",
            if f.timed { "yes" } else { "no (permanent)" },
            f.focus == LauncherField::Timed,
        ),
        chunks[5],
    );
    if f.timed {
        frame.render_widget(
            labeled_value(
                "Duration (minutes)",
                &f.duration_minutes,
                f.focus == LauncherField::DurationMinutes,
            ),
            chunks[6],
        );
    }

    if let Some(err) = &f.error {
        frame.render_widget(
            Paragraph::new(err.as_str()).style(Style::default().fg(Color::Red)),
            chunks[7],
        );
    } else if let Some(status) = &f.status {
        frame.render_widget(
            Paragraph::new(status.as_str()).style(Style::default().fg(Color::Green)),
            chunks[7],
        );
    }

    frame.render_widget(
        Paragraph::new(
            "Tab/Shift+Tab: move  Space: toggle  Enter: launch  Esc/Ctrl+C: quit",
        )
        .style(Style::default().fg(Color::DarkGray)),
        chunks[8],
    );
}

fn sparkline_block<'a>(title: String, data: &'a [u64]) -> Sparkline<'a> {
    Sparkline::default()
        .block(Block::default().borders(Borders::ALL).title(title))
        .data(data)
        .style(Style::default().fg(Color::Green))
}

fn fmt_rate(bytes_per_sec: u32) -> String {
    if bytes_per_sec >= 1024 * 1024 {
        format!("{:.2} MB/s", bytes_per_sec as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} KB/s", bytes_per_sec as f64 / 1024.0)
    }
}

/// Formats an absolute byte count (a session total, not a rate).
pub(crate) fn fmt_bytes(bytes: u32) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.2} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn fmt_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    format!("{:02}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
}

fn draw_running(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let Some(running) = &app.running else {
        return;
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // title / pid / timer
            Constraint::Length(6), // upload sparkline
            Constraint::Length(6), // download sparkline
            Constraint::Length(2), // status
            Constraint::Min(0),    // help
        ])
        .split(area);

    let timer_label = match running.time_remaining() {
        Some(d) => format!(" -- expires in {}", fmt_duration(d)),
        None => " -- permanent".to_string(),
    };
    let exited_label = if running.exited { " [exited]" } else { "" };
    frame.render_widget(
        Paragraph::new(format!(
            "{}  (pid {}){}{}",
            running.command_label, running.session.pid, timer_label, exited_label
        ))
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        chunks[0],
    );

    let send_data: Vec<u64> = running.send_history.iter().copied().collect();
    let recv_data: Vec<u64> = running.recv_history.iter().copied().collect();

    let send_title = match running.last_stats {
        Some(s) => format!(
            "Upload -- {} (limit {}, total {})",
            fmt_rate(s.send_rate),
            fmt_rate(s.send_limit),
            fmt_bytes(s.send_total)
        ),
        None => "Upload".to_string(),
    };
    let recv_title = match running.last_stats {
        Some(s) => format!(
            "Download -- {} (limit {}, total {})",
            fmt_rate(s.recv_rate),
            fmt_rate(s.recv_limit),
            fmt_bytes(s.recv_total)
        ),
        None => "Download".to_string(),
    };

    frame.render_widget(sparkline_block(send_title, &send_data), chunks[1]);
    frame.render_widget(sparkline_block(recv_title, &recv_data), chunks[2]);

    if let Some(status) = &running.status {
        frame.render_widget(
            Paragraph::new(status.as_str()).style(Style::default().fg(Color::Yellow)),
            chunks[3],
        );
    }

    frame.render_widget(
        Paragraph::new(
            "Up/Down: upload \u{b1}10%  Left/Right: download \u{b1}10%  k: kill  q/Esc: back  Ctrl+C: quit",
        )
        .style(Style::default().fg(Color::DarkGray)),
        chunks[4],
    );
}
