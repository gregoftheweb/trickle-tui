use std::collections::VecDeque;
use std::time::Instant;

use crate::trickle::{Stats, StatsSource};

/// How many samples to keep for the live sparklines. At ~1 poll/sec this is
/// a couple of minutes of history -- more than a terminal sparkline can
/// usefully show at once anyway (it truncates to the widget's width).
pub const HISTORY_LEN: usize = 120;

pub enum Screen {
    Launcher,
    Running,
}

/// How the target's traffic actually gets throttled. Direct (LD_PRELOAD)
/// only works on dynamically-linked targets -- nothing for a statically
/// linked one (no dynamic linker step for LD_PRELOAD to hook into at all;
/// confirmed empirically for codex, not a guess) to hook into. Proxy routes
/// the target through a local throttle-proxy instead, via HTTPS_PROXY --
/// works regardless of linkage, as long as the target's own HTTP client
/// respects that setting.
#[derive(PartialEq, Eq, Clone, Copy)]
pub enum ProxyRouting {
    Direct,
    Proxy,
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum LauncherField {
    Interactive,
    Routing,
    Command,
    UploadLimit,
    DownloadLimit,
    Timed,
    DurationMinutes,
}

impl LauncherField {
    fn next(self) -> Self {
        use LauncherField::*;
        match self {
            Interactive => Routing,
            Routing => Command,
            Command => UploadLimit,
            UploadLimit => DownloadLimit,
            DownloadLimit => Timed,
            Timed => {
                // DurationMinutes only makes sense when Timed is on; the UI
                // skips it when toggled off, handled in App::tab().
                DurationMinutes
            }
            DurationMinutes => Interactive,
        }
    }

    fn prev(self) -> Self {
        use LauncherField::*;
        match self {
            Interactive => DurationMinutes,
            Routing => Interactive,
            Command => Routing,
            UploadLimit => Command,
            DownloadLimit => UploadLimit,
            Timed => DownloadLimit,
            DurationMinutes => Timed,
        }
    }
}

pub struct LauncherForm {
    pub command: String,
    pub upload_limit_kbps: String,
    /// True once the user has typed into the upload field directly --
    /// stops the background speed-detection result (see
    /// App::upload_detect_rx) from clobbering a value they're actively
    /// editing.
    pub upload_limit_touched: bool,
    pub download_limit_kbps: String,
    /// Interactive programs (claude, codex, a shell) need the real
    /// terminal -- stdin to read from, stdout/stderr to write to -- so they
    /// get full control of it for their lifetime instead of a live
    /// dashboard, which can't coexist with that. Off by default: it's the
    /// right choice for fire-and-forget/background commands and GUI apps.
    pub interactive: bool,
    pub routing: ProxyRouting,
    pub timed: bool,
    pub duration_minutes: String,
    pub focus: LauncherField,
    pub error: Option<String>,
    pub status: Option<String>,
}

impl Default for LauncherForm {
    fn default() -> Self {
        LauncherForm {
            command: String::new(),
            upload_limit_kbps: "50".to_string(),
            upload_limit_touched: false,
            download_limit_kbps: "200".to_string(),
            interactive: false,
            routing: ProxyRouting::Direct,
            timed: false,
            duration_minutes: "30".to_string(),
            focus: LauncherField::Interactive,
            error: None,
            status: None,
        }
    }
}

impl LauncherForm {
    pub fn focused_field_mut(&mut self) -> &mut String {
        match self.focus {
            LauncherField::Command => &mut self.command,
            LauncherField::UploadLimit => &mut self.upload_limit_kbps,
            LauncherField::DownloadLimit => &mut self.download_limit_kbps,
            LauncherField::DurationMinutes => &mut self.duration_minutes,
            // Timed, Interactive and Routing are toggles, not text fields;
            // nothing to edit.
            LauncherField::Timed | LauncherField::Interactive | LauncherField::Routing => {
                // Safe dummy: Command is always a valid text target, and
                // callers only push/pop chars, which is a no-op UX-wise if
                // this ever got hit (it won't -- char input skips these).
                &mut self.command
            }
        }
    }

