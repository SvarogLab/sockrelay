mod config;
mod log_sink;
mod mux;
mod relay;
mod spawn;
#[cfg(test)]
mod test_support;
mod unix_listener;

use std::io::IsTerminal;
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use futures::FutureExt;
use tokio_util::compat::FuturesAsyncReadCompatExt;

use config::Config;
use log_sink::LogSink;
use mux::MuxHandle;
use spawn::{SpawnError, SpawnedServer};
use unix_listener::ListenError;

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

/// How this process reports a startup outcome — the only thing that differs between `--foreground`
/// and the default daemonized path (see [`run`], which is otherwise identical either way).
enum Mode {
    /// No fork happened; this process *is* the long-running daemon (or has already failed to
    /// become one). There's no separate waiting parent to report to — the outcome is just this
    /// process's own exit code — and no controlling terminal to detach from, since a supervisor
    /// (systemd, a container entrypoint) or an interactive user asked for this directly.
    Foreground,
    /// Forked from the original invocation, which is blocked reading `write_fd`'s other end (see
    /// `main`) waiting to mirror whatever outcome is reported here as its own exit code.
    Daemon { write_fd: RawFd },
}

impl Mode {
    /// Reports a startup failure and exits with `code`. Never returns.
    fn fail(&self, code: u8) -> ! {
        if let Mode::Daemon { write_fd } = self {
            report(*write_fd, code);
        }
        std::process::exit(code.into());
    }

    /// Reports startup success and, only when daemonized, stops mirroring output to the terminal
    /// (`log_sink.detach()`) and releases it (`detach_stdio`) — in `--foreground`, everything just
    /// keeps going to wherever stdio already points (e.g. journald under systemd, or an
    /// interactive terminal), since there's nothing to detach from.
    fn succeed(&self, log_sink: &LogSink) {
        tracing::info!("started");
        if let Mode::Daemon { write_fd } = self {
            report(*write_fd, 0);
            log_sink.detach();
            detach_stdio();
        }
    }

    /// Reports success *silently* (no "started" message) and exits `0` — the already-running
    /// case in [`run`], not worth announcing whether daemonized or not.
    fn already_running(&self) -> ! {
        if let Mode::Daemon { write_fd } = self {
            report(*write_fd, 0);
        }
        std::process::exit(0);
    }
}

