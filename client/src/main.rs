mod config;
mod mux;
mod relay;
#[cfg(test)]
mod test_support;
mod unix_listener;

use std::os::fd::RawFd;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use librelay::client::log::{self, LogFilter};
use librelay::client::spawn::Server;
use librelay::client::{lifecycle, systemd};
use tokio_util::compat::FuturesAsyncReadCompatExt;

use config::{Config, ListenOn};
use mux::MuxHandle;

/// How long to wait, after a successful cold start, for the spawned Windows server to prove it's
/// actually still alive before reporting startup success.
///
/// There's no positive "I'm healthy" signal to wait for here — only the absence of a death event
/// — so `tokio::time::timeout` always waits out this *entire* duration on the ordinary, healthy
/// path (a timeout can't return early just because nothing bad happened yet). That means this
/// value is a straight tax on every successful start, not just a ceiling — so it must stay close
/// to the failure signal's own actual latency rather than being a generous round number. The
/// specific failure this exists to catch (`sockrelay-server` exiting immediately because
/// `--pipe-path` doesn't correspond to a real, running agent — see its own `main.rs`) happens
/// within milliseconds of being spawned, since the check is one non-blocking `WaitNamedPipeW`
/// syscall; this only needs enough slack over that for ordinary process-spawn/IPC scheduling
/// jitter, not multiple seconds of margin.
const STARTUP_GRACE_PERIOD: Duration = Duration::from_millis(500);

/// Runs under systemd, either socket-activated or binding the socket itself (see
/// [`Config::listen_on`]), logging to stderr, which systemd sends to the journal.
///
/// The socket-activation environment is read before the tokio runtime starts: unsetting it is
/// only sound while this process has a single thread.
fn main() -> ExitCode {
    // SAFETY: still single-threaded — no runtime, no other thread exists yet.
    let inherited_sockets: Vec<RawFd> = match unsafe { systemd::take_listen_fds() } {
        Ok(fds) => fds,
        Err(e) => {
            eprintln!("sockrelay-client: invalid socket activation environment: {e}");
            return ExitCode::FAILURE;
        }
    };

    let config = Config::parse();

    let listen_on = match config.listen_on(&inherited_sockets) {
        Ok(listen_on) => listen_on,
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
        .block_on(run(config, listen_on, server_args))
}

/// Claims the socket, starts the Windows server and relays connections until the connection to
/// the server dies, which ends the process with a non-zero status for systemd to restart it.
/// Returns only on a startup failure.
async fn run(config: Config, listen_on: ListenOn, server_args: Vec<String>) -> ExitCode {
    let listener = match listen_on {
        ListenOn::Path { path, mode } => unix_listener::bind(&path, mode)
            .await
            .map_err(|e| tracing::error!(error = %e, "failed to bind the unix socket")),
        ListenOn::Inherited(fd) => systemd::listener_from_fd(fd)
            .map_err(|e| tracing::error!(error = %e, "failed to use the socket systemd passed")),
    };
    let Ok(listener) = listener else {
        return ExitCode::FAILURE;
    };

    // Spawned exactly once, here, before accepting any connections. If this fails, or if the
    // connection dies later (see the watcher below), the process exits rather than retrying.
    let mut server = match Server::spawn(config.server_exe.clone(), server_args).await {
        Ok(server) => server,
        Err(e) => {
            tracing::error!(error = %e, "failed to start the windows server");
            return ExitCode::FAILURE;
        }
    };

    // Wired up before the grace period below: the server's own "pipe does not exist" diagnostic
    // arrives during that window, and nothing reads a piped stderr unless something does.
    tokio::spawn(log::forward_server_stderr(
        server.stderr,
        tokio::io::stderr(),
    ));

    let (handle, driver) = mux::client(tokio::io::join(server.stdout, server.stdin));
    let mut driver_task = tokio::spawn(driver);

    if let Err(e) = lifecycle::wait_alive(&mut server.child, STARTUP_GRACE_PERIOD).await {
        tracing::error!(
            error = %e,
            "the spawned Windows server didn't stay up — check --pipe-path and that the real \
             agent is running"
        );
        return ExitCode::FAILURE;
    }

    tracing::info!("started");
    // With `Type=notify`, systemd counts the service as started only now, after the grace period.
    if let Err(e) = systemd::notify_ready() {
        tracing::warn!(error = %e, "failed to notify systemd of readiness");
    }

    let mut child = server.child;
    tokio::spawn(async move {
        match lifecycle::died(&mut child, &mut driver_task).await {
            lifecycle::Died::Exited(status) => {
                // Dropping a `JoinHandle` only detaches its task; the driver has to be stopped
                // explicitly.
                driver_task.abort();
                match status {
                    Ok(status) => tracing::error!(%status, "the windows server exited, exiting"),
                    Err(e) => tracing::error!(error = %e, "lost the windows server, exiting"),
                }
            }
            lifecycle::Died::TransportEnded => {
                tracing::error!("mux connection to the server died, exiting");
            }
        }
        std::process::exit(1);
    });

    let accept_error_backoff = Duration::from_millis(config.accept_error_backoff_ms);
    unix_listener::accept_loop(listener, accept_error_backoff, move |unix_stream| {
        handle_connection(unix_stream, handle.clone())
    })
    .await;
    unreachable!("the accept loop runs for as long as the process does")
}

async fn handle_connection(mut unix_stream: tokio::net::UnixStream, handle: MuxHandle) {
    let stream = match handle.open_stream().await {
        Ok(stream) => stream,
        Err(e) => {
            tracing::error!(error = %e, "failed to open a yamux stream");
            return;
        }
    };
    let mut stream = stream.compat();

    if let Err(e) = relay::relay(&mut unix_stream, &mut stream).await {
        tracing::warn!(error = %e, "relay ended with an error");
    }
}
