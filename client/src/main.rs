mod config;
mod socket;

use std::io;
use std::ops::ControlFlow;
use std::os::fd::RawFd;
use std::pin::{Pin, pin};
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use librelay::client::log::{self, LogFilter, LogFilterError};
use librelay::client::mux::MuxHandle;
use librelay::client::relay::{self, Link};
use librelay::client::{lifecycle, systemd};
use tokio::net::{UnixListener, UnixStream};
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;
use tokio_util::compat::FuturesAsyncReadCompatExt;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use config::Config;
use socket::SocketError;

/// How long the server gets to prove it stays up before systemd hears the service is ready. The
/// failure this catches — `sockrelay-server` exiting at once because `--pipe-path` names no running
/// agent — shows within milliseconds, and the wait is paid on every start, so it stays short.
const STARTUP_GRACE_PERIOD: Duration = Duration::from_millis(500);

/// Bounds both halves of a graceful stop: the wait for connections in flight, and then the wait
/// for the server to exit.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
enum StartupError {
    #[error("invalid socket activation environment: {0}")]
    ListenFds(#[source] io::Error),
    #[error(transparent)]
    Socket(#[from] SocketError),
    #[error(transparent)]
    LogFilter(#[from] LogFilterError),
    #[error("failed to build the tokio runtime: {0}")]
    Runtime(#[source] io::Error),
}

/// What the relay starts with, all of it settled before the runtime exists.
struct Startup {
    runtime: Runtime,
    config: Config,
    socket: RawFd,
    server_args: Vec<String>,
}

fn main() -> ExitCode {
    match start_up() {
        Ok(Startup {
            runtime,
            config,
            socket,
            server_args,
        }) => runtime.block_on(run(config, socket, server_args)),
        Err(e) => {
            eprintln!("sockrelay-client: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The socket-activation environment is read first: unsetting it is only sound while the process
/// has a single thread, before the runtime starts any.
fn start_up() -> Result<Startup, StartupError> {
    // SAFETY: still single-threaded — no runtime, no other thread exists yet.
    let inherited = unsafe { systemd::take_listen_fds() }.map_err(StartupError::ListenFds)?;
    let config = Config::parse();
    let socket = socket::listening_socket(&inherited)?;
    let log_filter = LogFilter::from_env()?;
    let server_args = config.server_args(&log_filter);
    log::init(log_filter);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(StartupError::Runtime)?;
    Ok(Startup {
        runtime,
        config,
        socket,
        server_args,
    })
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
    let (handle, mut link) =
        match start_link(config.server_exe, server_args, shutdown.as_mut()).await {
            ControlFlow::Continue(started) => started,
            ControlFlow::Break(code) => return code,
        };

    let mut connections = Connections::accept(
        listener,
        Duration::from_millis(config.accept_error_backoff_ms),
        handle.clone(),
    );
    if let ControlFlow::Break(code) =
        serve_until_stopped(&mut link, &mut connections.accepting, shutdown).await
    {
        return code;
    }

    tracing::info!("stopping");
    connections.stop().await;
    close_link(link, handle).await
}

type ShutdownSignal<'a> = Pin<&'a mut (dyn Future<Output = io::Result<()>> + Send)>;

async fn start_link(
    server_exe: std::path::PathBuf,
    server_args: Vec<String>,
    shutdown: ShutdownSignal<'_>,
) -> ControlFlow<ExitCode, (MuxHandle, Link)> {
    let started = tokio::select! {
        started = relay::start(server_exe, server_args, Some(STARTUP_GRACE_PERIOD)) => started,
        signal = shutdown => return ControlFlow::Break(stopped(signal, "stopped while starting")),
    };
    match started {
        Ok(started) => {
            tracing::info!("started");
            if let Err(e) = systemd::notify_ready() {
                tracing::warn!(error = %e, "failed to notify systemd of readiness");
            }
            ControlFlow::Continue(started)
        }
        Err(e) => {
            tracing::error!(
                error = %e,
                "failed to start the windows server — check --pipe-path and that the real agent \
                 is running"
            );
            ControlFlow::Break(ExitCode::FAILURE)
        }
    }
}

/// Relays until a stop signal, which continues to a graceful stop, or until the link or the socket
/// fails, which ends the relay for systemd to restart.
async fn serve_until_stopped(
    link: &mut Link,
    accepting: &mut JoinHandle<io::Error>,
    shutdown: ShutdownSignal<'_>,
) -> ControlFlow<ExitCode> {
    tokio::select! {
        died = link.died() => {
            tracing::error!(reason = %died, "lost the windows server, exiting");
            ControlFlow::Break(ExitCode::FAILURE)
        }
        failed = accepting => {
            match failed {
                Ok(e) => tracing::error!(error = %e, "can't accept connections, exiting"),
                Err(e) => tracing::error!(error = %e, "accepting connections stopped, exiting"),
            }
            ControlFlow::Break(ExitCode::FAILURE)
        }
        signal = shutdown => match signal {
            Ok(()) => ControlFlow::Continue(()),
            Err(e) => {
                tracing::error!(error = %e, "can't watch for a stop signal, exiting");
                ControlFlow::Break(ExitCode::FAILURE)
            }
        },
    }
}

async fn close_link(link: Link, handle: MuxHandle) -> ExitCode {
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

/// The accepted connections, each relayed on a task of its own, and the accept loop that feeds
/// them.
struct Connections {
    accepting: JoinHandle<io::Error>,
    relaying: TaskTracker,
    cancel: CancellationToken,
}

impl Connections {
    fn accept(listener: UnixListener, error_backoff: Duration, handle: MuxHandle) -> Self {
        let relaying = TaskTracker::new();
        let cancel = CancellationToken::new();
        let accepting = tokio::spawn(accept_connections(
            listener,
            error_backoff,
            handle,
            relaying.clone(),
            cancel.clone(),
        ));
        Self {
            accepting,
            relaying,
            cancel,
        }
    }

    /// Stops accepting and gives connections in flight [`SHUTDOWN_TIMEOUT`] before cancelling
    /// them: each holds a handle, and the link only closes once the last one is gone.
    async fn stop(self) {
        self.accepting.abort();
        self.relaying.close();
        if tokio::time::timeout(SHUTDOWN_TIMEOUT, self.relaying.wait())
            .await
            .is_err()
        {
            tracing::warn!("connections still open after {SHUTDOWN_TIMEOUT:?}, closing them");
            self.cancel.cancel();
            self.relaying.wait().await;
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
        let mut _unacknowledged = Vec::new();
        for _ in 0..256 {
            _unacknowledged.push(handle.open_stream().await.unwrap());
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
