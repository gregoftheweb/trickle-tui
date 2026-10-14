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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
    /// Cumulative bytes sent since the wrapped process started (not since
    /// the last poll) -- see trickle-overload.c's control-socket protocol
    /// doc comment.
    pub send_total: u32,
    pub recv_limit: u32,
    pub recv_rate: u32,
    pub recv_total: u32,
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
    /// no live view here. The caller is responsible for getting its own TUI
    /// out of the way (raw mode / alternate screen) before calling this,
    /// and restoring it after.
    ///
    /// Still connects to the control socket, just invisibly, in a
    /// background thread polling GET -- not for a live display, but so the
    /// returned cumulative totals (the last reading captured before the
    /// socket goes away) can be shown as a one-line summary once the child
    /// exits. `None` if the process never got far enough to open the
    /// socket, or exited before the first successful poll.
    pub fn launch_interactive(
        trickle_path: &str,
        upload_bps: u32,
        download_bps: u32,
        command: &str,
        args: &[String],
    ) -> std::io::Result<(ExitStatus, Option<Stats>)> {
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

        let mut child = cmd.spawn()?;
        let pid = child.id();

        // Lets the poller stop waiting for the socket to ever appear as
        // soon as we know the process is gone, rather than sitting out the
        // full SOCKET_WAIT_TIMEOUT on something that failed instantly (a
        // bad command, a typo) -- the GET-polling loop below this doesn't
        // need the same treatment, since a torn-down socket already fails
        // those reads/writes directly.
        let child_exited = Arc::new(AtomicBool::new(false));
        let poller = {
            let child_exited = Arc::clone(&child_exited);
            std::thread::spawn(move || poll_session_stats(pid, child_exited))
        };

        let status = child.wait()?;
        child_exited.store(true, Ordering::Relaxed);

        let final_stats = poller.join().unwrap_or(None);
        Ok((status, final_stats))
    }

    /// Same contract and shape as launch_interactive, for targets LD_PRELOAD
    /// can't touch (statically linked -- see ThrottleProxy's doc comment).
    /// Starts a ThrottleProxy under trickle, runs `command` as a plain
    /// process pointed at it via HTTPS_PROXY, and polls the *proxy's*
    /// control socket in the background instead of the target's own (it
    /// doesn't have one) -- everything else about the caller's experience
    /// is identical.
    pub fn launch_interactive_via_proxy(
        trickle_path: &str,
        proxy_bin_path: &str,
        upload_bps: u32,
        download_bps: u32,
        command: &str,
        args: &[String],
    ) -> std::io::Result<(ExitStatus, Option<Stats>)> {
        let (mut child, mut proxy) = start_via_proxy(
            trickle_path,
            proxy_bin_path,
            upload_bps,
            download_bps,
            command,
            args,
            true,
        )?;

        let proxy_pid = proxy.pid;
        let child_exited = Arc::new(AtomicBool::new(false));
        let poller = {
            let child_exited = Arc::clone(&child_exited);
            std::thread::spawn(move || poll_session_stats(proxy_pid, child_exited))
        };

        let status = child.wait()?;
        child_exited.store(true, Ordering::Relaxed);
        let final_stats = poller.join().unwrap_or(None);

        let _ = proxy.kill();
        Ok((status, final_stats))
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
        control_send_line(stream, line)
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

/// A local HTTP CONNECT proxy (see src/bin/throttle-proxy.rs), launched
/// under trickle the same way any other app is. Exists for targets LD_PRELOAD
/// can't touch at all -- statically linked binaries, confirmed (empirically,
/// by pointing HTTPS_PROXY at a dead port and watching codex hang instead of
/// going direct) to be the case for at least one real target this project
/// cares about. Routes the target's traffic through this process instead,
/// which *is* a normal dynamically-linked binary trickle can throttle like
/// anything else -- the target's own linkage stops mattering.
pub struct ThrottleProxy {
    child: Child,
    pub pid: u32,
    pub port: u16,
    socket_path: PathBuf,
}

/// How long to wait for the proxy to report its listening port before
/// giving up -- it's a tiny local process binding 127.0.0.1:0, this should
/// never take more than a few milliseconds in practice.
const PROXY_START_TIMEOUT: Duration = Duration::from_secs(5);

impl ThrottleProxy {
    /// `proxy_bin_path` is throttle-proxy's own path -- see
    /// `sibling_binary_path()`, which finds it next to trickle-tui's own
    /// executable (same cargo target dir, since it's a second `[[bin]]` in
    /// this same crate).
    pub fn start(
        trickle_path: &str,
        proxy_bin_path: &str,
        upload_bps: u32,
        download_bps: u32,
    ) -> std::io::Result<Self> {
        let overload_path = resolve_overload_path(trickle_path)?;

        let mut cmd = Command::new(trickle_path);
        cmd.env("TRICKLE_OVERLOAD_PATH", &overload_path)
            .arg("-s")
            .arg("-u")
            .arg((upload_bps / 1024).max(1).to_string())
            .arg("-d")
            .arg((download_bps / 1024).max(1).to_string())
            .arg(proxy_bin_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = cmd.spawn()?;
        let pid = child.id();
        let stdout = child.stdout.take().expect("just set to piped");

        // Reads throttle-proxy's one-line "LISTENING <port>" announcement
        // off a background thread, bounded by PROXY_START_TIMEOUT, rather
        // than blocking the caller directly on a read() that -- for a
        // process this simple -- should never actually hang, but shouldn't
        // be able to wedge the TUI if something's badly wrong either.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            let _ = tx.send(reader.read_line(&mut line).map(|_| line));
        });

        let port = match rx.recv_timeout(PROXY_START_TIMEOUT) {
            Ok(Ok(line)) => line
                .trim()
                .strip_prefix("LISTENING ")
                .and_then(|p| p.parse::<u16>().ok())
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("proxy's startup line didn't parse: {line:?}"),
                    )
                })?,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                let _ = child.kill();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "timed out waiting for proxy to report its listening port",
                ));
            }
        };

        // trickle_init() fires on the proxy's very first intercepted libc
        // call -- its own "LISTENING <port>" println! above, since every
        // bare write() is overloaded regardless of fd -- so this is
        // normally instant. Still, don't hand back a ThrottleProxy whose
        // control socket isn't actually there yet.
        let socket_path = PathBuf::from(format!("/tmp/trickle/{pid}.sock"));
        let wait_start = Instant::now();
        loop {
            if socket_path.exists() && UnixStream::connect(&socket_path).is_ok() {
                break;
            }
            if wait_start.elapsed() > PROXY_START_TIMEOUT {
                let _ = child.kill();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "proxy's control socket never came up",
                ));
            }
            std::thread::sleep(SOCKET_POLL_INTERVAL);
        }

        Ok(ThrottleProxy {
            child,
            pid,
            port,
            socket_path,
        })
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// One-shot GET against the proxy's control socket -- unlike Session,
    /// there's no persistent connection here, since callers only need an
    /// occasional snapshot (a live dashboard poll, or a final summary),
    /// not a tight polling loop.
    pub fn get_stats(&self) -> std::io::Result<Stats> {
        let mut stream = UnixStream::connect(&self.socket_path)?;
        control_get_stats(&mut stream)
    }

    pub fn kill(&mut self) -> std::io::Result<()> {
        self.child.kill()
    }
}

