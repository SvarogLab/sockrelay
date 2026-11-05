//! Runs the built client the way `sockrelay.socket` does, against a stand-in server: a shell script
//! that marks when it has started and then behaves as each test needs.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;

use std::io::{BufRead, BufReader, ErrorKind};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
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
        let mut launcher = Command::new("systemd-socket-activate");
        launcher.stderr(Stdio::null());
        Self::start_as(server_body, launcher)
    }

    /// Starts the client allowed only `open_files` file descriptors, and collects what it logs.
    fn start_short_of_fds(server_body: &str, open_files: u32) -> (Self, Arc<Mutex<String>>) {
        let mut launcher = Command::new("prlimit");
        launcher
            .arg(format!("--nofile={open_files}"))
            .arg("systemd-socket-activate")
            // The test waits for a warning, whatever filter its own environment sets.
            .env("RUST_LOG", "warn")
            .stderr(Stdio::piped());
        let mut relay = Self::start_as(server_body, launcher);
        let log = Arc::new(Mutex::new(String::new()));
        let stderr = BufReader::new(relay.client.stderr.take().unwrap());
        let collected = log.clone();
        std::thread::spawn(move || {
            for line in stderr.lines().map_while(Result::ok) {
                collected.lock().unwrap().push_str(&(line + "\n"));
            }
        });
        (relay, log)
    }

    /// `launcher` ends in `systemd-socket-activate`, which execs the client in its place.
    fn start_as(server_body: &str, mut launcher: Command) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let server = write_server(dir.path(), server_body);
        let socket = dir.path().join("agent.sock");
        let client = launcher
            .arg("-l")
            .arg(&socket)
            .arg(env!("CARGO_BIN_EXE_sockrelay-client"))
            .arg("--server-exe")
            .arg(&server)
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        // Owned before activating, so a failed activation still kills it on drop.
        let relay = Self { dir, client };
        activate(&socket);
        relay
    }

    fn socket(&self) -> PathBuf {
        self.dir.path().join("agent.sock")
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

/// A stand-in server: a script that marks when it has started, then runs `body`.
fn write_server(dir: &Path, body: &str) -> PathBuf {
    let server = dir.join("server");
    let marker = dir.join("server-started");
    std::fs::write(
        &server,
        format!("#!/bin/sh\ntouch '{}'\n{body}\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&server, std::fs::Permissions::from_mode(0o755)).unwrap();
    server
}

/// systemd-socket-activate execs the client only once the first connection arrives. The socket's
/// file appears when it is bound, a moment before it listens, and refuses connections until then.
fn activate(socket: &Path) {
    wait_for(
        || match UnixStream::connect(socket) {
            Ok(_) => true,
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => {
                false
            }
            Err(e) => panic!("can't connect to the socket: {e}"),
        },
        "the socket to listen",
    );
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

/// A burst of connections can leave `accept()` without a file descriptor to give the next one.
/// That passes once connections close, so the relay retries instead of exiting.
#[test]
fn running_out_of_file_descriptors_doesnt_end_the_relay() {
    const OPEN_FILES: u32 = 64;
    let (mut relay, log) = Relay::start_short_of_fds("cat >/dev/null", OPEN_FILES);
    relay.wait_for_server();
    std::thread::sleep(STARTED);

    let _burst: Vec<_> = (0..2 * OPEN_FILES)
        .map(|_| UnixStream::connect(relay.socket()).unwrap())
        .collect();
    wait_for(
        || log.lock().unwrap().contains("accept() failed, retrying"),
        "accept() to run out of file descriptors",
    );
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        relay.client.try_wait().unwrap().is_none(),
        "the relay exited: {}",
        log.lock().unwrap()
    );
}
