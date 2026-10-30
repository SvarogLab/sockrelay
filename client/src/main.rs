mod config;

use std::io;
use std::os::fd::RawFd;
use std::pin::pin;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use librelay::client::log::{self, LogFilter};
use librelay::client::mux::MuxHandle;
use librelay::client::{lifecycle, relay, systemd};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::compat::FuturesAsyncReadCompatExt;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use config::Config;

/// How long the server gets to prove it stays up before systemd hears the service is ready. The
/// failure this catches — `sockrelay-server` exiting at once because `--pipe-path` names no running
/// agent — shows within milliseconds, and the wait is paid on every start, so it stays short.
const STARTUP_GRACE_PERIOD: Duration = Duration::from_millis(500);

/// Bounds both halves of a graceful stop: the wait for connections in flight, and then the wait
/// for the server to exit.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// The socket-activation environment is read before the tokio runtime starts: unsetting it is only
/// sound while this process has a single thread.
fn main() -> ExitCode {
    // SAFETY: still single-threaded — no runtime, no other thread exists yet.
    let inherited: Vec<RawFd> = match unsafe { systemd::take_listen_fds() } {
        Ok(fds) => fds,
        Err(e) => {
            eprintln!("sockrelay-client: invalid socket activation environment: {e}");
            return ExitCode::FAILURE;
        }
    };
    let config = Config::parse();
    let socket = match config::listening_socket(&inherited) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("sockrelay-client: {e}");
            return ExitCode::FAILURE;
        }
    };
    let log_filter = match LogFilter::from_env() {
        Ok(filter) => filter,
        Err(e) => {
            eprintln!("sockrelay-client: {e}");
            return ExitCode::FAILURE;
        }
    };
    let server_args = config.server_args(&log_filter);
    log::init(log_filter);

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            tracing::error!(error = %e, "failed to build the tokio runtime");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(run(config, socket, server_args))
}

async fn run(config: Config, socket: RawFd, server_args: Vec<String>) -> ExitCode {
    let listener = match systemd::listener_from_fd(socket) {
        Ok(listener) => listener,
        Err(e) => {
            tracing::error!(error = %e, "failed to use the socket systemd passed");
            return ExitCode::FAILURE;
        }
    };

    // Watched from before the server is spawned: until then a stop signal would kill the client
    // outright and leave the server to `KillMode=mixed`.
    let mut shutdown = pin!(lifecycle::shutdown_signal());
    let started = tokio::select! {
        started = relay::start(config.server_exe, server_args, Some(STARTUP_GRACE_PERIOD)) => started,
        signal = &mut shutdown => return stopped(signal, "stopped while starting"),
    };
    let (handle, mut link) = match started {
        Ok(started) => started,
        Err(e) => {
            tracing::error!(
                error = %e,
                "failed to start the windows server — check --pipe-path and that the real agent \
                 is running"
            );
            return ExitCode::FAILURE;
        }
    };
    tracing::info!("started");
    if let Err(e) = systemd::notify_ready() {
        tracing::warn!(error = %e, "failed to notify systemd of readiness");
    }

    let connections = TaskTracker::new();
    let stop_connections = CancellationToken::new();
    let mut accepting = tokio::spawn(accept_connections(
        listener,
        Duration::from_millis(config.accept_error_backoff_ms),
        handle.clone(),
        connections.clone(),
        stop_connections.clone(),
    ));

    tokio::select! {
        died = link.died() => {
            tracing::error!(reason = %died, "lost the windows server, exiting");
            return ExitCode::FAILURE;
        }
        failed = &mut accepting => {
            match failed {
                Ok(e) => tracing::error!(error = %e, "can't accept connections, exiting"),
                Err(e) => tracing::error!(error = %e, "accepting connections stopped, exiting"),
            }
            return ExitCode::FAILURE;
        }
        signal = &mut shutdown => {
            if let Err(e) = signal {
                tracing::error!(error = %e, "can't watch for a stop signal, exiting");
                return ExitCode::FAILURE;
            }
        }
    }

    tracing::info!("stopping");
    accepting.abort();
    connections.close();
    if tokio::time::timeout(SHUTDOWN_TIMEOUT, connections.wait())
        .await
        .is_err()
    {
        tracing::warn!("connections still open after {SHUTDOWN_TIMEOUT:?}, closing them");
        // Each holds a handle, and the link only closes once the last one is gone.
        stop_connections.cancel();
        connections.wait().await;
    }
    match link.close(handle, SHUTDOWN_TIMEOUT).await {
        Ok(status) => {
            tracing::info!(%status, "stopped");
            ExitCode::SUCCESS
        }
        Err(e) => {
            tracing::error!(error = %e, "the windows server didn't exit cleanly");
            ExitCode::FAILURE
        }
    }
}