/// Spawns `command` as a plain (un-trickled) process with the proxy's URL
/// set as its HTTP(S)_PROXY -- lowercase and uppercase, since different
/// HTTP client libraries check different casings by convention. Trickling
/// the target itself here would be pointless (if it respects the proxy
/// setting at all, that's because its own HTTP client is doing the actual
/// socket work somewhere LD_PRELOAD can't see, same as the direct case) and
/// for a dynamically-linked target would double up the throttling, so this
/// is deliberately a bare spawn.
fn spawn_via_proxy(
    proxy: &ThrottleProxy,
    command: &str,
    args: &[String],
    interactive: bool,
) -> std::io::Result<Child> {
    let proxy_url = proxy.url();
    let mut cmd = Command::new(command);
    cmd.args(args)
        .env("HTTPS_PROXY", &proxy_url)
        .env("https_proxy", &proxy_url)
        .env("HTTP_PROXY", &proxy_url)
        .env("http_proxy", &proxy_url);

    if interactive {
        cmd.stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
    } else {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    }

    cmd.spawn()
}

/// Starts a ThrottleProxy and spawns `command` through it in one step,
/// cleaning up the proxy if the spawn itself fails. The shared setup behind
/// both launch_interactive_via_proxy (which also blocks on the child and
/// polls stats in the background) and dashboard mode's proxy path (which
/// instead hands the pieces straight to a StatsSource::Proxied and lets the
/// normal poll_running() loop handle it).
pub fn start_via_proxy(
    trickle_path: &str,
    proxy_bin_path: &str,
    upload_bps: u32,
    download_bps: u32,
    command: &str,
    args: &[String],
    interactive: bool,
) -> std::io::Result<(Child, ThrottleProxy)> {
    let mut proxy = ThrottleProxy::start(trickle_path, proxy_bin_path, upload_bps, download_bps)?;

    match spawn_via_proxy(&proxy, command, args, interactive) {
        Ok(child) => Ok((child, proxy)),
        Err(e) => {
            let _ = proxy.kill();
            Err(e)
        }
    }
}

