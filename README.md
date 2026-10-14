trickle-tui
===========

A TUI front-end for [trickle](http://monkey.org/~marius/trickle), the
userland bandwidth shaper, forked from Marius Aamodt Eriksen's original
project. Pick an app, set an upload/download cap, launch it wrapped in
trickle, and watch (or adjust) its bandwidth live.

Two ways a target gets throttled:

- **Direct** (LD_PRELOAD) -- works for most dynamically-linked programs.
  Live dashboard with sparklines, totals, and arrow-key limit adjustment.
- **Proxy** (HTTPS_PROXY) -- for targets LD_PRELOAD can't touch at all
  (statically linked binaries have no dynamic-linker step for it to hook
  into). Routes the target through a small local throttle-proxy instead,
  which *is* a normal dynamically-linked binary trickle can throttle.
  Works as long as the target respects the proxy env vars. Known
  limitation: a target that opens many concurrent connections (a chatty
  interactive session, not a single request) can run well under its
  configured cap -- see the "Known limitations" section.

The launcher also measures your real upload speed via curl at startup (in
the background, never blocking) and suggests a cap at 70% of it, so a
heavy upload doesn't starve everything else sharing the line.

Prerequisites
-------------

Build tooling, plus a Rust toolchain:

| | apt (Debian/Ubuntu) | pacman (Arch) | dnf (Fedora) |
|---|---|---|---|
| core | `build-essential autoconf automake libtool pkg-config` | `base-devel` | `gcc autoconf automake libtool pkgconf` |
| RPC headers | `libtirpc-dev` | `libtirpc` | `libtirpc-devel` |
| tests (optional) | `libcmocka-dev` | `cmocka` | `libcmocka-devel` |

Rust: install via [rustup](https://rustup.rs) if `cargo`/`rustc` aren't
already on the machine.

Build
-----

The C side (trickle itself, trickled, the LD_PRELOAD shim, the
throttle-proxy's trickle wrapper all route through this):

    autoreconf -fi
    ./configure
    make

This produces `trickle`, `trickled`, `tricklectl`, and
`trickle-overload.so` in the repo root. (Optional: `make check` runs the
C unit test suite, needs cmocka.)

The TUI:

    cd tui
    cargo build --release

This produces `tui/target/release/trickle-tui` and
`tui/target/release/throttle-proxy` (the proxy binary for Proxy routing
mode -- the TUI finds it automatically alongside its own executable, no
separate install step).

Run
---

From the `tui/` directory:

    ./target/release/trickle-tui

By default it looks for the `trickle` binary at `../trickle` (i.e.
alongside the repo root, matching the build layout above). Override with
`TRICKLE_TUI_TRICKLE_BIN` if you've installed `trickle` somewhere else
(e.g. after `make install`):

    TRICKLE_TUI_TRICKLE_BIN=/usr/local/bin/trickle ./target/release/trickle-tui

Installing on another machine
------------------------------

There's no package/installer yet -- clone the repo and build as above.
Keep the repo's layout intact (`trickle-overload.so` next to `trickle`,
`throttle-proxy` next to `trickle-tui`'s own binary); both are located
relative to their sibling at runtime, not hardcoded.

Usage
-----

- **Launch as?** -- "open an app" hands the real terminal to the target
  (claude, codex, a shell -- anything interactive); "run a hidden
  process" keeps a live dashboard instead (curl, vivaldi, anything that
  doesn't need your stdin).
- **Routing?** -- Direct for most things; Proxy for statically-linked
  targets Direct can't touch at all (codex, confirmed, is one of these).
- **Upload/Download limit (KB/s)** -- Upload defaults to a measured
  suggestion (see above); type over it to set your own.
- **Timed?** -- permanent, or auto-lift the cap after N minutes.

While running (dashboard mode): Up/Down adjusts upload ±10%, Left/Right
adjusts download ±10%, `k` kills, `q`/Esc returns to the launcher.

Known limitations
------------------

- The launcher's command field splits on whitespace -- no quoted
  arguments with spaces.
- Proxy routing's own stats/limit-adjustment work the same as Direct's,
  but a target that opens many concurrent connections (an interactive
  chat-style session, not a single request) can see real throughput well
  under its configured cap -- every relayed chunk still briefly touches
  trickle's single global lock, and enough concurrent small I/O calls
  can choke on that contention even after the locking fixes in this
  fork. Fine for one-shot calls; not yet solid for a long interactive
  session through Proxy mode specifically.

License
-------

trickle-tui is a fork of trickle, distributed under the same BSD-like
license -- see LICENSE.
