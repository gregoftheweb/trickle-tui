//! Spawns a command under standalone `trickle` and talks to its per-process
//! control socket (see trickle-overload.c's "Per-process control socket").
//!
//! trickle.c execvp()s the target command directly -- no fork -- so the
//! child process we spawn keeps the same PID all the way through into the
//! wrapped app. The control socket path is derived directly from that PID;
//! there's no discovery step.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// Resolves trickle-overload.so's absolute path, sitting alongside
/// trickle_path, rather than relying on trickle's own CWD-relative search
/// (which would otherwise require running the wrapped app from the
/// repo/install directory -- wrong for apps that care about their own
/// working directory, which is most of them). Requires the
/// TRICKLE_OVERLOAD_PATH support added to trickle.c alongside this.
fn resolve_overload_path(trickle_path: &str) -> std::io::Result<PathBuf> {
    let trickle_abs = std::fs::canonicalize(trickle_path)?;
    let overload_path = trickle_abs
        .parent()
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "couldn't determine trickle's directory",
            )
        })?
        .join("trickle-overload.so");
    if !overload_path.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("trickle-overload.so not found at {}", overload_path.display()),
        ));
    }
    Ok(overload_path)
}

#[derive(Debug, Clone, Copy)]
pub struct Stats {
    pub send_limit: u32,
    pub send_rate: u32,
    pub recv_limit: u32,
    pub recv_rate: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Send,
    Recv,
}

impl Direction {
    fn as_proto_str(self) -> &'static str {
        match self {
            Direction::Send => "SEND",
            Direction::Recv => "RECV",
        }
    }
}

pub struct Session {
    pub child: Child,
    pub pid: u32,
    socket_path: PathBuf,
    control: Option<UnixStream>,
}

/// How long to keep retrying for the control socket to appear after launch.
/// trickle_control_start() only runs once the wrapped app makes its first
/// intercepted libc call (first socket()/read()/write()/etc.), so for an
/// app that's slow to get going (module loading, etc.) the socket can take
/// a moment to show up.
const SOCKET_WAIT_TIMEOUT: Duration = Duration::from_secs(10);
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(100);

impl Session {
    /// Launches `command` (and its args) under standalone trickle with the
    /// given initial upload/download limits (bytes/sec). trickle_path
    /// should point at the `trickle` binary built alongside this crate
    /// (../trickle relative to the repo root in dev, or just "trickle" if
    /// it's on PATH once installed).
    pub fn launch(
        trickle_path: &str,
        upload_bps: u32,
        download_bps: u32,
        command: &str,
        args: &[String],
    ) -> std::io::Result<Self> {
        let overload_path = resolve_overload_path(trickle_path)?;

        let mut cmd = Command::new(trickle_path);
        cmd.env("TRICKLE_OVERLOAD_PATH", &overload_path)
            .arg("-s")
            .arg("-u")
            .arg((upload_bps / 1024).max(1).to_string()) // trickle's -u/-d are KB/s
            .arg("-d")
            .arg((download_bps / 1024).max(1).to_string())
            .arg(command)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let child = cmd.spawn()?;
        let pid = child.id();
        let socket_path = PathBuf::from(format!("/tmp/trickle/{pid}.sock"));

        Ok(Session {
            child,
            pid,
            socket_path,
            control: None,
        })
    }