/// Where a running session's stats and limit control actually come from:
/// either the target's own control socket (Direct, the LD_PRELOAD path), or
/// a ThrottleProxy's, standing in for a target that can't be trickled
/// directly. Unifies the two so the rest of the app (dashboard polling,
/// arrow-key limit adjustment) doesn't need to care which one it's talking
/// to.
pub enum StatsSource {
    Direct(Session),
    Proxied { child: Child, proxy: ThrottleProxy },
}

impl StatsSource {
    pub fn pid(&self) -> u32 {
        match self {
            StatsSource::Direct(s) => s.pid,
            StatsSource::Proxied { proxy, .. } => proxy.pid,
        }
    }

    pub fn has_exited(&mut self) -> std::io::Result<bool> {
        match self {
            StatsSource::Direct(s) => s.has_exited(),
            StatsSource::Proxied { child, .. } => Ok(child.try_wait()?.is_some()),
        }
    }

    pub fn get_stats(&mut self) -> std::io::Result<Stats> {
        match self {
            StatsSource::Direct(s) => s.get_stats(),
            StatsSource::Proxied { proxy, .. } => proxy.get_stats(),
        }
    }

    pub fn set_limit(&mut self, dir: Direction, bytes_per_sec: u32) -> std::io::Result<()> {
        match self {
            StatsSource::Direct(s) => s.set_limit(dir, bytes_per_sec),
            StatsSource::Proxied { proxy, .. } => {
                let mut stream = UnixStream::connect(&proxy.socket_path)?;
                let resp = control_send_line(
                    &mut stream,
                    &format!("SET {} {}", dir.as_proto_str(), bytes_per_sec),
                )?;
                if resp.trim_end() == "OK" {
                    Ok(())
                } else {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("proxy rejected SET: {}", resp.trim_end()),
                    ))
                }
            }
        }
    }

    /// Kills the target. For Proxied, also kills the proxy itself -- it has
    /// no reason to keep running once nothing's using it, and leaving it
    /// behind would just leak a process and a control socket.
    pub fn kill(&mut self) -> std::io::Result<()> {
        match self {
            StatsSource::Direct(s) => s.kill(),
            StatsSource::Proxied { child, proxy } => {
                let _ = child.kill();
                proxy.kill()
            }
        }
    }
}

/// Finds a binary expected to live alongside trickle-tui's own executable
/// in the same cargo target directory (debug or release, whichever this
/// process itself was built/run as) -- used to locate throttle-proxy
/// without the caller needing to know or configure a path for it.
pub fn sibling_binary_path(name: &str) -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let dir = exe.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "couldn't determine trickle-tui's own directory",
        )
    })?;
    Ok(dir.join(name))
}

/// Sends one line over an already-connected control socket and reads back
/// its response. The protocol's responses are all terminated by a line
/// that's either "OK" or starts with "ERR"; GET sends two data lines first.
fn control_send_line(stream: &mut UnixStream, line: &str) -> std::io::Result<String> {
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

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

fn control_get_stats(stream: &mut UnixStream) -> std::io::Result<Stats> {
    let resp = control_send_line(stream, "GET")?;
    parse_stats(&resp).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "malformed GET response")
    })
}