fn stopped(signal: io::Result<()>, message: &str) -> ExitCode {
    match signal {
        Ok(()) => {
            tracing::info!("{message}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            tracing::error!(error = %e, "can't watch for a stop signal, exiting");
            ExitCode::FAILURE
        }
    }
}

/// Relays each connection on a task in `connections`, so a graceful stop can wait for them, and
/// `stop` cancels them once that wait runs out. A failed `accept()` — fd exhaustion under a burst
/// of parallel connections, say — is retried after `error_backoff`; one that says the socket itself
/// is unusable ends accepting, and the relay with it, for systemd to restart.
async fn accept_connections(
    listener: UnixListener,
    error_backoff: Duration,
    handle: MuxHandle,
    connections: TaskTracker,
    stop: CancellationToken,
) -> io::Error {
    loop {
        match listener.accept().await {
            Ok((connection, _addr)) => {
                connections.spawn(relay_connection(connection, handle.clone(), stop.clone()));
            }
            Err(e) if socket_is_unusable(&e) => return e,
            Err(e) => {
                tracing::warn!(error = %e, "accept() failed, retrying");
                tokio::time::sleep(error_backoff).await;
            }
        }
    }
}

/// The `accept(2)` errors that come from the socket rather than from the moment.
fn socket_is_unusable(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EBADF | libc::EINVAL | libc::ENOTSOCK | libc::EOPNOTSUPP)
    )
}

/// Opening the stream is cancellable too: yamux holds a new stream back while too many wait for
/// the server's acknowledgement.
async fn relay_connection(connection: UnixStream, handle: MuxHandle, stop: CancellationToken) {
    stop.run_until_cancelled(relay(connection, &handle)).await;
}

async fn relay(mut connection: UnixStream, handle: &MuxHandle) {
    let mut stream = match handle.open_stream().await {
        Ok(stream) => stream.compat(),
        Err(e) => {
            tracing::error!(error = %e, "failed to open a yamux stream");
            return;
        }
    };
    if let Err(e) = tokio::io::copy_bidirectional(&mut connection, &mut stream).await {
        tracing::warn!(error = %e, "relay ended with an error");
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::fd::{FromRawFd, IntoRawFd};
    use std::os::unix::net::UnixStream as StdUnixStream;

    use librelay::client::mux;

    use super::*;

    #[tokio::test]
    async fn a_connection_still_waiting_for_its_stream_stops_when_cancelled() {
        let (near, _far) = tokio::io::duplex(64 * 1024);
        let (handle, driver) = mux::client(near);
        tokio::spawn(driver);
        // yamux holds back a new stream once this many are waiting for the server's ACK.
        let mut unacknowledged = Vec::new();
        for _ in 0..256 {
            unacknowledged.push(handle.open_stream().await.unwrap());
        }
        let stop = CancellationToken::new();
        stop.cancel();
        let (connection, _peer) = UnixStream::pair().unwrap();

        tokio::time::timeout(
            Duration::from_secs(2),
            relay_connection(connection, handle, stop),
        )
        .await
        .expect("the connection ignored the cancellation");
    }

    #[tokio::test]
    async fn accepting_ends_on_an_error_retrying_cannot_fix() {
        let (connected, mut peer) = StdUnixStream::pair().unwrap();
        // tokio only calls `accept()` once the socket reads as ready.
        peer.write_all(b"x").unwrap();
        // SAFETY: the fd was just released by `into_raw_fd`, so the listener is its sole owner.
        let not_listening =
            unsafe { std::os::unix::net::UnixListener::from_raw_fd(connected.into_raw_fd()) };
        not_listening.set_nonblocking(true).unwrap();
        let listener = UnixListener::from_std(not_listening).unwrap();
        let (near, _far) = tokio::io::duplex(1024);
        let (handle, _driver) = mux::client(near);

        let accepting = accept_connections(
            listener,
            Duration::from_millis(10),
            handle,
            TaskTracker::new(),
            CancellationToken::new(),
        );
        let ended = tokio::time::timeout(Duration::from_secs(2), accepting)
            .await
            .expect("accepting kept retrying an error that can't go away");
        assert_eq!(ended.raw_os_error(), Some(libc::EINVAL));
    }
}
