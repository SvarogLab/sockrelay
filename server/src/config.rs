use clap::Parser;

/// Every value here is a required CLI flag — deliberately **no defaults, no env var fallback**:
/// this process refuses to start rather than silently guessing at a pipe path or timeout,
/// matching this project's "fail fast and honest" design (see `pipe::connect`'s own doc comment
/// on why the relay never hides a real problem behind an assumption). Not meant to be launched by
/// a human directly — see the note below.
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
}

#[cfg(windows)]
impl Config {
    pub fn pipe_settings(&self) -> crate::pipe::PipeSettings {
        crate::pipe::PipeSettings {
            connect_timeout: std::time::Duration::from_secs(self.connect_timeout_secs),
            jitter_max_ms: self.jitter_max_ms,
        }
    }
}
