use std::env;
use std::os::fd::RawFd;
use std::path::PathBuf;

use clap::Parser;

const DEFAULT_SOCKET_PATH: &str = ".ssh/agent.sock";
const DEFAULT_SOCKET_MODE: u32 = 0o600;
const DEFAULT_PIPE_PATH: &str = r"\\.\pipe\openssh-ssh-agent";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error(
        "could not determine the home directory to build a default socket path; pass --socket explicitly"
    )]
    NoHomeDir,
    #[error("systemd passed {0} sockets; the .socket unit must declare exactly one ListenStream=")]
    TooManyInheritedSockets(usize),
    #[error(
        "--socket and --socket-mode have no effect when systemd passes the socket; set ListenStream= and SocketMode= in the .socket unit instead"
    )]
    SocketFlagsWithInheritedSocket,
}

/// Where the listening socket comes from.
#[derive(Debug, PartialEq, Eq)]
pub enum ListenOn {
    /// This process binds `path` itself (see `unix_listener::bind`).
    Path { path: PathBuf, mode: u32 },
    /// systemd has already bound the socket and passed it in (socket activation). Its path,
    /// permissions and lifetime belong to the `.socket` unit, and this process can't fork: systemd
    /// addresses the socket to its own pid through `LISTEN_PID`.
    Inherited(RawFd),
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
    /// Unix socket path to listen on. Defaults to `~/.ssh/agent.sock`. Rejected under socket
    /// activation, where the `.socket` unit's `ListenStream=` sets it.
    #[arg(long)]
    pub socket: Option<PathBuf>,

    /// Path to the `sockrelay-server.exe` to spawn on the Windows side. Defaults to searching
    /// `PATH` for `sockrelay-server.exe` (e.g. after `install.sh` puts it in `/usr/local/bin`).
    #[arg(long, default_value = "sockrelay-server.exe")]
    pub server_exe: PathBuf,

    /// Permission bits applied to the unix socket right as it's created (see
    /// `unix_listener::bind_with_restrictive_umask`), e.g. `600` (owner-only). Widening this is a
    /// real security tradeoff (see `unix_listener.rs`'s notes on why a restrictive mode matters at
    /// all for a `--socket` override outside a locked-down directory). Defaults to `600`. Rejected
    /// under socket activation, where the `.socket` unit's `SocketMode=` sets it.
    #[arg(long, value_parser = parse_octal_mode)]
    pub socket_mode: Option<u32>,

    /// Backoff before retrying `accept()` after a transient error, in milliseconds — see
    /// `unix_listener::accept_loop`.
    #[arg(long, default_value_t = 100)]
    pub accept_error_backoff_ms: u64,

    /// Runs in the foreground instead of daemonizing (no fork, no detaching from the controlling
    /// terminal) — for a process supervisor (systemd, a container entrypoint) that already
    /// expects to own this process directly, or for interactive debugging. Implied under socket
    /// activation.
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
    /// Settles where the listening socket comes from, given the sockets systemd passed in (none
    /// unless socket-activated): an inherited socket wins, and a flag that would only apply to a
    /// socket this process binds is an error rather than silently ignored.
    pub fn listen_on(&self, inherited_sockets: &[RawFd]) -> Result<ListenOn, ConfigError> {
        match inherited_sockets {
            [] => Ok(ListenOn::Path {
                path: self.socket_path()?,
                mode: self.socket_mode.unwrap_or(DEFAULT_SOCKET_MODE),
            }),
            [fd] if self.socket.is_none() && self.socket_mode.is_none() => {
                Ok(ListenOn::Inherited(*fd))
            }
            [_] => Err(ConfigError::SocketFlagsWithInheritedSocket),
            fds => Err(ConfigError::TooManyInheritedSockets(fds.len())),
        }
    }

    /// Resolves the configured (or default, home-dir-relative) socket path.
    fn socket_path(&self) -> Result<PathBuf, ConfigError> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Config {
        Config::try_parse_from(std::iter::once("sockrelay-client").chain(args.iter().copied()))
            .unwrap()
    }

    #[test]
    fn binds_the_configured_path_when_systemd_passes_no_socket() {
        let config = parse(&["--socket", "/run/test/agent.sock"]);
        assert_eq!(
            config.listen_on(&[]).unwrap(),
            ListenOn::Path {
                path: PathBuf::from("/run/test/agent.sock"),
                mode: 0o600,
            }
        );
    }

    #[test]
    fn binds_with_the_configured_socket_mode() {
        let config = parse(&["--socket", "/run/test/agent.sock", "--socket-mode", "660"]);
        assert!(matches!(
            config.listen_on(&[]).unwrap(),
            ListenOn::Path { mode: 0o660, .. }
        ));
    }

    #[test]
    fn uses_the_socket_systemd_passes() {
        assert_eq!(parse(&[]).listen_on(&[3]).unwrap(), ListenOn::Inherited(3));
    }

    #[test]
    fn refuses_more_than_one_inherited_socket() {
        assert!(matches!(
            parse(&[]).listen_on(&[3, 4]),
            Err(ConfigError::TooManyInheritedSockets(2))
        ));
    }

    #[test]
    fn refuses_socket_with_an_inherited_socket() {
        let config = parse(&["--socket", "/run/test/agent.sock"]);
        assert!(matches!(
            config.listen_on(&[3]),
            Err(ConfigError::SocketFlagsWithInheritedSocket)
        ));
    }

    #[test]
    fn refuses_socket_mode_with_an_inherited_socket() {
        let config = parse(&["--socket-mode", "600"]);
        assert!(matches!(
            config.listen_on(&[3]),
            Err(ConfigError::SocketFlagsWithInheritedSocket)
        ));
    }
}
