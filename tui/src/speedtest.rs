//! Measures real upload throughput via curl (rather than adding an HTTP/TLS
//! dependency to the crate just for this) and suggests a cap at 70% of it --
//! enough headroom that a heavy upload doesn't starve ACK traffic for
//! downloads sharing the same thin pipe, while still using most of it.
//!
//! Runs on a background thread, started once at App::new() so it's already
//! done (or close to it) by the time a user has read the launcher screen
//! and started typing a command -- never blocks startup.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::mpsc::Receiver;

/// How long we'll let curl run before giving up -- long enough for a slow
/// line to produce a real measurement, short enough not to leave the
/// suggestion hanging indefinitely if the network's down. Generous on
/// purpose: a thin line (the whole reason this feature exists) can take a
/// while just to push PAYLOAD_BYTES, and curl still reports an accurate
/// %{speed_upload} for a --max-time-aborted transfer, so there's no harm
/// in giving it room rather than cutting off a slow-but-real measurement.
const MAX_TIME_SECS: &str = "25";

/// Upload payload size. Big enough that curl's own connection-setup
/// overhead doesn't dominate the measurement (a tiny payload mostly times
/// TCP/TLS handshake, not real throughput), small enough to finish quickly
/// even on a slow line.
const PAYLOAD_BYTES: usize = 3_000_000;

const UPLOAD_TEST_URL: &str = "https://speed.cloudflare.com/__up";

/// Fraction of measured throughput to suggest as the cap.
const SUGGESTED_FRACTION: f64 = 0.70;

/// Starts the detection on a background thread; returns a receiver that
/// gets exactly one message once it's done -- Some(kbps) on a successful
/// measurement, None if curl's missing, the network's unreachable, or the
/// output didn't parse. Either way, the caller hears back and can fall
/// back to a hardcoded default.
pub fn detect_upload_kbps_suggestion() -> Receiver<Option<u32>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(measure_upload_kbps());
    });
    rx
}

fn measure_upload_kbps() -> Option<u32> {
    let mut child = Command::new("curl")
        .args([
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{speed_upload}",
            "--max-time",
            MAX_TIME_SECS,
            "-X",
            "POST",
            "--data-binary",
            "@-",
            UPLOAD_TEST_URL,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    // Feed the payload directly over the pipe -- no temp file, and the
    // actual byte values don't matter (nothing here compresses or inspects
    // them, it's a pure throughput measurement), so a zeroed buffer is
    // exactly as good as random data and far simpler to produce.
    let mut stdin = child.stdin.take()?;
    stdin.write_all(&vec![0u8; PAYLOAD_BYTES]).ok()?;
    drop(stdin); // closes the write end -- curl sees EOF and finishes up

    let output = child.wait_with_output().ok()?;
    // Deliberately not gating on output.status.success(): curl exits
    // non-zero if --max-time cuts the transfer off before the server's
    // final response, but it still reports an accurate %{speed_upload} for
    // whatever portion of the payload it actually got out the door --
    // confirmed empirically (a timed-out run still reported a real,
    // sane speed_upload value). A non-parsing or non-positive result below
    // is what actually distinguishes a real failure from a slow-but-valid
    // measurement.
    let bytes_per_sec: f64 = String::from_utf8_lossy(&output.stdout).trim().parse().ok()?;
    if bytes_per_sec <= 0.0 {
        return None;
    }

    let suggested_bytes_per_sec = bytes_per_sec * SUGGESTED_FRACTION;
    Some((suggested_bytes_per_sec / 1024.0).round().max(1.0) as u32)
}
