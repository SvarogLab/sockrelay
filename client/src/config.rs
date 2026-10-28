use std::env;
use std::path::PathBuf;

use clap::Parser;

const DEFAULT_SOCKET_PATH: &str = ".ssh/agent.sock";
const DEFAULT_PIPE_PATH: &str = r"\\.\pipe\openssh-ssh-agent";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error(
        "could not determine the home directory to build a default socket path; pass --socket explicitly"
    )]
    NoHomeDir,
}

/// Unlike `sockrelay-server`, every value here has a sensible default — this process is the one a
/// human actually invokes directly (typically from a shell profile), so it should work with no
/// flags at all in the common case. The pipe-path/timeout/jitter flags below have no effect on
/// this process itself — they're read here only to be forwarded as argv to the spawned
/// `sockrelay-server.exe` (see `server_args`), since env vars set on this process do **not** cross
/// the WSL binfmt-interop boundary into the spawned Windows process (confirmed empirically); their
/// defaults match the values `sockrelay-server` itself used before its own flags were made
/// mandatory.
#[derive(Parser, Debug)]
#[command(
    about = "sockrelay Linux/WSL-side relay: exposes a unix socket backed by the real Windows ssh-agent"
)]
pub struct Config {
    /// Unix socket path to listen on. Defaults to `~/.ssh/agent.sock`.
    #[arg(long)]
    pub socket: Option<PathBuf>,

    /// Path to the `sockrelay-server.exe` to spawn on the Windows side. Defaults to searching
    /// `PATH` for `sockrelay-server.exe` (e.g. after `install.sh` puts it in `/usr/local/bin`).
    #[arg(long, default_value = "sockrelay-server.exe")]
    pub server_exe: PathBuf,

    /// Permission bits applied to the unix socket right as it's created (see
    /// `unix_listener::bind_with_restrictive_umask`), e.g. `600` (owner-only). Widening this is a
    /// real security tradeoff (see `unix_listener.rs`'s notes on why a restrictive mode matters at
    /// all for a `--socket` override outside a locked-down directory).
    #[arg(long, default_value = "600", value_parser = parse_octal_mode)]
    pub socket_mode: u32,

    /// Backoff before retrying `accept()` after a transient error, in milliseconds — see
    /// `unix_listener::accept_loop`.
    #[arg(long, default_value_t = 100)]
    pub accept_error_backoff_ms: u64,

    /// Runs in the foreground instead of daemonizing (no fork, no detaching from the controlling
    /// terminal) — for a process supervisor (systemd, a container entrypoint) that already
    /// expects to own this process directly, or for interactive debugging.
    #[arg(long, default_value_t = false)]
    pub foreground: bool,

    /// Where to write timestamped diagnostics (this process's own, and the spawned Windows
    /// server's, forwarded from its stderr — see `log_sink::LogSink`). Unset by default: no file
    /// is opened and nothing is written to one — this and "does the invoking terminal see it" are
    /// two fully independent questions, see `LogSink`'s own doc comment.
    #[arg(long)]
    pub log_file: Option<PathBuf>,

    /// Named pipe path for the local ssh-agent — forwarded to the spawned Windows server as
    /// `--pipe-path` (see `server_args`); has no effect on this process itself.
    #[arg(long, default_value = DEFAULT_PIPE_PATH)]
    pub pipe_path: String,

    /// Forwarded to the spawned Windows server as `--connect-timeout-secs`; has no effect on this
    /// process itself. See `server::pipe::connect`.
    #[arg(long, default_value_t = 5)]
    pub connect_timeout_secs: u64,

    /// Forwarded to the spawned Windows server as `--jitter-max-ms`; has no effect on this process
    /// itself. See `server::pipe::connect`.
    #[arg(long, default_value_t = 50)]
    pub jitter_max_ms: u64,
}

impl Config {
    /// Resolves the configured (or default, home-dir-relative) socket path.
    pub fn socket_path(&self) -> Result<PathBuf, ConfigError> {
        match &self.socket {
            Some(path) => Ok(path.clone()),
            None => Ok(home_dir()
                .ok_or(ConfigError::NoHomeDir)?
                .join(DEFAULT_SOCKET_PATH)),
        }
    }

    /// The argv this process passes when spawning `sockrelay-server.exe` — the only channel that
    /// actually reaches it (see this module's doc comment on why env vars don't).
    pub fn server_args(&self) -> Vec<String> {
        vec![
            "--pipe-path".to_string(),
            self.pipe_path.clone(),
            "--connect-timeout-secs".to_string(),
            self.connect_timeout_secs.to_string(),
            "--jitter-max-ms".to_string(),
            self.jitter_max_ms.to_string(),
        ]
    }
}

fn parse_octal_mode(value: &str) -> Result<u32, String> {
    u32::from_str_radix(value, 8)
        .ok()
        .filter(|mode| *mode <= 0o777)
        .ok_or_else(|| format!("{value:?} is not a valid octal permission mode (e.g. \"600\")"))
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}
