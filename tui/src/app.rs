use std::collections::VecDeque;
use std::time::Instant;

use crate::trickle::{Session, Stats};

/// How many samples to keep for the live sparklines. At ~1 poll/sec this is
/// a couple of minutes of history -- more than a terminal sparkline can
/// usefully show at once anyway (it truncates to the widget's width).
pub const HISTORY_LEN: usize = 120;

pub enum Screen {
    Launcher,
    Running,
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum LauncherField {
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
            Command => UploadLimit,
            UploadLimit => DownloadLimit,
            DownloadLimit => Timed,
            Timed => {
                // DurationMinutes only makes sense when Timed is on; the UI
                // skips it when toggled off, handled in App::tab().
                DurationMinutes
            }
            DurationMinutes => Command,
        }
    }

    fn prev(self) -> Self {
        use LauncherField::*;
        match self {
            Command => DurationMinutes,
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
    pub download_limit_kbps: String,
    pub timed: bool,
    pub duration_minutes: String,
    pub focus: LauncherField,
    pub error: Option<String>,
}

impl Default for LauncherForm {
    fn default() -> Self {
        LauncherForm {
            command: String::new(),
            upload_limit_kbps: "50".to_string(),
            download_limit_kbps: "200".to_string(),
            timed: false,
            duration_minutes: "30".to_string(),
            focus: LauncherField::Command,
            error: None,
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
            // Timed is a bool toggle, not a text field; nothing to edit.
            LauncherField::Timed => {
                // Safe dummy: Command is always a valid text target, and
                // callers only push/pop chars, which is a no-op UX-wise if
                // this ever got hit (it won't -- char input skips Timed).
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
    pub session: Session,
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
    pub fn new(session: Session, command_label: String, timer_end: Option<Instant>) -> Self {
        RunningState {
            session,
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
}

impl App {
    pub fn new(trickle_path: String) -> Self {
        App {
            screen: Screen::Launcher,
            launcher: LauncherForm::default(),
            running: None,
            should_quit: false,
            trickle_path,
        }
    }
}
