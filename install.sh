#!/bin/sh
# trickle-tui installer
#
# Builds trickle-tui from source and installs it so you can just type
# `trickle-tui` from anywhere. Safe to re-run.
#
# Usage:
#   git clone https://github.com/gregoftheweb/trickle-tui.git
#   cd trickle-tui
#   ./install.sh

set -e

REPO_ROOT="$(cd "$(dirname "$0")" && pwd)"
LIBDIR="$HOME/.local/lib/trickle-tui"
BINDIR="$HOME/.local/bin"

say() { printf '\n==> %s\n' "$1"; }
ask() {
    # ask "question" -- returns 0 for yes, 1 for no. Defaults to yes.
    printf '%s [Y/n] ' "$1"
    read -r reply
    case "$reply" in
        [nN]*) return 1 ;;
        *) return 0 ;;
    esac
}

cd "$REPO_ROOT"
if [ ! -f configure.in ] || [ ! -d tui ]; then
    echo "error: run this script from inside the trickle-tui repo (couldn't find configure.in / tui/)." >&2
    exit 1
fi

say "Step 1 of 5: checking build tools"

missing_pkgs=""
need_cmd() { command -v "$1" >/dev/null 2>&1; }

if ! need_cmd autoreconf || ! need_cmd automake || ! need_cmd libtool || ! need_cmd pkg-config || ! need_cmd gcc && ! need_cmd cc; then
    PKG_MANAGER=""
    if need_cmd apt-get; then
        PKG_MANAGER="apt-get"
        missing_pkgs="build-essential autoconf automake libtool pkg-config libtirpc-dev"
    elif need_cmd pacman; then
        PKG_MANAGER="pacman"
        missing_pkgs="base-devel libtirpc"
    elif need_cmd dnf; then
        PKG_MANAGER="dnf"
        missing_pkgs="gcc autoconf automake libtool pkgconf libtirpc-devel"
    fi

    if [ -z "$PKG_MANAGER" ]; then
        echo "Couldn't detect apt, pacman, or dnf on this machine."
        echo "Please install these yourself, then re-run this script:"
        echo "  - a C compiler, autoconf, automake, libtool, pkg-config"
        echo "  - libtirpc development headers"
        exit 1
    fi

    echo "trickle-tui needs some build tools that aren't installed yet:"
    echo "  $missing_pkgs"
    if ask "Install them now with sudo $PKG_MANAGER?"; then
        case "$PKG_MANAGER" in
            apt-get) sudo apt-get update && sudo apt-get install -y $missing_pkgs ;;
            pacman)  sudo pacman -S --needed --noconfirm $missing_pkgs ;;
            dnf)     sudo dnf install -y $missing_pkgs ;;
        esac
    else
        echo "OK, skipping. Install them yourself and re-run this script."
        exit 1
    fi
else
    echo "Build tools already present."
fi

say "Step 2 of 5: checking for Rust"

if [ -f "$HOME/.cargo/env" ]; then
    . "$HOME/.cargo/env"
fi

if ! need_cmd cargo || ! need_cmd rustc; then
    echo "trickle-tui's interface is written in Rust, which isn't installed yet."
    if ask "Install Rust now via rustup (the standard installer)?"; then
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
        . "$HOME/.cargo/env"
    else
        echo "OK, skipping. Install Rust yourself from https://rustup.rs and re-run this script."
        exit 1
    fi
else
    echo "Rust already present ($(rustc --version))."
fi

say "Step 3 of 5: building trickle (the C side)"

autoreconf -fi
./configure
make

say "Step 4 of 5: building trickle-tui (the interface)"

(cd tui && cargo build --release)

say "Step 5 of 5: installing"

mkdir -p "$LIBDIR" "$BINDIR"
cp trickle trickle-overload.so tui/target/release/trickle-tui tui/target/release/throttle-proxy "$LIBDIR/"
chmod +x "$LIBDIR/trickle" "$LIBDIR/trickle-tui" "$LIBDIR/throttle-proxy"

cat > "$BINDIR/trickle-tui" <<EOF
#!/bin/sh
export TRICKLE_TUI_TRICKLE_BIN="$LIBDIR/trickle"
exec "$LIBDIR/trickle-tui" "\$@"
EOF
chmod +x "$BINDIR/trickle-tui"

echo "Installed to $LIBDIR, with a launcher at $BINDIR/trickle-tui."

case ":$PATH:" in
    *":$BINDIR:"*)
        say "All done! Type trickle-tui to run it."
        ;;
    *)
        say "One more thing: $BINDIR isn't on your PATH yet."
        SHELL_RC=""
        case "$SHELL" in
            */zsh)  SHELL_RC="$HOME/.zshrc" ;;
            */bash) SHELL_RC="$HOME/.bashrc" ;;
        esac
        if [ -n "$SHELL_RC" ] && ask "Add it to $SHELL_RC automatically?"; then
            printf '\nexport PATH="%s:$PATH"\n' "$BINDIR" >> "$SHELL_RC"
            echo "Added. Close and reopen your terminal (or run: source $SHELL_RC), then type trickle-tui to run it."
        else
            echo "Add this line to your shell's startup file (.bashrc / .zshrc), then open a new terminal:"
            echo "  export PATH=\"$BINDIR:\$PATH\""
        fi
        ;;
esac
