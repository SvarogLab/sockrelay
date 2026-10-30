//! The one listening socket systemd passes in: its path, permissions and lifetime all belong to
//! `sockrelay.socket`.

use std::os::fd::RawFd;

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

pub const fn listening_socket(inherited: &[RawFd]) -> Result<RawFd, SocketError> {
    match inherited {
        [fd] => Ok(*fd),
        [] => Err(SocketError::NotPassed),
        fds => Err(SocketError::TooMany(fds.len())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