/// With `--foreground`, runs directly — no fork, no readiness pipe, this process *is* the daemon
/// and its own exit code *is* the outcome.
///
/// Without it (the default), daemonizes: forks, then blocks in the *original* process until the
/// forked child has actually tried to bind the socket and start the Windows server, reporting the
/// real outcome as this process's own exit code — `0` for "started fresh" *or* "already running,
/// nothing to do", non-zero for a genuine startup failure. This is what makes it safe to source
/// from a shell profile: the invoking shell doesn't get its prompt back until startup has a known
/// outcome, and can act on `$?` instead of always seeing success.
///
/// The fork happens *before* any of that startup work runs, not after — forking a process that
/// already has a running multi-threaded tokio runtime is unsafe (only the forking thread survives
/// into the child; the runtime's other worker threads simply vanish, leaving it broken), so the
/// child must build its own fresh runtime after `fork()`, before doing anything async. The
/// original process still doesn't return control to its caller until the child reports a real
/// outcome over the pipe below — from the caller's point of view this is indistinguishable from
/// "fork after a successful bind", it just isn't implemented that way at the syscall level.
///
/// `tracing` is initialized here, before the fork — both the parent (for the "exited before
/// reporting readiness" case below) and the child inherit the same already-configured global
/// subscriber this way (fork copies process memory, statics included), instead of each needing
/// its own separate initialization. Everything before this point (config/path resolution, opening
/// the log file itself) can't go through `tracing` yet — there's nothing for it to write to — so
/// those few early failures use `eprintln!` instead; everything after uses `tracing::error!`
/// consistently, both here and in `run`.
fn main() -> std::process::ExitCode {
    let config = Config::parse();

    // Resolved before forking, same as clap's own argument errors: a missing `$HOME` with no
    // explicit `--socket` is a static configuration problem, not a runtime bind failure, so it's
    // handled the same way — fail fast in the foreground, before any daemonizing happens.
    let socket_path = match config.socket_path() {
        Ok(path) => path,
        Err(e) => {
            eprintln!("sockrelay-client: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let log_filter = match config::effective_log_filter(std::env::var("RUST_LOG").ok()) {
        Ok(filter) => filter,
        Err(e) => {
            eprintln!("sockrelay-client: invalid RUST_LOG: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let log_sink = match LogSink::open(config.log_file.as_deref()) {
        Ok(sink) => sink,
        Err(e) => {
            eprintln!(
                "sockrelay-client: failed to open --log-file {:?}: {e}",
                config.log_file
            );
            return std::process::ExitCode::FAILURE;
        }
    };
    tracing_subscriber::fmt()
        .with_writer(log_sink.clone())
        .with_ansi(log_sink.wants_color(std::io::stderr().is_terminal()))
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_new(&log_filter)
                .expect("effective_log_filter validated it"),
        )
        .init();

    if config.foreground {
        run(config, socket_path, log_filter, log_sink, Mode::Foreground);
    }

    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        tracing::error!(
            error = %std::io::Error::last_os_error(),
            "failed to create readiness pipe"
        );
        return std::process::ExitCode::FAILURE;
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);

    match unsafe { libc::fork() } {
        -1 => {
            tracing::error!(error = %std::io::Error::last_os_error(), "fork failed");
            std::process::ExitCode::FAILURE
        }
        0 => {
            // Child: this becomes the daemon. Close the read end (only the original process
            // reads from it) and detach from the controlling terminal's session before doing
            // anything else, so a later SIGHUP to that session (e.g. the invoking shell exiting)
            // doesn't reach this process.
            unsafe { libc::close(read_fd) };
            if unsafe { libc::setsid() } == -1 {
                tracing::error!(error = %std::io::Error::last_os_error(), "setsid failed");
                report(write_fd, 1);
                std::process::exit(1);
            }
            run(
                config,
                socket_path,
                log_filter,
                log_sink,
                Mode::Daemon { write_fd },
            );
        }
        _child_pid => {
            // Original process: close the write end (only the child writes to it) and block
            // until the child reports a real outcome, then mirror that as this process's own
            // exit code.
            unsafe { libc::close(write_fd) };
            let mut status = [0u8; 1];
            let n = unsafe { libc::read(read_fd, status.as_mut_ptr() as *mut _, 1) };
            unsafe { libc::close(read_fd) };
            if n == 1 {
                std::process::ExitCode::from(status[0])
            } else {
                tracing::error!("daemon process exited before reporting readiness");
                std::process::ExitCode::FAILURE
            }
        }
    }
}

/// Reports `code` to the original process blocked in `main` and closes the pipe. Best-effort: if
/// that process has somehow already gone away, there's nothing more useful to do than proceed.
fn report(write_fd: RawFd, code: u8) {
    let buf = [code];
    unsafe {
        libc::write(write_fd, buf.as_ptr() as *const _, 1);
        libc::close(write_fd);
    }
}

/// Redirects stdin/stdout/stderr to `/dev/null`. Only called in `Mode::Daemon`, and only *after*
/// startup has succeeded and readiness has been reported, so any startup-time diagnostics are
/// still visible to whoever invoked this process, but the long-running daemon doesn't hold the
/// original terminal open afterward. Always targets `/dev/null` regardless of `--log-file`: this
/// exists to release the controlling terminal (and catch anything that bypasses `LogSink`
/// entirely, like a raw panic message), not to carry `tracing`'s own output — that's `LogSink`'s
/// job, via its own independently-held file handle, unaffected by what fds 0/1/2 happen to point
/// at.
fn detach_stdio() {
    unsafe {
        let devnull_r = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY);
        let devnull_w = libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY);
        libc::dup2(devnull_r, 0);
        libc::dup2(devnull_w, 1);
        libc::dup2(devnull_w, 2);
        if devnull_r > 2 {
            libc::close(devnull_r);
        }
        if devnull_w > 2 {
            libc::close(devnull_w);
        }
    }
}

