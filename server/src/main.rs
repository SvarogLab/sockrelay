#[cfg(windows)]
mod config;
mod pipe;

#[cfg(windows)]
#[tokio::main]
async fn main() {
    use clap::Parser;
    use librelay::server::log;

    let config = config::Config::parse();
    // stderr only: stdout carries the mux, and anything else written there would corrupt it.
    log::init(&config.log);

    // Checked at startup so a wrong `--pipe-path`, or no agent running, fails the start (the
    // client's grace period catches the exit) rather than the first relayed connection.
    if !pipe::exists(&config.pipe_path) {
        tracing::error!(
            pipe_path = %config.pipe_path,
            "the configured ssh-agent named pipe does not exist — is the real agent running?"
        );
        std::process::exit(1);
    }

    let outcome = serve(config).await;
    // Exiting — rather than waiting for relays still running — also closes every named-pipe
    // handle they hold, so this process never outlives its client.
    std::process::exit(exit_code(&outcome));
}

#[cfg(not(windows))]
fn main() {
    eprintln!("sockrelay-server: runs only on Windows, started by sockrelay-client");
    std::process::exit(1);
}

/// Relays every stream the client opens to the pipe, until the client is gone.
#[cfg(windows)]
async fn serve(config: config::Config) -> Result<(), librelay::mux::MuxError> {
    let settings = config.pipe_settings();
    let pipe_path: std::sync::Arc<str> = config.pipe_path.into();
    let stdio = tokio::io::join(tokio::io::stdin(), tokio::io::stdout());
    librelay::server::mux::serve(stdio, move |stream| {
        let pipe_path = pipe_path.clone();
        async move { pipe::relay_stream(&pipe_path, stream, settings).await }
    })
    .await
}

#[cfg(windows)]
fn exit_code(outcome: &Result<(), librelay::mux::MuxError>) -> i32 {
    match outcome {
        Ok(()) => {
            tracing::info!("the client closed the connection, exiting");
            0
        }
        Err(e) => {
            tracing::error!(error = %e, "the connection to the client failed, exiting");
            1
        }
    }
}
