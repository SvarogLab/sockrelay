//! Windows-only: connects each accepted yamux stream to a fresh handle on the local ssh-agent
//! named pipe. Gated on the whole file so a host-target `cargo check`/`cargo test --workspace`
//! (this is developed on Linux/WSL) just sees an empty module instead of failing to find
//! `tokio::net::windows::named_pipe`, which doesn't exist under `cfg(unix)`. Build/test this
//! module for real via `--target aarch64-pc-windows-msvc` (see this project's README for the
//! exact recipe — WSL binfmt interop runs the cross-compiled test binary natively, no
//! wine/emulation needed).
#![cfg(windows)]

use std::io;
use std::time::Duration;

use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY};
use windows_sys::Win32::System::Pipes::{NMPWAIT_NOWAIT, NMPWAIT_USE_DEFAULT_WAIT, WaitNamedPipeW};

/// Tunables for `connect`'s busy-wait loop — configurable (CLI flag or env var, see
/// `config.rs`) rather than hardcoded, since the right values depend on the real agent's
/// capacity, which varies by machine and isn't something this project can know in advance.
#[derive(Debug, Clone, Copy)]
pub struct PipeSettings {
    /// Overall budget for `connect`'s busy-wait loop — bounds how long a caller can be stuck
    /// behind a saturated agent before getting an honest failure, same spirit as the old
    /// (now-removed) 4s polling-retry budget, just reached via real OS waits instead of blind
    /// sleeps.
    pub connect_timeout: Duration,
    /// Upper bound (in milliseconds) of the random jitter slept after each `WaitNamedPipeW`
    /// wakeup and before retrying the open — see `connect`'s doc comment for why this exists.
    pub jitter_max_ms: u64,
}

