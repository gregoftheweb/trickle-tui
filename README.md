# trickle-tui

A TUI front-end for [trickle](http://monkey.org/~marius/trickle), the userland bandwidth shaper, forked from Marius Aamodt Eriksen's original project. Pick an app, set an upload/download cap, launch it wrapped in trickle, and watch (or adjust) its bandwidth live.

Two ways a target gets throttled:

- **Direct** (LD\_PRELOAD) -- works for most dynamically-linked programs. Live dashboard with sparklines, totals, and arrow-key limit adjustment.
- **Proxy** (HTTPS\_PROXY) -- for targets LD\_PRELOAD can't touch at all (statically linked binaries have no dynamic-linker step for it to hook into). Routes the target through a small local throttle-proxy instead, which *is* a normal dynamically-linked binary trickle can throttle. Works as long as the target respects the proxy env vars. Known limitation: a target that opens many concurrent connections (a chatty interactive session, not a single request) can run well under its configured cap -- see the "Known limitations" section.

The launcher also measures your real upload speed via curl at startup (in the background, never blocking) and suggests a cap at 70% of it, so a heavy upload doesn't starve everything else sharing the line.

## Installing

### Option A: the installer script (recommended)

This does everything for you: installs build tools, installs Rust if you don't have it, builds trickle-tui, and sets it up so you can just type `trickle-tui` from any terminal, in any folder. It will ask before it uses `sudo` or changes anything system-wide.

1. Open a terminal.
2. Download the project:
   ```
   git clone https://github.com/gregoftheweb/trickle-tui.git
   cd trickle-tui
   ```
3. Run the installer:
   ```
   ./install.sh
   ```
4. Answer `y` (or just press Enter) at each prompt.
5. When it finishes, either open a new terminal window, or run `source ~/.bashrc` (or `source ~/.zshrc` if you use zsh).
6. Type `trickle-tui` and press Enter. You should see the launcher screen.

If anything goes wrong partway through, it's safe to just run `./install.sh` again -- it picks up where it left off.

### Option B: by hand

If you'd rather not run a script, or want to see exactly what's happening at each step, here's the same process done manually.

**1. Install the build tools.** You need a C compiler, autoconf, automake, libtool, pkg-config, and the libtirpc headers.

|                  | apt (Debian/Ubuntu)                                    | pacman (Arch) | dnf (Fedora)                            |
| ---------------- | ------------------------------------------------------ | ------------- | --------------------------------------- |
| core             | `build-essential autoconf automake libtool pkg-config` | `base-devel`  | `gcc autoconf automake libtool pkgconf` |
| RPC headers      | `libtirpc-dev`                                         | `libtirpc`    | `libtirpc-devel`                        |
| tests (optional) | `libcmocka-dev`                                        | `cmocka`      | `libcmocka-devel`                       |

Example on Ubuntu: `sudo apt-get install build-essential autoconf automake libtool pkg-config libtirpc-dev`

**2. Install Rust.** trickle-tui's interface is written in Rust. If `cargo --version` doesn't print anything, install it from [rustup.rs](https://rustup.rs) (one line, no sudo needed) and then open a new terminal.

**3. Download the project** (skip if you already did this for Option A):
```
git clone https://github.com/gregoftheweb/trickle-tui.git
cd trickle-tui
```

**4. Build trickle itself** (the bandwidth-shaping engine):
```
autoreconf -fi
./configure
make
```
This produces `trickle`, `trickled`, `tricklectl`, and `trickle-overload.so` in the repo root. (Optional: `make check` runs the C unit test suite, needs cmocka.)

**5. Build the TUI** (the screen you actually interact with):
```
cd tui
cargo build --release
cd ..
```
This produces `tui/target/release/trickle-tui` and `tui/target/release/throttle-proxy`.

**6. Install the four binaries somewhere permanent, together in one folder** -- `trickle-overload.so` must sit next to `trickle`, and `throttle-proxy` must sit next to `trickle-tui`, because the app finds them by looking in its own folder at runtime, not by a fixed path:
```
mkdir -p ~/.local/lib/trickle-tui
cp trickle trickle-overload.so tui/target/release/trickle-tui tui/target/release/throttle-proxy ~/.local/lib/trickle-tui/
```

**7. Create a launcher on your PATH**, so typing `trickle-tui` works from anywhere:
```
mkdir -p ~/.local/bin
cat > ~/.local/bin/trickle-tui <<'EOF'
#!/bin/sh
export TRICKLE_TUI_TRICKLE_BIN="$HOME/.local/lib/trickle-tui/trickle"
exec "$HOME/.local/lib/trickle-tui/trickle-tui" "$@"
EOF
chmod +x ~/.local/bin/trickle-tui
```

**8. Make sure `~/.local/bin` is on your PATH.** Run `echo $PATH` and check it's listed. If not, add this line to `~/.bashrc` (or `~/.zshrc`), then open a new terminal:
```
export PATH="$HOME/.local/bin:$PATH"
```

**9. Run it:**
```
trickle-tui
```

## Usage

- **Launch as?** -- "open an app" hands the real terminal to the target (claude, codex, a shell -- anything interactive); "run a hidden process" keeps a live dashboard instead (curl, vivaldi, anything that doesn't need your stdin).
- **Routing?** -- Direct for most things; Proxy for statically-linked targets Direct can't touch at all (codex, confirmed, is one of these).
- **Upload/Download limit (KB/s)** -- Upload defaults to a measured suggestion (see above); type over it to set your own.
- **Timed?** -- permanent, or auto-lift the cap after N minutes.

While running (dashboard mode): Up/Down adjusts upload ±10%, Left/Right adjusts download ±10%, `k` kills, `q`/Esc returns to the launcher.

## Known limitations

- The launcher's command field splits on whitespace -- no quoted arguments with spaces.
- Proxy routing's own stats/limit-adjustment work the same as Direct's, but a target that opens many concurrent connections (an interactive chat-style session, not a single request) can see real throughput well under its configured cap -- every relayed chunk still briefly touches trickle's single global lock, and enough concurrent small I/O calls can choke on that contention even after the locking fixes in this fork. Fine for one-shot calls; not yet solid for a long interactive session through Proxy mode specifically.

## License

trickle-tui is a fork of trickle, distributed under the same BSD-like license -- see LICENSE.