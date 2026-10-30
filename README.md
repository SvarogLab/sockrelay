# sockrelay

`sockrelay` relays a Windows named pipe to a unix socket on the WSL side: the real Windows OpenSSH
`ssh-agent` pipe, for Linux `ssh`. It relays raw bytes and parses nothing, but it has only been
built and tested for `ssh-agent`, which is what this README describes.

It replaces the `socat` + `npiperelay.exe` bridge, which starts a Windows process per connection.
Under parallel SSH (Ansible forking many connections) starting Windows processes concurrently from
WSL hits an interop bug ([microsoft/WSL #7371](https://github.com/microsoft/WSL/issues/7371),
[#6891](https://github.com/microsoft/WSL/issues/6891),
[#53250](https://github.com/microsoft/WSL/issues/53250)) and fails with "communication with agent
failed". `sockrelay` starts its Windows process once and carries every connection over it.
[BENCHMARKS.md](BENCHMARKS.md) compares the two under load, the old bridge's failure included.

## Architecture

- **`sockrelay-client`** (Linux) runs as a systemd user service on the unix socket systemd passes
  it (see [Running](#running)). It starts **`sockrelay-server`** once and carries each accepted
  connection over its own stream of one [`yamux`](https://docs.rs/yamux) connection over the
  server's stdin and stdout. If that link is lost the client exits, and systemd restarts it.
- **`sockrelay-server`** (Windows, cross-compiled; WSL runs it natively) relays each stream to a
  fresh handle on the `ssh-agent` pipe. It exits as soon as the client is gone, so it never
  outlives it.

Starting the server, the yamux connection, watching both ends, logging and systemd integration come
from [`librelay`](https://github.com/SvarogLab/librelay), shared with
[`credrelay`](https://github.com/SvarogLab/credrelay), a relay of the same shape for the Windows
Credential Manager. What is left here is the relay: the socket on one side, the pipe on the other.

Windows named pipes have no half-close, so the server's relay ends a connection as soon as either
direction does, rather than waiting for both as `tokio::io::copy_bidirectional` does on the unix
side.

A load-testing tool used while developing this lives in its own repository,
[`sockrelay-loadtest`](https://github.com/ffaxl/tools-sockrelay-loadtest).

## Configuration

### `sockrelay-client`

Every flag has a default, so the systemd unit runs it bare:

```text
sockrelay-client
  --server-exe <PATH>                default: sockrelay-server.exe, searched in PATH
  --accept-error-backoff-ms <MS>     default: 100
  --pipe-path <PATH>                 default: \\.\pipe\openssh-ssh-agent   (passed to the server)
  --connect-timeout-secs <N>         default: 5                           (passed to the server)
  --jitter-max-ms <MS>               default: 50                          (passed to the server)
```

The socket's path and mode belong to `sockrelay.socket`, not to a flag. The last three flags only
pass through to the server as arguments: environment variables don't cross the WSL interop
boundary into a Windows process.

### `sockrelay-server`

Started by `sockrelay-client`, which passes every flag; every flag is required, so it never guesses.
By hand, against a real agent:

```powershell
sockrelay-server.exe --pipe-path '\\.\pipe\openssh-ssh-agent' --connect-timeout-secs 5 `
  --jitter-max-ms 50 --log-filter info
```

- `--connect-timeout-secs` bounds how long a connection waits for a saturated agent once the pipe
  answers `ERROR_PIPE_BUSY`.
- `--jitter-max-ms` is the random pause between busy retries, so waiters woken by the same freed
  pipe instance don't all retry at once.
- `--log-filter` is the server's `tracing` filter (see [Logging](#logging)).

## Running

`sockrelay` runs under systemd, which needs systemd running in WSL (`systemd=true` under `[boot]`
in `/etc/wsl.conf`). `install.sh` installs two user units into `$XDG_CONFIG_HOME/systemd/user`
(`~/.config/systemd/user` by default):

- **`sockrelay.socket`** binds `~/.ssh/agent.sock` (mode `0600`) and starts the service on the
  first connection. It stays open while the service restarts, so a connection made meanwhile waits
  instead of being refused.
- **`sockrelay.service`** runs `sockrelay-client` as `Type=notify` on that socket, and restarts it
  2 seconds after a failure.

```sh
systemctl --user enable --now sockrelay.socket
```

starts the relay on demand; enabling `sockrelay.service` too starts it with the session. The shell
profile only points SSH at the socket:

```sh
export SSH_AUTH_SOCK=~/.ssh/agent.sock
```

systemd hears the service is ready only once the relay is known to work. `sockrelay-server` exits at
once if `--pipe-path` names no running agent, and `sockrelay-client` waits 500ms for the server to
prove it stays up, so a wrong pipe or a stopped agent fails the start rather than the first
connection. Any startup failure exits non-zero with its reason in the journal.

`systemctl --user stop sockrelay` stops gracefully: the client stops accepting, gives connections
in flight up to 5 seconds, closes the link, and the server exits on its own.

For debugging, with the units stopped, `systemd-socket-activate` passes a socket the way the unit
does:

```sh
RUST_LOG=debug systemd-socket-activate -l /tmp/agent.sock sockrelay-client
```

## Logging

Both processes log to the client's stderr, which systemd sends to the journal:

```sh
journalctl --user -u sockrelay
```

- The filter is `RUST_LOG`, `info` when unset or empty; set it for the service with
  `systemctl --user edit sockrelay.service` and `Environment=RUST_LOG=debug` under `[Service]`. An
  invalid one stops the client before it starts anything.
- The client passes the same filter to the server as `--log-filter`.
- Each line the server writes is forwarded with a `server:` prefix followed by a space. The server
  leaves out its own timestamp when its stderr isn't a terminal, since the journal adds one.
- Output is colored only on a terminal.

## Building

The server is cross-compiled for the host's own architecture with
[`cargo-xwin`](https://github.com/rust-cross/cargo-xwin); WSL then runs the `.exe` natively, with no
emulation:

```sh
rustup target add aarch64-pc-windows-msvc
cargo install cargo-xwin

cargo build -p sockrelay-client
cargo xwin build -p sockrelay-server --target aarch64-pc-windows-msvc
```

`install.sh` does this for the host's architecture and installs both release binaries into
`/usr/local/bin` and the units. Windows keeps running an `.exe` it already has open even after the
file is replaced, so it first stops the service and any server left running, and starts the
service again if it was running. It enables nothing and touches no shell config; it prints the
remaining steps:

```sh
./install.sh
```

### Prebuilt binaries

Each [release](https://github.com/SvarogLab/sockrelay/releases) carries the client for Linux
(`sockrelay-client-linux-amd64`, `sockrelay-client-linux-arm64`, static musl builds) and the server
for Windows (`sockrelay-server-windows-amd64.exe`, `sockrelay-server-windows-arm64.exe`), with
their checksums in `SHA256SUMS`. Install them as `sockrelay-client` and `sockrelay-server.exe` on
`PATH`, then copy `contrib/sockrelay.socket` and `contrib/sockrelay.service` where `install.sh` puts
them, replacing `@BINDIR@` with the directory holding `sockrelay-client`.

### Releasing

Run the **Release PR** workflow from the Actions tab. It opens `chore: release vX.Y.Z` with the
version git-cliff computes from the commits since the last tag and a regenerated `CHANGELOG.md`;
merging it tags the commit and publishes the release.

## Testing

```sh
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

The server's pipe code only builds for Windows. Its tests run there, cross-compiled; WSL runs the
test binary natively:

```sh
eval "$(cargo xwin env --target aarch64-pc-windows-msvc)"
unset CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_RUNNER
cargo test -p sockrelay-server --target aarch64-pc-windows-msvc
cargo clippy -p sockrelay-server --all-targets --target aarch64-pc-windows-msvc -- -D warnings
```

(`cargo xwin env` sets the target's runner to `wine`, which isn't needed here, so it is unset.) The
client is unix-only, so only `-p sockrelay-server` builds for Windows.

## Security notes

- systemd creates the socket with `SocketMode=0600` from `sockrelay.socket`, before the client runs.
- The relay never retries or respawns: if the link to the server is lost the client exits rather
  than masking it, and systemd restarts it.
- A saturated agent produces a bounded failure (`--connect-timeout-secs`) rather than a hang or a
  silently retried request.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT)
at your option.