/// Runs the actual daemon inside a freshly built tokio runtime. In `Mode::Daemon` this runs in the
/// forked child, for the reason explained on `main`; in `Mode::Foreground` it runs directly in the
/// original process. Never returns: it either becomes the long-running daemon (until the accept
/// loop's task tree eventually calls `std::process::exit` on its own — see `cold_start`'s "died"
/// watcher below) or reports a startup failure and exits immediately.
fn run(
    config: Config,
    socket_path: PathBuf,
    log_filter: String,
    log_sink: LogSink,
    mode: Mode,
) -> ! {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build the tokio runtime");

    runtime.block_on(async move {
        // Claim the socket path first — this is also what settles "is another instance already
        // running" (see `unix_listener::bind_reclaiming_stale`) — and only then start the Windows
        // server. An already-running instance isn't a failure from this process's point of view:
        // there's nothing more for it to do, so it reports success and exits.
        let listener = match unix_listener::bind(&socket_path, config.socket_mode).await {
            Ok(listener) => listener,
            Err(ListenError::AlreadyInUse { .. }) => mode.already_running(),
            Err(e) => {
                tracing::error!(error = %e, "failed to bind the unix socket");
                mode.fail(1);
            }
        };

        // Spawned exactly once, here, before accepting any connections. Deliberately simple: if
        // this fails, or if the connection dies later (see the spawned watcher below), the
        // daemon just exits rather than trying to retry or respawn.
        let (handle, died, server_stderr) =
            match cold_start(config.server_exe.clone(), config.server_args(&log_filter)).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!(error = %e, "failed to start the windows server");
                    mode.fail(1);
                }
            };

        // Forwards the spawned server's own stderr into our own log sink, each line marked as the
        // server's (see `log_sink::forward_server_lines`). This has to be wired up before the grace-period check below,
        // not after: the server's own "pipe does not exist" diagnostic (see its `pipe::exists`
        // check) is exactly the kind of message that arrives *during* that window, and unlike
        // before `.stderr(Stdio::piped())` was introduced (when this was simply inherited from
        // this process), nothing reads a piped stderr unless something explicitly does.
        tokio::spawn(log_sink::forward_server_lines(
            server_stderr,
            log_sink.clone(),
        ));

        // Give the server a short grace period to prove it's actually still alive — cheap
        // insurance against reporting success for a process that's already exited (e.g. a
        // misconfigured `--pipe-path`; `sockrelay-server` checks for and fails fast on that,
        // see its own `main.rs`) before this daemon's very first real connection ever surfaces it.
        // `died.clone()` doesn't consume the original: if the timeout elapses first (the ordinary,
        // healthy case), `died` is still fully valid for the long-running watcher spawned below.
        if tokio::time::timeout(STARTUP_GRACE_PERIOD, died.clone())
            .await
            .is_ok()
        {
            tracing::error!(
                "the spawned Windows server exited immediately after starting — check \
                 --pipe-path and that the real agent is running"
            );
            mode.fail(1);
        }

        // Startup succeeded: tell whoever invoked this (visible exactly once, for a genuinely
        // fresh start — the already-running case above never reaches here) and, if daemonized,
        // report readiness and detach — see `Mode::succeed`.
        mode.succeed(&log_sink);

        tokio::spawn(async move {
            died.await;
            tracing::error!("mux connection to the server died, exiting");
            std::process::exit(1);
        });

        let accept_error_backoff = Duration::from_millis(config.accept_error_backoff_ms);
        unix_listener::accept_loop(
            &socket_path,
            listener,
            accept_error_backoff,
            move |unix_stream| handle_connection(unix_stream, handle.clone()),
        )
        .await;
    });

    std::process::exit(0);
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
/// `log_sink::forward_server_lines`) — taken here, before `server` moves into the "died" future's closure,
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
