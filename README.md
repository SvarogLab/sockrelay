# sockrelay

`sockrelay` is a Rust two-binary system that relays a Windows named pipe to a unix domain socket on
the WSL/Linux side, over a single multiplexed connection. It's a generic, protocol-agnostic byte
relay, not an ssh-agent-specific proxy — but it was built for, and so far only tested against, the
real Windows OpenSSH `ssh-agent` pipe, which is the use case the rest of this README describes.

It replaces the classic `socat` + `npiperelay.exe` bridge, which spawns a brand-new Windows-side
process for every single connection. Under parallel SSH usage (e.g. Ansible forking many
concurrent SSH connections), concurrent Windows process spawning from WSL2 hits a real,
documented WSL2 interop bug ([microsoft/WSL #7371](https://github.com/microsoft/WSL/issues/7371),
[#6891](https://github.com/microsoft/WSL/issues/6891),
[#53250](https://github.com/microsoft/WSL/issues/53250)) and starts failing with
"communication with agent failed". `sockrelay` spawns the Windows-side process exactly **once**
and multiplexes every subsequent connection over it, so the bug's trigger condition never occurs.

See [BENCHMARKS.md](BENCHMARKS.md) for a head-to-head load-test comparison against the old bridge,
including the old bridge's failure caught live in the same run.

## Architecture

- **`sockrelay-client`** (WSL/Linux) daemonizes (forks, detaches from the controlling terminal —
  see [Running](#running) below), then listens on a unix domain socket. On startup it spawns
  **`sockrelay-server`** exactly once, over an anonymous pipe (the spawned process's own
  stdin/stdout), and multiplexes every accepted unix connection over that single connection using
  [`yamux`](https://docs.rs/yamux). If the connection to the server ever dies, the daemon exits
  rather than silently retrying or respawning — a supervisor (e.g. systemd) is expected to restart
  it, same as it would restart any other crashed daemon.
- **`sockrelay-server`** (Windows, cross-compiled and run via WSL binfmt interop — see
  [Building](#building) below) accepts yamux streams from the client and relays each one to a
  fresh handle on the real `ssh-agent` named pipe.

Just two crates — an earlier `sockrelay-common` (shared mux error types and a byte-relay helper)
was folded back in once one of its two pieces turned out to be single-consumer: `MuxError` is now
duplicated verbatim in both `client::mux` and `server::mux` (identical, 9 lines, cheaper to keep in
sync by hand than to maintain a whole extra crate for), and the byte-relay helper moved into
`client::relay`, its only remaining caller.

A separate load-testing tool used during this project's own development to measure behavior under
concurrent load against the real agent lives in its own repository,
[`sockrelay-loadtest`](https://github.com/ffaxl/tools-sockrelay-loadtest) — it isn't part of this
workspace, since it has no dependency on any of the crates above.

Neither `sockrelay-client` nor `sockrelay-server` parses the bytes flowing through them — no
framing, no message boundaries, nothing that assumes ssh-agent's wire format; both just relay raw
bytes between a unix socket and a named pipe in both directions. The one relay-shape decision that
was informed by real ssh-agent behavior (racing both copy directions and closing on whichever ends
first, instead of `tokio::io::copy_bidirectional`'s wait-for-both-sides-to-half-close) is actually a
fix for a property of Windows named pipes in general — they have no real half-close — not anything
ssh-specific, so it holds for any protocol relayed the same way.

## Configuration

`sockrelay-client` is the one a human actually invokes (typically from a shell profile), so every
one of its flags has a sensible default and running it completely bare — no flags at all — works
in the common case. `sockrelay-server` is different: it's spawned by `sockrelay-client`, not
launched directly by a human, so every one of its flags is **required** with no default at all —
it refuses to start rather than silently guessing at a pipe path or timeout.

### `sockrelay-client`

```text
sockrelay-client
  --socket <PATH>                        default: ~/.ssh/agent.sock
  --server-exe <PATH>                     default: "sockrelay-server.exe", searched via PATH
  --socket-mode <OCTAL>                   default: 600
  --accept-error-backoff-ms <MS>          default: 100
  --log-file <PATH>                       default: unset (no file written) — see Logging below
  --foreground                            default: off — see Running below
  --pipe-path <PATH>                      default: \\.\pipe\openssh-ssh-agent — forwarded to the spawned server
  --connect-timeout-secs <N>              default: 5 — forwarded to the spawned server
  --jitter-max-ms <MS>                    default: 50 — forwarded to the spawned server
```

```sh
sockrelay-client
```

is enough once `sockrelay-server.exe` is on `PATH` (e.g. after `install.sh` puts it in
`/usr/local/bin`) — every flag above can still be overridden explicitly, e.g.:

```sh
sockrelay-client --socket ~/.ssh/agent.sock --server-exe /path/to/sockrelay-server.exe \
  --socket-mode 600 --pipe-path '\\.\pipe\openssh-ssh-agent'
```

The last three flags (`--pipe-path`, `--connect-timeout-secs`, `--jitter-max-ms`) have no effect
on `sockrelay-client` itself — they're read here only to be forwarded as CLI arguments to the
spawned `sockrelay-server.exe`. This is deliberate, not redundant: environment variables set on
`sockrelay-client` do **not** cross the WSL binfmt-interop boundary into the spawned Windows
process (confirmed empirically), so argv is the only channel that reliably reaches it.

### `sockrelay-server`

Not meant to be run directly by an end user — it is spawned by `sockrelay-client`, which passes
every flag explicitly (and its own `--help` text says as much). Useful for manual testing against
a real agent, though — every flag below is required, with no default:

```powershell
sockrelay-server.exe \
  --pipe-path '\\.\pipe\openssh-ssh-agent' \
  --connect-timeout-secs 5 \
  --jitter-max-ms 50
```

- `--connect-timeout-secs` bounds how long a connection attempt will wait for a saturated agent
  before giving up, once it hits `ERROR_PIPE_BUSY`.
- `--jitter-max-ms` is the random delay slept between busy-retry attempts, which exists to break a
  thundering herd: multiple waiters woken by the same freed pipe instance would otherwise all
  retry in the same instant, and only one could win.

## Running

`sockrelay-client` daemonizes itself: it forks, detaches from the controlling terminal (`setsid`),
and only returns control to whoever invoked it once startup has a known outcome — so it's safe to
invoke synchronously from a shell profile, without a trailing `&`, and without hanging the shell
startup. Pass `--foreground` to skip all of that — no fork, no detaching — when a process
supervisor (systemd, a container entrypoint) already expects to own this process directly, or for
interactive debugging; the exit code is then just this process's own, since there's no separate
daemon to report an outcome back from.

"A known outcome" means more than just "the socket bind and spawn succeeded": `sockrelay-server`
checks, at startup, whether the configured `--pipe-path` actually exists (a fast, non-blocking
check — it doesn't wait around for a busy agent, just confirms *some* agent is listening under
that name) and exits immediately if not. `sockrelay-client` gives the spawned server a short grace
period (500ms) to prove it's still alive before declaring success, specifically to catch that
failure — a misconfigured `--pipe-path`, or the real agent not running at all — rather than only
discovering it on the first real connection, potentially much later.

Exit codes reflect the real outcome:

- **`0`** — either started fresh (prints `sockrelay-client: started` to stderr) or an instance was
  already running for that socket (prints nothing — this is the ordinary case on every subsequent
  shell login, not worth announcing every time). Both mean "the agent is up" to the caller.
- **non-zero** — a genuine startup failure (couldn't bind the socket for a real reason, couldn't
  spawn the Windows server, the server exited immediately — e.g. the configured pipe doesn't
  exist). Diagnostics are printed to stderr by the binary itself, since output isn't redirected
  until *after* a successful start — no shell-level `|| echo ...` needed to surface a failure.

```sh
sockrelay-client
echo "started ok: $?"
```

A shell-profile invocation only needs to export `SSH_AUTH_SOCK` once startup is confirmed —
`&&` is enough, since a failure already prints its own diagnostics and a non-zero exit code:

```sh
sockrelay-client && export SSH_AUTH_SOCK=~/.ssh/agent.sock
```

## Logging

`--log-file`, unset by default, controls where timestamped diagnostics *also* end up once
daemonized — both `sockrelay-client`'s own, and the spawned `sockrelay-server`'s (its stderr is
piped and forwarded verbatim into the same sink, since it has no concept of a log file, or of this
process's `--log-file`, at all — it just writes to its own stderr exactly as if run directly). If
`--log-file` isn't given, no file is ever opened — there's no implicit fallback path standing in
for "no file".

Whether a diagnostic *also* reaches stderr is a fully independent question from `--log-file`,
governed by daemonization rather than by the flag: before a successful start, it always does — this
is what lets a startup failure surface immediately to whoever ran the command (see
[Running](#running) above) — and after a successful *daemonized* start, it stops, since
`detach_stdio` releases the terminal at that point anyway. With `--foreground`, there's no
detaching at all, so output keeps reaching stderr (and `--log-file`, if given) for as long as the
process runs — the natural behavior for something a supervisor or an interactive user is watching
directly.

```sh
sockrelay-client --log-file ~/.cache/sockrelay/client.log
```

## Building

The Windows side is cross-compiled for `aarch64-pc-windows-msvc` using
[`cargo-xwin`](https://github.com/rust-cross/cargo-xwin) — WSL's own binfmt interop executes the
cross-compiled `.exe` **natively** (no Wine/emulation, since it's the same CPU architecture), which
is what lets this whole system run entirely from a WSL shell without ever touching a separate
Windows session.

```sh
rustup target add aarch64-pc-windows-msvc
cargo install cargo-xwin

cargo build -p sockrelay-client
cargo xwin build -p sockrelay-server --target aarch64-pc-windows-msvc
```

`install.sh` automates this (detecting the host's real CPU architecture rather than assuming
`aarch64`) and installs both release binaries into `/usr/local/bin`. It doesn't touch any shell
config or stop any currently-running agent bridge — it only prints what to add and where, leaving
the actual wiring-up to you:

```sh
./install.sh
```

### Prebuilt binaries

Each [release](https://github.com/ffaxl/tools-sockrelay/releases) carries the client for Linux
(`sockrelay-client-linux-amd64`, `sockrelay-client-linux-arm64`, static musl builds) and the server
for Windows (`sockrelay-server-windows-amd64.exe`, `sockrelay-server-windows-arm64.exe`), with
their checksums in `SHA256SUMS`. Each is built and tested on its own architecture's runner.

### Releasing

Run the **Release PR** workflow from the Actions tab. It opens `chore: release vX.Y.Z` with the
version git-cliff computes from the commits since the last tag and a regenerated `CHANGELOG.md`;
merging it tags the commit and publishes the release.

## Testing

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
```

`server/src/pipe.rs` is gated on `#![cfg(windows)]`, so the host build above only compiles an
empty module for it — its real logic and tests only build for the Windows target:

```sh
eval "$(cargo xwin env --target aarch64-pc-windows-msvc)"
unset CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_RUNNER
cargo test -p sockrelay-server --target aarch64-pc-windows-msvc
cargo clippy -p sockrelay-server --all-targets --target aarch64-pc-windows-msvc
```

(`cargo xwin test`/`cargo xwin run` set `CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_RUNNER=wine`, which
isn't installed or needed here — WSL interop already runs the binary directly — so the runner
variable is explicitly unset before falling back to plain `cargo test`/`cargo build`.)

`cargo clippy --workspace --all-targets --target aarch64-pc-windows-msvc` is **not** meaningful:
`sockrelay-client` is unix-only code with no Windows-compat shims, so cross-compiling the whole
workspace for Windows fails on unrelated errors in that crate. Only `-p sockrelay-server` is a
valid Windows-target build.

CI runs the same checks on every push and pull request, and daily on `main`: the workspace on
Linux for amd64 and arm64, and `-p sockrelay-server` natively on Windows for amd64 and arm64.

## Security notes

- The unix socket is created with the process umask temporarily restricted so its permission bits
  come out as exactly `--socket-mode`, with no bind-then-`chmod` window where it briefly exists
  with looser permissions.
- A startup-only file lock guards against two instances racing to reclaim the same stale socket
  path left behind by an unclean previous exit.
- The relay never retries or respawns past its initial cold start: if the connection to the
  spawned Windows process dies, the daemon exits rather than masking the failure. A supervisor
  (systemd, a process manager) is expected to restart it, the same as any other crashed daemon.
- A busy/saturated agent produces an honest, bounded failure (`--connect-timeout-secs`) rather
  than an unbounded hang or a silently retried request.
