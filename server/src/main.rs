mod config;
mod pipe;

use clap::Parser;
use librelay::server::{log, mux};

use config::Config;

#[tokio::main]
async fn main() {
    let config = Config::parse();
    // stderr only: stdout carries the mux, and anything else written there would corrupt it.
    log::init(&config.log);

    // Checked at startup so a wrong `--pipe-path`, or no agent running, fails the start (the
    // client's grace period catches the exit) rather than the first relayed connection.
    #[cfg(windows)]
    if !pipe::exists(&config.pipe_path) {
        tracing::error!(
            pipe_path = %config.pipe_path,
            "the configured ssh-agent named pipe does not exist — is the real agent running?"
        );
        std::process::exit(1);
    }

    let stdio = tokio::io::join(tokio::io::stdin(), tokio::io::stdout());
    let outcome = mux::serve(stdio, move |stream| {
        let pipe_path = config.pipe_path.clone();
        #[cfg(windows)]
        let settings = config.pipe_settings();
        async move {
            #[cfg(windows)]
            pipe::relay_stream(&pipe_path, stream, settings).await;
            #[cfg(not(windows))]
            let _ = (pipe_path, stream);
        }
    })
    .await;

    // The client is gone. Exiting — rather than waiting for relays still running — also closes
    // every named-pipe handle they hold, so this process never outlives its client.
    match outcome {
        Ok(()) => {
            tracing::info!("the client closed the connection, exiting");
            std::process::exit(0);
        }
        Err(e) => {
            tracing::error!(error = %e, "the connection to the client failed, exiting");
            std::process::exit(1);
        }
    }
}
