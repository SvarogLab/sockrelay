mod config;
mod mux;
mod relay;
mod server_log;
mod spawn;
#[cfg(test)]
mod test_support;
mod unix_listener;

use std::io::IsTerminal;
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use futures::FutureExt;
use tokio_util::compat::FuturesAsyncReadCompatExt;

use config::{Config, ListenOn};
use mux::MuxHandle;
use spawn::{SpawnError, SpawnedServer};

/// Resolves once the mux connection to the spawned server is no longer usable, for any reason
/// (mux driver ended, spawned process exited, whichever comes first — see [`cold_start`]).
///
/// `Shared` (not a plain one-shot `BoxFuture`) specifically so it can be polled twice without
/// being consumed: once during the startup grace period (see [`run`]), and again by the
/// long-running background watcher — a plain future would be dropped (and the watch lost) if the
/// first poll won a `tokio::time::timeout` race.
type DiedFuture = futures::future::Shared<futures::future::BoxFuture<'static, ()>>;

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
    // SAFETY: still single-threaded — no runtime, no other thread exists yet. Unsetting
    // `LISTEN_PID`/`LISTEN_FDS` keeps the spawned server from seeing sockets meant for this
    // process; the fds themselves come back close-on-exec.
    let inherited_sockets: Vec<RawFd> = match unsafe { sd_notify::listen_fds_and_unset_env() } {
        Ok(fds) => fds.collect(),
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

    let log_filter = match config::effective_log_filter(std::env::var("RUST_LOG").ok()) {
        Ok(filter) => filter,
        Err(e) => {
            eprintln!("sockrelay-client: invalid RUST_LOG: {e}");
            return ExitCode::FAILURE;
        }
    };

    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_new(&log_filter)
                .expect("effective_log_filter validated it"),
        )
        .init();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build the tokio runtime")
        .block_on(run(config, listen_on, log_filter))
}

/// Claims the socket, starts the Windows server and relays connections until the connection to
/// the server dies, which ends the process with a non-zero status for systemd to restart it.
/// Returns only on a startup failure.
async fn run(config: Config, listen_on: ListenOn, log_filter: String) -> ExitCode {
    let listener = match listen_on {
        ListenOn::Path { path, mode } => unix_listener::bind(&path, mode)
            .await
            .map_err(|e| tracing::error!(error = %e, "failed to bind the unix socket")),
        ListenOn::Inherited(fd) => unix_listener::from_inherited(fd)
            .map_err(|e| tracing::error!(error = %e, "failed to use the socket systemd passed")),
    };
    let Ok(listener) = listener else {
        return ExitCode::FAILURE;
    };

    // Spawned exactly once, here, before accepting any connections. If this fails, or if the
    // connection dies later (see the watcher below), the process exits rather than retrying.
    let (handle, died, server_stderr) =
        match cold_start(config.server_exe.clone(), config.server_args(&log_filter)).await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(error = %e, "failed to start the windows server");
                return ExitCode::FAILURE;
            }
        };

    // Wired up before the grace period below: the server's own "pipe does not exist" diagnostic
    // arrives during that window, and nothing reads a piped stderr unless something does.
    tokio::spawn(server_log::forward(server_stderr, tokio::io::stderr()));

    // `died.clone()` leaves `died` itself for the long-running watcher below.
    if tokio::time::timeout(STARTUP_GRACE_PERIOD, died.clone())
        .await
        .is_ok()
    {
        tracing::error!(
            "the spawned Windows server exited immediately after starting — check \
             --pipe-path and that the real agent is running"
        );
        return ExitCode::FAILURE;
    }

    tracing::info!("started");
    // With `Type=notify`, systemd counts the service as started only now, after the grace period.
    if let Err(e) = sd_notify::notify(&[sd_notify::NotifyState::Ready]) {
        tracing::warn!(error = %e, "failed to notify systemd of readiness");
    }

    tokio::spawn(async move {
        died.await;
        tracing::error!("mux connection to the server died, exiting");
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

/// Spawns the Windows server and builds the mux connection over its joined stdio. This is the
/// only place that touches the real `SpawnedServer`/`mux::client` — it isn't unit tested (there's
/// no Windows binary to spawn on this host); exercise it against a real build with the recipe in
/// this crate's README.
///
/// The returned "died" future watches *both* the mux driver and the child process directly (via
/// `SpawnedServer::wait()`), resolving on whichever ends first — the daemon exits as soon as
/// either side of the connection is no longer usable, rather than lingering as a half-alive
/// process. The returned `ChildStderr` is the server's own diagnostic stream (see
/// `server_log::forward`) — taken here, before `server` moves into the "died" future's closure,
/// since it can't be taken out again afterward.
async fn cold_start(
    server_exe_path: PathBuf,
    server_args: Vec<String>,
) -> Result<(MuxHandle, DiedFuture, tokio::process::ChildStderr), SpawnError> {
    let mut server = SpawnedServer::spawn(server_exe_path, server_args).await?;
    let io = server.take_io()?;
    let stderr = server.take_stderr()?;
    let (handle, driver) = mux::client(io);
    let mut driver_task = tokio::spawn(driver);

    let died: DiedFuture = (Box::pin(async move {
        let mut server = server;
        // `&mut driver_task` (not moving it in) so that, if the child exits first, the still-
        // owned handle can be aborted explicitly instead of merely dropped — dropping a
        // `JoinHandle` only detaches the task, it doesn't stop it.
        tokio::select! {
            _ = &mut driver_task => {}
            _ = server.wait() => {
                driver_task.abort();
            }
        }
    }) as futures::future::BoxFuture<'static, ()>)
        .shared();

    Ok((handle, died, stderr))
}
