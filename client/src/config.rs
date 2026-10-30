use std::os::fd::RawFd;
use std::path::PathBuf;

use clap::Parser;
use librelay::client::log::LogFilter;

const DEFAULT_PIPE_PATH: &str = r"\\.\pipe\openssh-ssh-agent";

#[derive(Debug, thiserror::Error)]
pub enum SocketError {
    #[error(
        "systemd passed no socket; start through sockrelay.socket, or for a manual run under \
         systemd-socket-activate"
    )]
    NotPassed,
    #[error("systemd passed {0} sockets; the .socket unit must declare exactly one ListenStream=")]
    TooMany(usize),
}

/// The one listening socket systemd passes in: its path, permissions and lifetime all belong to
/// `sockrelay.socket`.
pub fn listening_socket(inherited: &[RawFd]) -> Result<RawFd, SocketError> {
    match inherited {
        [fd] => Ok(*fd),
        [] => Err(SocketError::NotPassed),
        fds => Err(SocketError::TooMany(fds.len())),
    }
}

/// Every value has a default, so the systemd unit runs this process with no flags. The
/// pipe-path, timeout and jitter flags only pass through to the server as arguments (see
/// `server_args`): environment variables don't cross the WSL interop boundary into a Windows
/// process.
#[derive(Parser, Debug)]
#[command(
    about = "sockrelay Linux/WSL-side relay: exposes a unix socket backed by the real Windows ssh-agent"
)]
pub struct Config {
    /// Path to the `sockrelay-server.exe` to spawn on the Windows side. Defaults to searching
    /// `PATH` for `sockrelay-server.exe` (e.g. after `install.sh` puts it in `/usr/local/bin`).
    #[arg(long, default_value = "sockrelay-server.exe")]
    pub server_exe: PathBuf,

    /// Backoff before retrying `accept()` after a transient error, in milliseconds — see
    /// `accept_connections` in `main.rs`.
    #[arg(long, default_value_t = 100)]
    pub accept_error_backoff_ms: u64,

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
    pub fn server_args(&self, log_filter: &LogFilter) -> Vec<String> {
        let mut args = Vec::from(log_filter.server_args());
        args.extend([
            "--pipe-path".to_string(),
            self.pipe_path.clone(),
            "--connect-timeout-secs".to_string(),
            self.connect_timeout_secs.to_string(),
            "--jitter-max-ms".to_string(),
            self.jitter_max_ms.to_string(),
        ]);
        args
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Config {
        Config::try_parse_from(std::iter::once("sockrelay-client").chain(args.iter().copied()))
            .unwrap()
    }

    #[test]
    fn takes_the_socket_systemd_passes() {
        assert_eq!(listening_socket(&[3]).unwrap(), 3);
    }

    #[test]
    fn refuses_to_run_without_a_socket_from_systemd() {
        assert!(matches!(listening_socket(&[]), Err(SocketError::NotPassed)));
    }

    #[test]
    fn refuses_more_than_one_socket() {
        assert!(matches!(
            listening_socket(&[3, 4]),
            Err(SocketError::TooMany(2))
        ));
    }

    #[test]
    fn leaves_the_socket_path_and_mode_to_the_socket_unit() {
        for flag in [["--socket", "/tmp/agent.sock"], ["--socket-mode", "600"]] {
            let args = std::iter::once("sockrelay-client").chain(flag);
            assert!(Config::try_parse_from(args).is_err(), "{flag:?}");
        }
    }

    #[test]
    fn has_no_daemon_mode_to_opt_out_of() {
        assert!(Config::try_parse_from(["sockrelay-client", "--foreground"]).is_err());
    }

    #[test]
    fn has_no_log_file() {
        let args = ["sockrelay-client", "--log-file", "/tmp/client.log"];
        assert!(Config::try_parse_from(args).is_err());
    }

    #[test]
    fn passes_the_log_filter_to_the_server() {
        let filter = LogFilter::parse(Some("debug".to_string())).unwrap();
        let args = parse(&[]).server_args(&filter);
        let at = args.iter().position(|arg| arg == "--log-filter").unwrap();
        assert_eq!(args[at + 1], "debug");
    }
}
