//! Minimal local HTTP CONNECT proxy, built to be wrapped by trickle.
//!
//! Exists for one reason: some target apps (codex, confirmed by testing --
//! pointing HTTPS_PROXY at a dead port made it hang instead of going direct)
//! are statically linked, which makes them completely immune to trickle's
//! LD_PRELOAD-based interception -- there's no dynamic linker step for
//! LD_PRELOAD to hook into. But if the app respects HTTPS_PROXY/HTTP_PROXY
//! (an application-level config choice, read by the app's own HTTP client,
//! not something LD_PRELOAD is involved in at all), routing it through a
//! local proxy sidesteps the whole problem: this proxy process does the
//! real socket I/O to the outside world, *this* process is a normal
//! dynamically-linked Rust binary, and trickle can throttle it exactly the
//! same way it throttles curl or claude.
//!
//! Deliberately minimal: CONNECT-tunnel only (the only thing an HTTPS API
//! client needs), blind byte relay once established, no TLS termination or
//! request inspection. Prints "LISTENING <port>\n" to stdout as the very
//! first thing once bound, then flushes -- the launcher reads that one line
//! to learn which port the OS actually gave it (binds to port 0) before it
//! starts the real target app pointed at it.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};

fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let listener = TcpListener::bind(("127.0.0.1", port)).expect("failed to bind proxy listener");
    let actual_port = listener.local_addr().expect("no local addr").port();

    println!("LISTENING {actual_port}");
    std::io::stdout().flush().expect("failed to flush stdout");

    for conn in listener.incoming() {
        match conn {
            Ok(client) => {
                std::thread::spawn(move || {
                    let _ = handle_connection(client);
                });
            }
            Err(_) => continue,
        }
    }
}

fn handle_connection(client: TcpStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(client.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;

    // Drain the rest of the request headers up to the blank line -- we
    // don't need them (CONNECT's own header block carries nothing we act
    // on), but they're already in flight on the wire and must be consumed
    // before the tunnel is transparent in both directions.
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");

    if method != "CONNECT" {
        let mut client = client;
        client.write_all(
            b"HTTP/1.1 501 Not Implemented\r\n\
              Content-Length: 0\r\n\
              \r\n",
        )?;
        return Ok(());
    }

    let remote = match TcpStream::connect(target) {
        Ok(s) => s,
        Err(e) => {
            let mut client = client;
            let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n");
            return Err(e);
        }
    };

    let mut client = client;
    client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;

    let mut client_to_remote = remote.try_clone()?;
    let mut remote_to_client = client.try_clone()?;
    // Whatever the BufReader already pulled off the wire past the header
    // block (shouldn't normally be anything -- a conformant client waits
    // for the 200 before sending tunnel bytes -- but nothing guarantees
    // that) has to go out first, or it's silently lost once we drop down
    // to relaying from the raw stream.
    let leftover = reader.buffer().to_vec();
    let mut reader_for_relay = reader.into_inner();

    let relay_out = std::thread::spawn(move || {
        if !leftover.is_empty() {
            let _ = client_to_remote.write_all(&leftover);
        }
        let _ = std::io::copy(&mut reader_for_relay, &mut client_to_remote);
        let _ = client_to_remote.shutdown(std::net::Shutdown::Write);
    });

    let mut remote_reader = remote;
    let _ = std::io::copy(&mut remote_reader, &mut remote_to_client);
    let _ = remote_to_client.shutdown(std::net::Shutdown::Write);

    let _ = relay_out.join();
    Ok(())
}
