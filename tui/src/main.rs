mod app;
mod trickle;
mod ui;

use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::execute;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use app::{App, LauncherField, RunningState, Screen};
use trickle::{Direction, Session};

const POLL_INTERVAL: Duration = Duration::from_millis(1000);
const EVENT_POLL_TIMEOUT: Duration = Duration::from_millis(100);

fn main() -> std::io::Result<()> {
    let trickle_path = std::env::var("TRICKLE_TUI_TRICKLE_BIN")
        .unwrap_or_else(|_| "../trickle".to_string());

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(trickle_path);
    let result = run(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn run(terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>, app: &mut App) -> std::io::Result<()> {
    let mut last_poll = Instant::now();

    loop {
        terminal.draw(|frame| ui::draw(frame, app))?;

        if event::poll(EVENT_POLL_TIMEOUT)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    handle_key(app, key.code, key.modifiers, terminal);
                }
            }
        }

        if app.should_quit {
            return Ok(());
        }

        if last_poll.elapsed() >= POLL_INTERVAL {
            poll_running(app);
            last_poll = Instant::now();
        }
    }
}

fn handle_key(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
) {
    if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
        app.should_quit = true;
        return;
    }

    match app.screen {
        Screen::Launcher => handle_launcher_key(app, code, terminal),
        Screen::Running => handle_running_key(app, code),
    }
}

fn handle_launcher_key(
    app: &mut App,
    code: KeyCode,
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
) {
    let form = &mut app.launcher;
    match code {
        KeyCode::Esc => app.should_quit = true,
        KeyCode::Tab => form.tab(true),
        KeyCode::BackTab => form.tab(false),
        KeyCode::Char(' ') if form.focus == LauncherField::Timed => {
            form.timed = !form.timed;
        }
        KeyCode::Char(' ') if form.focus == LauncherField::Interactive => {
            form.interactive = !form.interactive;
        }
        KeyCode::Char(c)
            if form.focus != LauncherField::Timed && form.focus != LauncherField::Interactive =>
        {
            form.focused_field_mut().push(c);
        }
        KeyCode::Backspace
            if form.focus != LauncherField::Timed && form.focus != LauncherField::Interactive =>
        {
            form.focused_field_mut().pop();
        }
        KeyCode::Enter => try_launch(app, terminal),
        _ => {}
    }
}

fn try_launch(app: &mut App, terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>) {
    let Some((upload_bps, download_bps, duration)) = app.launcher.validate() else {
        return;
    };

    // Split "command arg1 arg2" naively on whitespace. Good enough for v1;
    // doesn't handle quoted args with spaces.
    let mut parts = app.launcher.command.split_whitespace();
    let Some(command) = parts.next().map(String::from) else {
        app.launcher.error = Some("Enter a command to launch".to_string());
        return;
    };
    let args: Vec<String> = parts.map(String::from).collect();
    let command_label = app.launcher.command.clone();

    app.launcher.error = None;
    app.launcher.status = None;

    if app.launcher.interactive {
        try_launch_interactive(
            app,
            terminal,
            upload_bps,
            download_bps,
            &command,
            &args,
            command_label,
        );
        return;
    }

    match Session::launch(&app.trickle_path, upload_bps, download_bps, &command, &args) {
        Ok(mut session) => {
            // v1 simplification: this blocks the UI thread briefly (usually
            // under a second -- trickle_control_start() runs as soon as the
            // wrapped app makes its first libc call) while waiting for the
            // control socket. Worth revisiting with a background thread if
            // a slow-starting app makes this noticeably janky in practice.
            if let Err(e) = session.wait_for_control() {
                app.launcher.error = Some(format!("Launched but couldn't connect: {e}"));
                let _ = session.kill();
                return;
            }

            let timer_end = duration.map(|d| Instant::now() + d);
            app.running = Some(RunningState::new(session, command_label, timer_end));
            app.screen = Screen::Running;
            app.launcher = app::LauncherForm::default();
        }
        Err(e) => {
            app.launcher.error = Some(format!("Failed to launch: {e}"));
        }
    }
}

