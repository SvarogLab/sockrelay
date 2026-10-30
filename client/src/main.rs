mod config;

use std::os::fd::RawFd;
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

/// How long a graceful stop waits for connections still in flight, and then for the server to
/// exit, before giving up on each.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Runs under systemd, socket-activated, logging to stderr, which systemd sends to the journal.
/// The socket-activation environment is read before the tokio runtime starts: unsetting it is
/// only sound while this process has a single thread.
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

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build the tokio runtime")
        .block_on(run(config, socket, server_args))
}

/// Relays connections until the link to the server is lost — a failure, for systemd to restart
/// the service — or systemd stops the service, which ends gracefully.
async fn run(config: Config, socket: RawFd, server_args: Vec<String>) -> ExitCode {
    let listener = match systemd::listener_from_fd(socket) {
        Ok(listener) => listener,
        Err(e) => {
            tracing::error!(error = %e, "failed to use the socket systemd passed");
            return ExitCode::FAILURE;
        }
    };

    // Spawned exactly once, before accepting any connection; never respawned.
    let (handle, mut link) =
        match relay::start(config.server_exe, server_args, Some(STARTUP_GRACE_PERIOD)).await {
            Ok(started) => started,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "failed to start the windows server — check --pipe-path and that the real \
                     agent is running"
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
    let accepting = tokio::spawn(accept_connections(
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
        signal = lifecycle::shutdown_signal() => {
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
            tracing::warn!(error = %e, "stopped without the windows server exiting cleanly");
            ExitCode::SUCCESS
        }
    }
}

/// Accepts connections for as long as it runs, relaying each over its own stream on a task in
/// `connections`, so a graceful stop can wait for them, and cancelled through `stop` once that wait
/// runs out. A failed `accept()` — fd exhaustion under
/// a burst of parallel connections, say — is retried after `error_backoff` instead of taking the
/// relay down.
async fn accept_connections(
    listener: UnixListener,
    error_backoff: Duration,
    handle: MuxHandle,
    connections: TaskTracker,
    stop: CancellationToken,
) {
    loop {
        match listener.accept().await {
            Ok((connection, _addr)) => {
                connections.spawn(relay_connection(connection, handle.clone(), stop.clone()));
            }
            Err(e) => {
                tracing::warn!(error = %e, "accept() failed, retrying");
                tokio::time::sleep(error_backoff).await;
            }
        }
    }
}

/// Relays one connection over its own stream until either side ends it, or `stop` is cancelled.
async fn relay_connection(mut connection: UnixStream, handle: MuxHandle, stop: CancellationToken) {
    let mut stream = match handle.open_stream().await {
        Ok(stream) => stream.compat(),
        Err(e) => {
            tracing::error!(error = %e, "failed to open a yamux stream");
            return;
        }
    };
    tokio::select! {
        relayed = tokio::io::copy_bidirectional(&mut connection, &mut stream) => {
            if let Err(e) = relayed {
                tracing::warn!(error = %e, "relay ended with an error");
            }
        }
        () = stop.cancelled() => {}
    }
}
