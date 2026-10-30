//! Runs the built client the way `sockrelay.socket` does, against a stand-in server: a shell script
//! that marks when it has started and then behaves as each test needs.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;

use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// Past the client's startup grace period, so the relay is up.
const STARTED: Duration = Duration::from_millis(800);

struct Relay {
    dir: TempDir,
    client: Child,
}

impl Relay {
    fn start(server_body: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let server = dir.path().join("server");
        let marker = dir.path().join("server-started");
        std::fs::write(
            &server,
            format!("#!/bin/sh\ntouch '{}'\n{server_body}\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&server, std::fs::Permissions::from_mode(0o755)).unwrap();
        let client = Command::new("systemd-socket-activate")
            .arg("-l")
            .arg(dir.path().join("agent.sock"))
            .arg(env!("CARGO_BIN_EXE_sockrelay-client"))
            .arg("--server-exe")
            .arg(&server)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // systemd-socket-activate execs the client only once the first connection arrives.
        let socket = dir.path().join("agent.sock");
        wait_for(|| socket.exists(), "the socket");
        drop(std::os::unix::net::UnixStream::connect(&socket).unwrap());
        Self { dir, client }
    }

    fn wait_for_server(&self) {
        wait_for(
            || self.dir.path().join("server-started").exists(),
            "the server to start",
        );
    }

    fn stop(&mut self, within: Duration) -> ExitStatus {
        let sent = Command::new("kill")
            .args(["-TERM", &self.client.id().to_string()])
            .status()
            .unwrap();
        assert!(sent.success());
        let deadline = Instant::now() + within;
        loop {
            if let Some(status) = self.client.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "the client didn't stop within {within:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        let _ = self.client.kill();
        let _ = self.client.wait();
    }
}

fn wait_for(ready: impl Fn() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn exit_code(status: ExitStatus) -> Option<i32> {
    assert_eq!(
        status.signal(),
        None,
        "the client was killed by a signal: {status}"
    );
    status.code()
}

// Not `exec`: the shell keeps the server's stdout open, as a real server does, while `cat` waits for
// stdin to close.
#[test]
fn a_stop_closes_the_link_and_the_server_exits_on_its_own() {
    let mut relay = Relay::start("cat >/dev/null");
    relay.wait_for_server();
    std::thread::sleep(STARTED);
    assert_eq!(exit_code(relay.stop(Duration::from_secs(3))), Some(0));
}

#[test]
fn a_stop_while_starting_ends_cleanly() {
    let mut relay = Relay::start("exec sleep 30");
    relay.wait_for_server();
    assert_eq!(exit_code(relay.stop(Duration::from_secs(3))), Some(0));
}

#[test]
fn a_server_that_outlives_the_stop_fails_it() {
    let mut relay = Relay::start("exec sleep 30");
    relay.wait_for_server();
    std::thread::sleep(STARTED);
    assert_eq!(exit_code(relay.stop(Duration::from_secs(10))), Some(1));
}