    pub fn tab(&mut self, forward: bool) {
        loop {
            self.focus = if forward {
                self.focus.next()
            } else {
                self.focus.prev()
            };
            // Skip DurationMinutes when not in timed mode.
            if self.focus == LauncherField::DurationMinutes && !self.timed {
                continue;
            }
            break;
        }
    }

    /// Validates the form and returns (upload_bps, download_bps, duration).
    pub fn validate(&mut self) -> Option<(u32, u32, Option<std::time::Duration>)> {
        self.error = None;

        if self.command.trim().is_empty() {
            self.error = Some("Enter a command to launch".to_string());
            return None;
        }

        let upload_kbps: u32 = match self.upload_limit_kbps.trim().parse() {
            Ok(v) if v > 0 => v,
            _ => {
                self.error = Some("Upload limit must be a positive number (KB/s)".to_string());
                return None;
            }
        };
        let download_kbps: u32 = match self.download_limit_kbps.trim().parse() {
            Ok(v) if v > 0 => v,
            _ => {
                self.error = Some("Download limit must be a positive number (KB/s)".to_string());
                return None;
            }
        };

        let duration = if self.timed {
            match self.duration_minutes.trim().parse::<u64>() {
                Ok(v) if v > 0 => Some(std::time::Duration::from_secs(v * 60)),
                _ => {
                    self.error = Some("Duration must be a positive number (minutes)".to_string());
                    return None;
                }
            }
        } else {
            None
        };

        Some((upload_kbps * 1024, download_kbps * 1024, duration))
    }
}

pub struct RunningState {
    pub source: StatsSource,
    pub command_label: String,
    pub send_history: VecDeque<u64>,
    pub recv_history: VecDeque<u64>,
    pub last_stats: Option<Stats>,
    pub last_poll: Instant,
    pub timer_end: Option<Instant>,
    pub status: Option<String>,
    pub exited: bool,
}

impl RunningState {
    pub fn new(source: StatsSource, command_label: String, timer_end: Option<Instant>) -> Self {
        RunningState {
            source,
            command_label,
            send_history: VecDeque::with_capacity(HISTORY_LEN),
            recv_history: VecDeque::with_capacity(HISTORY_LEN),
            last_stats: None,
            last_poll: Instant::now(),
            timer_end,
            status: None,
            exited: false,
        }
    }

    pub fn record(&mut self, stats: Stats) {
        self.send_history.push_back(stats.send_rate as u64);
        if self.send_history.len() > HISTORY_LEN {
            self.send_history.pop_front();
        }
        self.recv_history.push_back(stats.recv_rate as u64);
        if self.recv_history.len() > HISTORY_LEN {
            self.recv_history.pop_front();
        }
        self.last_stats = Some(stats);
        self.last_poll = Instant::now();
    }

    pub fn time_remaining(&self) -> Option<std::time::Duration> {
        self.timer_end.map(|end| end.saturating_duration_since(Instant::now()))
    }
}

pub struct App {
    pub screen: Screen,
    pub launcher: LauncherForm,
    pub running: Option<RunningState>,
    pub should_quit: bool,
    pub trickle_path: String,
    /// Set once at startup; None once its one message has been received
    /// and handled (see poll_upload_detection in main.rs).
    pub upload_detect_rx: Option<std::sync::mpsc::Receiver<Option<u32>>>,
}

impl App {
    pub fn new(trickle_path: String) -> Self {
        App {
            screen: Screen::Launcher,
            launcher: LauncherForm::default(),
            running: None,
            should_quit: false,
            trickle_path,
            // Started immediately so it's already done (or close) by the
            // time someone's read the launcher screen and started typing --
            // never adds a startup delay.
            upload_detect_rx: Some(crate::speedtest::detect_upload_kbps_suggestion()),
        }
    }
}
