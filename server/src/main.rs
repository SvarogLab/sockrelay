mod config;
mod mux;
mod pipe;
mod watchdog;

use clap::Parser;

use config::Config;

#[tokio::main]
async fn main() {
    // Must write to stderr, not the default stdout: this process's own stdout *is* the yamux
    // transport (joined with stdin below) — any tracing output on stdout would land in the
    // middle of the binary frame stream and corrupt it.
    tracing_subscriber::fmt().with_writer(std::io::stderr).init();

    let config = Config::parse();

    // Fails fast and loudly if the configured pipe doesn't exist at all (wrong `--pipe-path`, or
    // the real agent isn't running), instead of only discovering that on the first real relayed
    // connection — the client watches for this process dying shortly after spawn and treats it as
    // a startup failure (see `client/src/main.rs`'s startup grace period).
    #[cfg(windows)]
    if !pipe::exists(&config.pipe_path) {
        tracing::error!(
            pipe_path = %config.pipe_path,
            "the configured ssh-agent named pipe does not exist — is the real agent running?"
        );
        std::process::exit(1);
    }

    let stdio = tokio::io::join(tokio::io::stdin(), tokio::io::stdout());
    let (mut acceptor, driver) = mux::server(stdio);
    let driver_task = tokio::spawn(driver);

    tokio::spawn(async move {
        while let Some(stream) = acceptor.accept().await {
            let pipe_path = config.pipe_path.clone();
            #[cfg(windows)]
            let settings = config.pipe_settings();
            tokio::spawn(async move {
                #[cfg(windows)]
                pipe::relay_stream(&pipe_path, stream, settings).await;
                #[cfg(not(windows))]
                {
                    let _ = (pipe_path, stream);
                }
            });
        }
    });

    // The client side does not automatically kill this process when it dies, since WSL-spawned
    // Windows processes are not tied to their spawning Linux process's lifetime — this watchdog
    // is what actually notices and exits instead of lingering as an orphan. `std::process::exit`
    // also closes every open named-pipe handle as an OS-level side effect, satisfying that
    // requirement without a separate handle registry.
    let result = watchdog::watch(driver_task, || {
        tracing::info!("mux connection to the client ended, shutting down");
    })
    .await;

    // Exit non-zero on a real error instead of always reporting success, so anything watching
    // this process's exit code (a supervisor, a manual smoke test) can tell a clean shutdown
    // (the client went away normally) apart from a genuine connection/protocol failure.
    match result {
        Ok(Ok(())) => std::process::exit(0),
        Ok(Err(e)) => {
            tracing::error!(error = %e, "mux driver ended with an error");
            std::process::exit(1);
        }
        Err(e) => {
            tracing::error!(error = %e, "mux driver task panicked");
            std::process::exit(1);
        }
    }
}