/// Interactive programs (claude, codex, a shell) need the real terminal, and
/// can't share it with a live dashboard at the same time -- so this hands
/// our terminal to the child entirely: leaves the alternate screen and raw
/// mode, blocks until the child exits, then takes the screen back. No live
/// stats or in-flight limit adjustment during this -- the bandwidth shaping
/// itself is still fully in effect regardless, just not visible here.
fn try_launch_interactive(
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    upload_bps: u32,
    download_bps: u32,
    command: &str,
    args: &[String],
    command_label: String,
) {
    if let Err(e) = disable_raw_mode() {
        app.launcher.error = Some(format!("Couldn't hand off terminal: {e}"));
        return;
    }
    if let Err(e) = execute!(terminal.backend_mut(), LeaveAlternateScreen) {
        let _ = enable_raw_mode();
        app.launcher.error = Some(format!("Couldn't hand off terminal: {e}"));
        return;
    }

    let start = Instant::now();
    let result =
        Session::launch_interactive(&app.trickle_path, upload_bps, download_bps, command, args);
    let elapsed = start.elapsed();

    // Always try to restore our own screen, whether the launch succeeded or
    // not, so the TUI isn't left stranded in normal-terminal mode.
    let _ = execute!(terminal.backend_mut(), EnterAlternateScreen);
    let _ = enable_raw_mode();
    let _ = terminal.clear();

    match result {
        Ok((status, stats)) => {
            let code = status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "killed by signal".to_string());
            let totals = match stats {
                Some(s) => format!(
                    " -- sent {}, received {}",
                    ui::fmt_bytes(s.send_total),
                    ui::fmt_bytes(s.recv_total)
                ),
                None => String::new(),
            };
            app.launcher.status = Some(format!(
                "{command_label} exited ({code}) after {}{totals}",
                fmt_elapsed(elapsed)
            ));
        }
        Err(e) => {
            app.launcher.error = Some(format!("Failed to launch {command_label}: {e}"));
        }
    }
}

fn fmt_elapsed(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

fn handle_running_key(app: &mut App, code: KeyCode) {
    let Some(running) = &mut app.running else {
        app.screen = Screen::Launcher;
        return;
    };

    match code {
        KeyCode::Char('q') | KeyCode::Esc => {
            if !running.exited {
                let _ = running.session.kill();
            }
            app.running = None;
            app.screen = Screen::Launcher;
        }
        KeyCode::Char('k') => {
            let _ = running.session.kill();
            running.status = Some("Killed.".to_string());
        }
        KeyCode::Up => adjust_limit(running, Direction::Send, 1.10),
        KeyCode::Down => adjust_limit(running, Direction::Send, 0.90),
        KeyCode::Right => adjust_limit(running, Direction::Recv, 1.10),
        KeyCode::Left => adjust_limit(running, Direction::Recv, 0.90),
        _ => {}
    }
}

fn adjust_limit(running: &mut RunningState, dir: Direction, factor: f64) {
    let Some(stats) = running.last_stats else {
        return;
    };
    let current = match dir {
        Direction::Send => stats.send_limit,
        Direction::Recv => stats.recv_limit,
    };
    let new_limit = ((current as f64 * factor).round() as u32).max(1024);

    match running.session.set_limit(dir, new_limit) {
        Ok(()) => {
            running.status = Some(format!(
                "{:?} limit -> {:.1} KB/s",
                dir,
                new_limit as f64 / 1024.0
            ));
        }
        Err(e) => {
            running.status = Some(format!("Failed to set limit: {e}"));
        }
    }
}

fn poll_running(app: &mut App) {
    let Some(running) = &mut app.running else {
        return;
    };

    match running.session.has_exited() {
        Ok(true) => {
            running.exited = true;
            running.status = Some("Process exited.".to_string());
            return;
        }
        Ok(false) => {}
        Err(_) => return,
    }

    if let Some(remaining) = running.time_remaining() {
        if remaining.is_zero() {
            // Timed control expired: lift the throttle rather than kill the
            // process. "Unlimited" isn't a real value in the protocol --
            // just set a limit far above anything the app can push.
            const EFFECTIVELY_UNLIMITED: u32 = 1024 * 1024 * 1024; // 1 GB/s
            let _ = running.session.set_limit(Direction::Send, EFFECTIVELY_UNLIMITED);
            let _ = running.session.set_limit(Direction::Recv, EFFECTIVELY_UNLIMITED);
            running.timer_end = None;
            running.status = Some("Timer expired -- limit lifted.".to_string());
        }
    }

    if let Ok(stats) = running.session.get_stats() {
        running.record(stats);
    }
}