impl Default for PipeSettings {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(5),
            jitter_max_ms: 50,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PipeError {
    #[error("failed to connect to named pipe {path:?}: {source}")]
    Connect {
        path: String,
        #[source]
        source: io::Error,
    },
}

/// Blocks the calling (blocking-pool) thread until `path` has a free instance to connect to, or
/// the pipe's own default wait timeout elapses — a thin wrapper around the real Win32
/// `WaitNamedPipeW` API, which is how a well-behaved named-pipe client is meant to handle
/// `ERROR_PIPE_BUSY` (see `connect`'s doc comment for why this replaced both the removed
/// naive polling retry and a since-rejected userspace semaphore).
///
/// `NMPWAIT_USE_DEFAULT_WAIT` deliberately avoids inventing our own timeout: it tells Windows to
/// use whatever wait time the *server* (the real ssh-agent) configured when it created the pipe —
/// we don't know that number, don't need to, and have no principled way to guess it ourselves.
fn wait_for_free_instance(path: &str) -> io::Result<()> {
    let wide_path: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide_path` is a valid null-terminated UTF-16 string live for the duration of this
    // call; `WaitNamedPipeW` reads it but does not retain the pointer past returning.
    let succeeded = unsafe { WaitNamedPipeW(wide_path.as_ptr(), NMPWAIT_USE_DEFAULT_WAIT) };
    if succeeded == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Checks whether a named pipe exists at `path` at all — regardless of whether an instance is
/// free to connect to right now — via `WaitNamedPipeW` with `NMPWAIT_NOWAIT`, which returns
/// immediately instead of waiting. Used once at startup (see `main.rs`) to fail fast and loudly if
/// `--pipe-path` is wrong or the real agent isn't running, rather than only discovering that on
/// the first real relayed connection, potentially much later.
///
/// The distinction that makes this work: if the pipe exists but every instance is currently busy,
/// `WaitNamedPipeW` still fails (there's nothing to wait for with `NMPWAIT_NOWAIT`), but with
/// `ERROR_SEM_TIMEOUT` — only a pipe that doesn't exist under this name at all fails with
/// `ERROR_FILE_NOT_FOUND`. So busy-but-real and genuinely-absent are told apart by the specific
/// error code, not just success/failure.
pub fn exists(path: &str) -> bool {
    let wide_path: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: as in `wait_for_free_instance`.
    let succeeded = unsafe { WaitNamedPipeW(wide_path.as_ptr(), NMPWAIT_NOWAIT) };
    if succeeded != 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() != Some(ERROR_FILE_NOT_FOUND as i32)
}

/// Opens a fresh client handle on `path`.
///
/// **History**: this used to retry via a blind polling loop (up to 4s), which was removed
/// entirely once the Rust load-test tool quantified its cost under sustained load (multi-second
/// p99s hidden inside a single opaque "request"). It was then replaced with a single
/// `WaitNamedPipeW`-then-retry-once attempt — a real OS wait instead of blind polling, avoiding
/// the need to guess the agent's concurrency ceiling the way a userspace semaphore would have.
///
/// **That single-retry version turned out to still fail constantly under real sustained
/// contention** — a 32-client load-test run against the real relay produced 60,200 request
/// errors, **all** `ERROR_PIPE_BUSY` (confirmed via the server's own log: zero `ERROR_SEM_TIMEOUT`
/// instances), meaning `WaitNamedPipeW` itself was doing its job (a free instance really did show
/// up), but by the time the single follow-up `open` ran, another one of the many other waiters —
/// all woken by the *same* freed instance at once — had already grabbed it. A single retry can't
/// survive that: it needs to keep trying.
///
/// **Current design**: loop — on `ERROR_PIPE_BUSY`, wait via `WaitNamedPipeW`, sleep a small random
/// jitter (`settings.jitter_max_ms`, default 50ms), then retry — until the open succeeds, fails
/// with a genuinely different error, or `settings.connect_timeout` (default 5s) elapses. The
/// jitter exists specifically to break the thundering herd above: without it, every waiter woken
/// by the same freed instance retries in the same instant and only one can win, so the rest just
/// loop back into another synchronized pile-up; spreading retries out over a random window gives
/// each waiter a real chance to grab a slot before the next one frees. The overall timeout keeps
/// this from becoming the old unbounded-feeling retry in spirit — a saturated agent still
/// eventually produces an honest failure instead of hanging forever — while giving real
/// contention enough room to actually resolve instead of failing after a single round. Both
/// numbers are configurable (CLI flag or env var, see `config.rs`) rather than hardcoded, since
/// the right values depend on the real agent's capacity, which this project can't know in advance.
pub async fn connect(path: &str, settings: PipeSettings) -> Result<NamedPipeClient, PipeError> {
    let attempt = async {
        loop {
            match ClientOptions::new().open(path) {
                Ok(pipe) => return Ok(pipe),
                Err(source) if source.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                    let wait_path = path.to_string();
                    // The wait's own outcome (freed vs. its own short timeout) doesn't need
                    // separate handling: either way we just retry the open, which is the actual
                    // source of truth for whether a slot is available.
                    let _ = tokio::task::spawn_blocking(move || wait_for_free_instance(&wait_path))
                        .await
                        .expect("wait_for_free_instance blocking task panicked");

                    let jitter_ms = rand::random_range(0..=settings.jitter_max_ms);
                    tokio::time::sleep(Duration::from_millis(jitter_ms)).await;
                }
                Err(source) => return Err(source),
            }
        }
    };

    match tokio::time::timeout(settings.connect_timeout, attempt).await {
        Ok(result) => result.map_err(|source| PipeError::Connect {
            path: path.to_string(),
            source,
        }),
        Err(_elapsed) => Err(PipeError::Connect {
            path: path.to_string(),
            source: io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "pipe stayed busy for the whole {:?} connect timeout",
                    settings.connect_timeout
                ),
            ),
        }),
    }
}

/// Relays one accepted yamux stream to a fresh named-pipe connection. Errors are logged, not
/// propagated — one bad stream must not take down the server's accept loop (see `main.rs`, which
/// spawns this per accepted stream).
///
/// Deliberately **not** `sockrelay_common::relay::relay` (`copy_bidirectional`) here.
/// `copy_bidirectional` shuts down one direction on EOF, then *waits for the other direction to
/// also complete on its own* — relying on both ends supporting a real half-close.
/// `NamedPipeClient::poll_shutdown` is a no-op (confirmed directly in tokio's source: a plain
/// named pipe, like a plain anonymous pipe — see `client/src/spawn.rs`'s note on `ChildStdin` for
/// the same class of issue — has no OS-level half-close, only a real close signals EOF to the
/// peer), and a real ssh-agent doesn't proactively close its end after answering one request (a
/// connection is allowed to carry more than one). Combined, the "wait for the other direction
/// too" half of `copy_bidirectional` would **never** resolve: every relayed connection leaks
/// forever — one yamux stream *and* one named-pipe handle per ssh-agent operation — until yamux's
/// default 512-stream cap is hit and the whole mux connection (and, per this project's "keep it
/// simple, just exit" design, the whole server process) dies. Found via this project's own
/// load-test tool (`loadtest/`) sending real fresh-connect-per-request traffic — exactly what a
/// real `ssh`/`ssh-add` invocation already does, so this isn't a synthetic-load-only concern; it
/// would eventually have hit this ceiling under perfectly ordinary use.
///
/// The fix: race both copy directions and, as soon as **either** one ends — the client closing
/// its write side (a real yamux FIN, which *is* reliably delivered) or the pipe erroring/closing
/// — drop both halves immediately instead of waiting for the other to also finish gracefully.
pub async fn relay_stream(pipe_path: &str, stream: yamux::Stream, settings: PipeSettings) {
    use tokio_util::compat::FuturesAsyncReadCompatExt;

    let pipe = match connect(pipe_path, settings).await {
        Ok(pipe) => pipe,
        Err(e) => {
            tracing::error!(error = %e, "failed to connect to the ssh-agent named pipe");
            return;
        }
    };
    relay(stream.compat(), pipe).await;
}

/// The actual relay logic, generic over the client-facing transport so it can be exercised in
/// tests without a real yamux connection (see the tests below, which pair a real named pipe with
/// a plain `tokio::io::duplex` standing in for the client side — the property under test is
/// specifically about *this* transport's real half-close behavior, not anything yamux-specific).
async fn relay<C, P>(client: C, pipe: P)
where
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite,
    P: tokio::io::AsyncRead + tokio::io::AsyncWrite,
{
    let (mut client_read, mut client_write) = tokio::io::split(client);
    let (mut pipe_read, mut pipe_write) = tokio::io::split(pipe);

    tokio::select! {
        result = tokio::io::copy(&mut client_read, &mut pipe_write) => {
            if let Err(e) = result {
                tracing::warn!(error = %e, "error relaying client -> agent");
            }
        }
        result = tokio::io::copy(&mut pipe_read, &mut client_write) => {
            if let Err(e) = result {
                tracing::warn!(error = %e, "error relaying agent -> client");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::windows::named_pipe::ServerOptions;

    fn unique_pipe_path(name: &str) -> String {
        format!(r"\\.\pipe\sockrelay-test-{}-{}", name, std::process::id())
    }

    /// Regression test for the hang this file's `relay` doc comment describes: a real named pipe
    /// standing in for the agent answers exactly once and then **never closes** (exactly what a
    /// real ssh-agent does — nothing here is contrived), while the client-facing side closes
    /// after reading its one response, same as any real `ssh`/`ssh-add` invocation.
    /// `copy_bidirectional` would hang here forever; this `relay` must return promptly instead.
    #[tokio::test]
    async fn relay_terminates_once_the_client_closes_even_though_the_agent_never_does() {
        let path = unique_pipe_path("relay-terminates");
        let mut agent = ServerOptions::new().create(&path).unwrap();

        tokio::spawn(async move {
            agent.connect().await.unwrap();
            let mut buf = [0u8; 5];
            agent.read_exact(&mut buf).await.unwrap();
            agent.write_all(b"reply").await.unwrap();
            // Deliberately never close `agent` from here — held open for the rest of the test,
            // exactly like a real ssh-agent connection that's allowed to carry more requests.
            std::future::pending::<()>().await
        });

        let pipe = connect(&path, PipeSettings::default()).await.unwrap();
        // A plain in-memory duplex genuinely supports a real half-close on `shutdown()` (unlike
        // `NamedPipeClient`), which is all that's needed to stand in for the client-facing side
        // here — the property under test doesn't depend on anything yamux-specific.
        let (mut client_side, server_side) = tokio::io::duplex(64);

        let relay_task = tokio::spawn(relay(server_side, pipe));

        client_side.write_all(b"hello").await.unwrap();
        let mut reply = [0u8; 5];
        client_side.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"reply");

        client_side.shutdown().await.unwrap();
        drop(client_side);

        tokio::time::timeout(std::time::Duration::from_secs(5), relay_task)
            .await
            .expect("relay() hung instead of returning once the client side closed")
            .unwrap();
    }

    #[tokio::test]
    async fn connect_relays_bytes_in_both_directions() {
        let path = unique_pipe_path("connect-echo");
        let mut server = ServerOptions::new().create(&path).unwrap();

        let server_task = tokio::spawn(async move {
            server.connect().await.unwrap();
            let mut buf = [0u8; 5];
            server.read_exact(&mut buf).await.unwrap();
            server.write_all(&buf).await.unwrap();
        });

        let mut client = connect(&path, PipeSettings::default()).await.unwrap();
        client.write_all(b"hello").await.unwrap();
        let mut buf = [0u8; 5];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");

        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn connect_fails_for_a_nonexistent_pipe() {
        let path = unique_pipe_path("does-not-exist");
        match connect(&path, PipeSettings::default()).await {
            Err(PipeError::Connect { .. }) => {}
            Ok(_) => panic!("expected connecting to a nonexistent pipe to fail"),
        }
    }

    #[test]
    fn exists_is_false_for_a_nonexistent_pipe() {
        let path = unique_pipe_path("exists-check-missing");
        assert!(!exists(&path));
    }

    #[tokio::test]
    async fn exists_is_true_for_a_pipe_with_a_free_instance() {
        let path = unique_pipe_path("exists-check-free");
        let _server = ServerOptions::new().create(&path).unwrap();
        assert!(exists(&path));
    }

    /// The whole point of `exists`: a busy pipe (every instance occupied) must still be reported
    /// as existing — only a truly absent pipe name should read as "doesn't exist".
    #[tokio::test]
    async fn exists_is_true_even_when_every_instance_is_busy() {
        let path = unique_pipe_path("exists-check-busy");
        let _server = ServerOptions::new().max_instances(1).create(&path).unwrap();
        let _client = ClientOptions::new().open(&path).unwrap();

        assert!(exists(&path));
    }

    /// Regression test for the `WaitNamedPipeW`-based wait added to `connect`: with only one pipe
    /// instance available, a second connect attempt must hit `ERROR_PIPE_BUSY` and then, once the
    /// instance is freed, succeed on its single follow-up retry — instead of failing immediately,
    /// which is what a single bare `ClientOptions::open` attempt (the old, pre-wait behavior)
    /// would do.
    #[tokio::test]
    async fn connect_waits_for_a_busy_instance_to_free_up_and_then_succeeds() {
        let path = unique_pipe_path("busy-then-free");
        let server = ServerOptions::new().max_instances(1).create(&path).unwrap();

        // Occupies the sole instance — a client's `CreateFile` completes without requiring the
        // server to have called `connect()` first (confirmed in tokio's own `disconnect()` doc
        // example, which opens the client before ever awaiting `server.connect()`).
        let client_a = ClientOptions::new().open(&path).unwrap();

        // Sanity-check that the instance really is exhausted right now, i.e. that this test is
        // actually exercising the busy path rather than trivially passing.
        match ClientOptions::new().open(&path) {
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {}
            other => panic!("expected the sole instance to be busy, got {other:?}"),
        }

        let connect_task =
            tokio::spawn(async move { connect(&path, PipeSettings::default()).await });

        // Gives the spawned task time to run its first (busy) open attempt and enter the real
        // `WaitNamedPipeW` blocking call before we free the instance below.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        // Disconnecting alone does not re-arm the instance for a *new* connection — per the
        // documented server pattern (`CreateNamedPipe` once, then loop { `ConnectNamedPipe`;
        // serve; `DisconnectNamedPipe` }), only the very first connection is accepted without an
        // explicit `connect()`/`ConnectNamedPipe` call; every connection after a disconnect needs
        // a fresh one to put the instance back into a listening state. Confirmed by this test:
        // omitting the `server.connect()` call below reliably reproduced the exact
        // `WaitNamedPipeW` timeout (`ERROR_SEM_TIMEOUT`, os error 121) this test now guards
        // against, since the instance never actually became available for `connect_task`'s retry.
        server.disconnect().unwrap();
        drop(client_a);
        server.connect().await.unwrap();

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), connect_task)
            .await
            .expect("connect() did not return promptly after the instance was freed")
            .unwrap();
        result.expect("connect() should succeed once the busy instance is freed");
    }
}
