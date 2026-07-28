#!/bin/sh
# Builds both sockrelay binaries and installs them into /usr/local/bin. Does not touch
# ~/.profile (or any other shell config) or any already-running agent bridge — it just prints
# what to add, and leaves wiring it up to you. Safe to re-run any time to rebuild and reinstall.
set -eu

REPO_DIR="$(cd "$(dirname "$0")" && pwd)"
SOCKET_PATH="$HOME/.ssh/agent.sock"
CLIENT_BIN="/usr/local/bin/sockrelay-client"
SERVER_BIN="/usr/local/bin/sockrelay-server.exe"

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

cat <<EOF

==> Installed:
    $CLIENT_BIN
    $SERVER_BIN

To use sockrelay instead of whatever currently provides your ssh-agent socket:

1. Stop whatever is currently listening on $SOCKET_PATH, if anything (e.g. an old
   socat+npiperelay bridge). Check with:
       ps aux | grep -i "socat\|npiperelay"

2. In whatever starts your ssh-agent bridge today (~/.profile, ~/.bashrc, etc.), comment out
   that old invocation.

3. Add this in its place:

       $CLIENT_BIN && export SSH_AUTH_SOCK="$SOCKET_PATH"

   $CLIENT_BIN needs no flags in the common case (every setting defaults sensibly — see the
   README's Configuration section). It daemonizes itself, prints its own diagnostics to stderr
   on a genuine failure (nothing at all if an instance was already running — only a fresh start
   prints anything), and returns a real exit code — so \$SSH_AUTH_SOCK only gets set once it's
   confirmed there's actually something listening behind it.

4. Open a new login shell (or re-source whichever file you edited) to pick it up.
EOF