    /// Launches `command` under standalone trickle with its stdin/stdout/
    /// stderr connected straight to ours (the caller's terminal), and blocks
    /// until it exits. For genuinely interactive programs (claude, codex, a
    /// shell) rather than fire-and-forget/background ones -- those can't
    /// share a terminal with a live dashboard at the same time, so there's
    /// no control-socket connection and no live stats here. The caller is
    /// responsible for getting its own TUI out of the way (raw mode /
    /// alternate screen) before calling this, and restoring it after.
    pub fn launch_interactive(
        trickle_path: &str,
        upload_bps: u32,
        download_bps: u32,
        command: &str,
        args: &[String],
    ) -> std::io::Result<ExitStatus> {
        let overload_path = resolve_overload_path(trickle_path)?;

        let mut cmd = Command::new(trickle_path);
        cmd.env("TRICKLE_OVERLOAD_PATH", &overload_path)
            .arg("-s")
            .arg("-u")
            .arg((upload_bps / 1024).max(1).to_string())
            .arg("-d")
            .arg((download_bps / 1024).max(1).to_string())
            .arg(command)
            .args(args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        cmd.spawn()?.wait()
    }

    /// True once the wrapped process has exited.
    pub fn has_exited(&mut self) -> std::io::Result<bool> {
        Ok(self.child.try_wait()?.is_some())
    }

    /// Blocks (up to SOCKET_WAIT_TIMEOUT) waiting for the control socket to
    /// appear and connecting to it. Call once, early, before polling stats.
    pub fn wait_for_control(&mut self) -> std::io::Result<()> {
        let start = Instant::now();
        loop {
            if self.socket_path.exists() {
                match UnixStream::connect(&self.socket_path) {
                    Ok(stream) => {
                        self.control = Some(stream);
                        return Ok(());
                    }
                    Err(_) => { /* socket file exists but not accepting yet; keep trying */
                    }
                }
            }

            if self.has_exited()? {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "process exited before its control socket appeared",
                ));
            }

            if start.elapsed() > SOCKET_WAIT_TIMEOUT {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "timed out waiting for control socket",
                ));
            }

            std::thread::sleep(SOCKET_POLL_INTERVAL);
        }
    }

    fn send_line(&mut self, line: &str) -> std::io::Result<String> {
        let stream = self.control.as_mut().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotConnected, "not connected")
        })?;

        stream.write_all(line.as_bytes())?;
        stream.write_all(b"\n")?;
        stream.flush()?;

        // The protocol's responses are all terminated by a line that's
        // either "OK" or starts with "ERR"; GET sends two data lines first.
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut out = String::new();
        loop {
            let mut buf = String::new();
            let n = reader.read_line(&mut buf)?;
            if n == 0 {
                break; // EOF
            }
            let trimmed = buf.trim_end();
            let is_terminal = trimmed == "OK" || trimmed.starts_with("ERR");
            out.push_str(trimmed);
            out.push('\n');
            if is_terminal {
                break;
            }
        }
        Ok(out)
    }

    pub fn get_stats(&mut self) -> std::io::Result<Stats> {
        let resp = self.send_line("GET")?;
        parse_stats(&resp).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "malformed GET response")
        })
    }

    pub fn set_limit(&mut self, dir: Direction, bytes_per_sec: u32) -> std::io::Result<()> {
        let resp = self.send_line(&format!("SET {} {}", dir.as_proto_str(), bytes_per_sec))?;
        if resp.trim_end() == "OK" {
            Ok(())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("trickle rejected SET: {}", resp.trim_end()),
            ))
        }
    }

    pub fn kill(&mut self) -> std::io::Result<()> {
        self.child.kill()
    }
}

fn parse_stats(resp: &str) -> Option<Stats> {
    let mut send_limit = None;
    let mut send_rate = None;
    let mut recv_limit = None;
    let mut recv_rate = None;

    for line in resp.lines() {
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("SEND") => {
                send_limit = parts.next()?.parse().ok();
                send_rate = parts.next()?.parse().ok();
            }
            Some("RECV") => {
                recv_limit = parts.next()?.parse().ok();
                recv_rate = parts.next()?.parse().ok();
            }
            _ => {}
        }
    }

    Some(Stats {
        send_limit: send_limit?,
        send_rate: send_rate?,
        recv_limit: recv_limit?,
        recv_rate: recv_rate?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_well_formed_get_response() {
        let resp = "SEND 51200 78526\nRECV 10240 231726\nOK\n";
        let stats = parse_stats(resp).expect("should parse");
        assert_eq!(stats.send_limit, 51200);
        assert_eq!(stats.send_rate, 78526);
        assert_eq!(stats.recv_limit, 10240);
        assert_eq!(stats.recv_rate, 231726);
    }

    #[test]
    fn rejects_a_truncated_response() {
        assert!(parse_stats("SEND 51200 78526\nOK\n").is_none());
    }

    /// Exercises the real launch -> connect -> GET -> SET -> kill path
    /// against a real spawned process and the real control socket --
    /// not just the response parser. Needs the sibling `trickle` C binary
    /// built (`../trickle` relative to this crate) and network access
    /// (curl against a real endpoint, to guarantee the wrapped process
    /// actually makes an intercepted socket/read/write call -- a command
    /// like plain `sleep` never would, since trickle_init() only fires
    /// from inside an overloaded libc call). Not run by default: `cargo
    /// test -- --ignored` to run it explicitly.
    #[test]
    #[ignore]
    fn launches_and_controls_a_real_process() {
        let mut session = Session::launch(
            "../trickle",
            50 * 1024,
            100 * 1024,
            "curl",
            &[
                "-s".to_string(),
                "-o".to_string(),
                "/dev/null".to_string(),
                "https://speed.cloudflare.com/__down?bytes=3000000".to_string(),
            ],
        )
        .expect("failed to spawn session");

        session
            .wait_for_control()
            .expect("control socket never came up");

        let stats = session.get_stats().expect("GET failed");
        assert_eq!(stats.send_limit, 50 * 1024);
        assert_eq!(stats.recv_limit, 100 * 1024);

        session
            .set_limit(Direction::Recv, 20 * 1024)
            .expect("SET failed");

        let stats = session.get_stats().expect("GET after SET failed");
        assert_eq!(stats.recv_limit, 20 * 1024);

        session.kill().expect("kill failed");
    }
}
