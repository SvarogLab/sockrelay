use clap::Parser;

/// Every flag is required, with no default: the client always passes them, and a server started
/// any other way refuses to guess at a pipe path or timeout.
#[derive(Parser, Debug)]
#[command(
    about = "sockrelay Windows-side relay: bridges a yamux connection over stdio to the local ssh-agent named pipe.\n\nNot meant to be run directly by an end user — it is spawned by sockrelay-client, which passes every flag below explicitly and speaks the yamux protocol over this process's stdin/stdout."
)]
pub struct Config {
    /// Named pipe path for the local ssh-agent, e.g. `\\.\pipe\openssh-ssh-agent`.
    #[arg(long)]
    pub pipe_path: String,

    /// Overall budget for the busy-wait loop when the agent's pipe is saturated, in seconds — see
    /// `pipe::connect`.
    #[arg(long)]
    pub connect_timeout_secs: u64,

    /// Upper bound of the random jitter slept between busy-retry attempts, in milliseconds — see
    /// `pipe::connect`.
    #[arg(long)]
    pub jitter_max_ms: u64,

    #[command(flatten)]
    pub log: librelay::server::log::LogArgs,
}

impl Config {
    pub const fn pipe_settings(&self) -> crate::pipe::PipeSettings {
        crate::pipe::PipeSettings {
            connect_timeout: std::time::Duration::from_secs(self.connect_timeout_secs),
            jitter_max_ms: self.jitter_max_ms,
        }
    }
}
