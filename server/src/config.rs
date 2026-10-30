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

    /// `tracing` filter for this process's diagnostics, in `RUST_LOG` syntax. A flag rather than
    /// `RUST_LOG` itself, since environment variables don't cross the WSL interop boundary.
    #[arg(long, value_parser = parse_log_filter)]
    pub log_filter: String,
}

impl Config {
    pub fn log_filter(&self) -> tracing_subscriber::EnvFilter {
        tracing_subscriber::EnvFilter::try_new(&self.log_filter)
            .expect("--log-filter is validated while parsing")
    }
}

fn parse_log_filter(value: &str) -> Result<String, String> {
    tracing_subscriber::EnvFilter::try_new(value)
        .map(|_| value.to_string())
        .map_err(|e| e.to_string())
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

#[cfg(test)]
mod tests {
    use super::*;

    const REQUIRED: [&str; 7] = [
        "sockrelay-server",
        "--pipe-path",
        r"\\.\pipe\test",
        "--connect-timeout-secs",
        "5",
        "--jitter-max-ms",
        "50",
    ];

    #[test]
    fn requires_a_log_filter() {
        assert!(Config::try_parse_from(REQUIRED).is_err());
    }

    #[test]
    fn accepts_a_valid_log_filter() {
        let args = REQUIRED
            .into_iter()
            .chain(["--log-filter", "debug,yamux=warn"]);
        assert_eq!(
            Config::try_parse_from(args).unwrap().log_filter,
            "debug,yamux=warn"
        );
    }

    #[test]
    fn refuses_an_invalid_log_filter() {
        let args = REQUIRED
            .into_iter()
            .chain(["--log-filter", "sockrelay_server=loud"]);
        assert!(Config::try_parse_from(args).is_err());
    }
}
