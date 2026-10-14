mod app;
mod speedtest;
mod trickle;
mod ui;

use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::execute;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use app::{App, LauncherField, ProxyRouting, RunningState, Screen};
use trickle::{Direction, Session, StatsSource};

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

        poll_upload_detection(app);
    }
}

/// Checks (non-blocking) whether the background upload-speed detection
/// kicked off in App::new() has finished, and if so, applies its
/// suggestion -- unless the user's already typed their own value into that
/// field, in which case it's left alone.
fn poll_upload_detection(app: &mut App) {
    let Some(rx) = &app.upload_detect_rx else {
        return;
    };

    match rx.try_recv() {
        Ok(result) => {
            app.upload_detect_rx = None; // one-shot; done either way
            if app.launcher.upload_limit_touched {
                return;
            }
            match result {
                Some(kbps) => {
                    app.launcher.upload_limit_kbps = kbps.to_string();
                    app.launcher.status = Some(format!(
                        "Measured your upload speed -- suggesting {kbps} KB/s (70%, leaves headroom for other traffic)"
                    ));
                }
                None => {
                    app.launcher.status =
                        Some("Couldn't measure upload speed -- using default".to_string());
                }
            }
        }
        Err(std::sync::mpsc::TryRecvError::Empty) => {}
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            app.upload_detect_rx = None;
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
        KeyCode::Char(' ') if form.focus == LauncherField::Routing => {
            form.routing = match form.routing {
                ProxyRouting::Direct => ProxyRouting::Proxy,
                ProxyRouting::Proxy => ProxyRouting::Direct,
            };
        }
        KeyCode::Char(c) if is_text_field(form.focus) => {
            if form.focus == LauncherField::UploadLimit {
                form.upload_limit_touched = true;
            }
            form.focused_field_mut().push(c);
        }
        KeyCode::Backspace if is_text_field(form.focus) => {
            if form.focus == LauncherField::UploadLimit {
                form.upload_limit_touched = true;
            }
            form.focused_field_mut().pop();
        }
        KeyCode::Enter => try_launch(app, terminal),
        _ => {}
    }
}

/// Command/UploadLimit/DownloadLimit/DurationMinutes take typed text;
/// Timed/Interactive/Routing are toggles with no text to edit.
fn is_text_field(focus: LauncherField) -> bool {
    !matches!(
        focus,
        LauncherField::Timed | LauncherField::Interactive | LauncherField::Routing
    )
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

    let routing = app.launcher.routing;

    if app.launcher.interactive {
        try_launch_interactive(
            app,
            terminal,
            upload_bps,
            download_bps,
            &command,
            &args,
            command_label,
            routing,
        );
        return;
    }

    let source = match routing {
        ProxyRouting::Direct => {
            Session::launch(&app.trickle_path, upload_bps, download_bps, &command, &args).and_then(
                |mut session| {
                    // v1 simplification: this blocks the UI thread briefly
                    // (usually under a second -- trickle_control_start()
                    // runs as soon as the wrapped app makes its first libc
                    // call) while waiting for the control socket. Worth
                    // revisiting with a background thread if a
                    // slow-starting app makes this noticeably janky.
                    if let Err(e) = session.wait_for_control() {
                        let _ = session.kill();
                        return Err(e);
                    }
                    Ok(StatsSource::Direct(session))
                },
            )
        }
        ProxyRouting::Proxy => resolve_proxy_bin_path().and_then(|proxy_bin| {
            trickle::start_via_proxy(
                &app.trickle_path,
                &proxy_bin,
                upload_bps,
                download_bps,
                &command,
                &args,
                false,
            )
            .map(|(child, proxy)| StatsSource::Proxied { child, proxy })
        }),
    };

    match source {
        Ok(source) => {
            let timer_end = duration.map(|d| Instant::now() + d);
            app.running = Some(RunningState::new(source, command_label, timer_end));
            app.screen = Screen::Running;
            app.launcher = app::LauncherForm::default();
        }
        Err(e) => {
            app.launcher.error = Some(format!("Failed to launch: {e}"));
        }
    }
}

/// throttle-proxy lives alongside trickle-tui's own executable (cargo puts
/// every `[[bin]]` from the same crate in the same target dir).
fn resolve_proxy_bin_path() -> std::io::Result<String> {
    let path = trickle::sibling_binary_path("throttle-proxy")?;
    path.to_str().map(String::from).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "throttle-proxy's path isn't valid UTF-8",
        )
    })
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
    routing: ProxyRouting,
) {
    let proxy_bin = if routing == ProxyRouting::Proxy {
        match resolve_proxy_bin_path() {
            Ok(p) => Some(p),
            Err(e) => {
                app.launcher.error = Some(format!("Couldn't find throttle-proxy: {e}"));
                return;
            }
        }
    } else {
        None
    };

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
    let result = match routing {
        ProxyRouting::Direct => Session::launch_interactive(
            &app.trickle_path,
            upload_bps,
            download_bps,
            command,
            args,
        ),
        ProxyRouting::Proxy => Session::launch_interactive_via_proxy(
            &app.trickle_path,
            proxy_bin.as_deref().expect("resolved above when Proxy"),
            upload_bps,
            download_bps,
            command,
            args,
        ),
    };
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
                let _ = running.source.kill();
            }
            app.running = None;
            app.screen = Screen::Launcher;
        }
        KeyCode::Char('k') => {
            let _ = running.source.kill();
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

    match running.source.set_limit(dir, new_limit) {
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

    match running.source.has_exited() {
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
            let _ = running.source.set_limit(Direction::Send, EFFECTIVELY_UNLIMITED);
            let _ = running.source.set_limit(Direction::Recv, EFFECTIVELY_UNLIMITED);
            running.timer_end = None;
            running.status = Some("Timer expired -- limit lifted.".to_string());
        }
    }

    if let Ok(stats) = running.source.get_stats() {
        running.record(stats);
    }
}
