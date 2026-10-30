#!/bin/sh
# Builds both sockrelay binaries and installs them into /usr/local/bin, and the systemd user units
# into the user unit directory. Restarts a running sockrelay.service, enables and starts nothing,
# and doesn't touch ~/.profile (or any other shell config) or any other agent bridge — it prints
# the steps that are left. Safe to re-run any time to rebuild and reinstall.
set -eu

REPO_DIR="$(cd "$(dirname "$0")" && pwd)"
SOCKET_PATH="$HOME/.ssh/agent.sock"
BIN_DIR="/usr/local/bin"
CLIENT_BIN="$BIN_DIR/sockrelay-client"
SERVER_BIN="$BIN_DIR/sockrelay-server.exe"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"

# The Windows target must match the *host's* real CPU architecture, not just default to aarch64:
# WSL's binfmt interop runs the cross-compiled .exe natively (no Wine/emulation) only because it's
# the same architecture as the host — a mismatched target would need real emulation instead, which
# defeats the whole point (and likely wouldn't even be set up here).
case "$(uname -m)" in
    aarch64 | arm64) WIN_TARGET="aarch64-pc-windows-msvc" ;;
    x86_64 | amd64) WIN_TARGET="x86_64-pc-windows-msvc" ;;
    *)
        echo "sockrelay install.sh: unrecognized host architecture '$(uname -m)' — don't know which *-pc-windows-msvc target matches it" >&2
        exit 1
        ;;
esac
echo "==> Host architecture $(uname -m) -> Windows target $WIN_TARGET"

rustup target add "$WIN_TARGET"

echo "==> Building sockrelay-client (release)"
(cd "$REPO_DIR" && cargo build --release -p sockrelay-client)

echo "==> Building sockrelay-server (release, $WIN_TARGET)"
(cd "$REPO_DIR" && cargo xwin build --release -p sockrelay-server --target "$WIN_TARGET")

echo "==> Installing binaries into /usr/local/bin (sudo)"
sudo install -m 755 "$REPO_DIR/target/release/sockrelay-client" "$CLIENT_BIN"
sudo install -m 755 "$REPO_DIR/target/$WIN_TARGET/release/sockrelay-server.exe" "$SERVER_BIN"

echo "==> Installing the systemd user units into $UNIT_DIR"
mkdir -p "$UNIT_DIR"
cp "$REPO_DIR/contrib/sockrelay.socket" "$UNIT_DIR/sockrelay.socket"
sed "s|@BINDIR@|$BIN_DIR|g" "$REPO_DIR/contrib/sockrelay.service" > "$UNIT_DIR/sockrelay.service"
if systemctl --user show-environment >/dev/null 2>&1; then
    systemctl --user daemon-reload
    # A running instance keeps the binary it started with until it restarts; a stopped one stays stopped.
    systemctl --user try-restart sockrelay.service
fi

cat <<EOF

==> Installed:
    $CLIENT_BIN
    $SERVER_BIN
    $UNIT_DIR/sockrelay.socket
    $UNIT_DIR/sockrelay.service

sockrelay runs under systemd, which needs systemd running in WSL (systemd=true under [boot] in
/etc/wsl.conf). To use it instead of whatever currently provides your ssh-agent socket:

1. Stop whatever is currently listening on $SOCKET_PATH, if anything (e.g. an old
   socat+npiperelay bridge). Check with:
       ps aux | grep -i "socat\|npiperelay\|sockrelay"

2. In whatever starts that bridge today (~/.profile, ~/.bashrc, etc.), remove that invocation and
   only point SSH at the socket:

       export SSH_AUTH_SOCK="$SOCKET_PATH"

3. Let systemd own the socket and start the relay on the first connection:

       systemctl --user enable --now sockrelay.socket

4. Open a new login shell (or re-source whichever file you edited) to pick it up.
EOF