/// Background-thread body for Session::launch_interactive: connects to
/// pid's control socket once it appears, then polls GET until a request
/// fails (the socket's gone -- the process exited and its atexit handler
/// unlinked it) or `exited` is observed set first (the process never made
/// it far enough to open the socket at all). Returns the last Stats
/// successfully parsed, if any -- cumulative totals only climb, so that's
/// effectively the session total, just as of up to one poll interval
/// before the process actually exited.
fn poll_session_stats(pid: u32, exited: Arc<AtomicBool>) -> Option<Stats> {
    const STATS_POLL_INTERVAL: Duration = Duration::from_millis(500);
    let socket_path = PathBuf::from(format!("/tmp/trickle/{pid}.sock"));
    let start = Instant::now();

    let mut stream = loop {
        if socket_path.exists() {
            if let Ok(s) = UnixStream::connect(&socket_path) {
                break s;
            }
        }
        if exited.load(Ordering::Relaxed) || start.elapsed() > SOCKET_WAIT_TIMEOUT {
            return None;
        }
        std::thread::sleep(SOCKET_POLL_INTERVAL);
    };

    let mut last_stats = None;
    loop {
        match control_get_stats(&mut stream) {
            Ok(stats) => last_stats = Some(stats),
            Err(_) => break,
        }
        // For a Direct-mode target, its own control socket normally dies
        // with it, so the next GET above would already fail on its own.
        // But `pid` here can also be a ThrottleProxy's -- which stays alive
        // (and keeps answering GET perfectly happily) after the *target*
        // it was relaying for has exited -- so this loop needs its own way
        // to know to stop. Without this check it polls a perfectly healthy
        // socket forever, and the caller's poller.join() never returns.
        if exited.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(STATS_POLL_INTERVAL);
    }
    last_stats
}

fn parse_stats(resp: &str) -> Option<Stats> {
    let mut send_limit = None;
    let mut send_rate = None;
    let mut send_total = None;
    let mut recv_limit = None;
    let mut recv_rate = None;
    let mut recv_total = None;

    for line in resp.lines() {
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("SEND") => {
                send_limit = parts.next()?.parse().ok();
                send_rate = parts.next()?.parse().ok();
                send_total = parts.next()?.parse().ok();
            }
            Some("RECV") => {
                recv_limit = parts.next()?.parse().ok();
                recv_rate = parts.next()?.parse().ok();
                recv_total = parts.next()?.parse().ok();
            }
            _ => {}
        }
    }

    Some(Stats {
        send_limit: send_limit?,
        send_rate: send_rate?,
        send_total: send_total?,
        recv_limit: recv_limit?,
        recv_rate: recv_rate?,
        recv_total: recv_total?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_well_formed_get_response() {
        let resp = "SEND 51200 78526 9043200\nRECV 10240 231726 55924736\nOK\n";
        let stats = parse_stats(resp).expect("should parse");
        assert_eq!(stats.send_limit, 51200);
        assert_eq!(stats.send_rate, 78526);
        assert_eq!(stats.send_total, 9043200);
        assert_eq!(stats.recv_limit, 10240);
        assert_eq!(stats.recv_rate, 231726);
        assert_eq!(stats.recv_total, 55924736);
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

    /// Exercises launch_interactive's full path -- spawn, background
    /// control-socket polling, blocking wait, final totals -- against a
    /// real process. Same network-access caveat as the test above: needs a
    /// command that actually makes an intercepted call so trickle_init()
    /// (and so the control socket) ever fires. Not run by default: `cargo
    /// test -- --ignored`.
    #[test]
    #[ignore]
    fn launches_interactively_and_reports_totals() {
        let (status, stats) = Session::launch_interactive(
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
        .expect("failed to launch");

        assert!(status.success());
        let stats = stats.expect("expected final stats to have been captured");
        assert!(stats.recv_total > 0, "expected some bytes to have been received");
    }
}
