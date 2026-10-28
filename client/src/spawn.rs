use std::io;
use std::path::PathBuf;

use tokio::io::Join;
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error("failed to spawn server process {path:?}: {source}")]
    Spawn {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("spawned server process did not expose piped stdin/stdout/stderr")]
    MissingStdio,
}

/// A spawned server child process. Only ever spawned once, from `main.rs::cold_start`, before the
/// daemon starts accepting connections — the fix for the WSL2 interop concurrency bug this project
/// exists for (parallel `ssh`/`ssh-add` invocations used to each spawn their own Windows-side
/// process concurrently; this daemon spawns exactly one and multiplexes every client over it).
pub struct SpawnedServer {
    child: Child,
}

impl SpawnedServer {
    /// Spawns `path` with piped stdin/stdout (its future joined duplex is the transport `mux.rs`
    /// multiplexes over). The child is killed if this value is dropped without an explicit
    /// hand-off, so a failed cold start doesn't leave an orphan process behind.
    ///
    /// Runs the actual `Command::spawn()` call inside `spawn_blocking`. This isn't just style:
    /// `Command::spawn()` is a synchronous OS call, and a documented WSL2 interop hang
    /// (microsoft/WSL #7371, #6891, #53250) can occur in exactly that call — one that
    /// spawning-on-a-core-async-worker-thread would block the whole worker on, not just this task.
    /// Off a blocking-pool thread, a hang there only stalls this one task instead of stalling
    /// every other task scheduled on that worker.
    pub async fn spawn(
        path: impl Into<PathBuf>,
        args: impl IntoIterator<Item = String> + Send + 'static,
    ) -> Result<Self, SpawnError> {
        let path = path.into();
        tokio::task::spawn_blocking(move || {
            let child = Command::new(&path)
                .args(args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .map_err(|source| SpawnError::Spawn {
                    path: path.clone(),
                    source,
                })?;
            Ok(Self { child })
        })
        .await
        .expect("spawn_blocking task panicked")
    }

    /// Takes ownership of the child's stdout/stdin, joined into a single duplex transport. Can
    /// only be called once per spawned child.
    pub fn take_io(&mut self) -> Result<Join<ChildStdout, ChildStdin>, SpawnError> {
        let stdout = self.child.stdout.take().ok_or(SpawnError::MissingStdio)?;
        let stdin = self.child.stdin.take().ok_or(SpawnError::MissingStdio)?;
        Ok(tokio::io::join(stdout, stdin))
    }

    /// Takes ownership of the child's stderr — the server writes its own timestamped diagnostics
    /// there exactly as if run directly (see `server/src/main.rs`); it has no concept of a log
    /// file or of this process's `--log-file` at all. The caller (see `main.rs`'s server-log
    /// forwarding task) is responsible for reading it and deciding where those bytes end up. Can
    /// only be called once per spawned child.
    pub fn take_stderr(&mut self) -> Result<ChildStderr, SpawnError> {
        self.child.stderr.take().ok_or(SpawnError::MissingStdio)
    }

    /// Resolves once the child process exits — the client-side half of mutual liveness detection
    /// between this process and the mux driver (see `main.rs::cold_start`'s "died" future, which
    /// races this against the driver task). The daemon exits as soon as either side is no longer
    /// usable, rather than lingering as a half-alive process.
    pub async fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        self.child.wait().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// `cat` stands in for `sockrelay-server.exe` here: real process spawn and real piped
    /// stdio, exercised on this Linux host, just not the Windows binary itself (see this
    /// project's README for how to exercise the real cross-compiled binary end to end).
    ///
    /// `Join`'s `poll_shutdown` forwards to `ChildStdin`'s, which for a plain pipe is a no-op —
    /// unlike a socket half-close, a pipe fd only actually signals EOF to the reading end when
    /// it's closed. So triggering `cat`'s EOF here means splitting the joined duplex back apart
    /// with `into_inner()` and dropping the writer half, not calling `.shutdown()` on it.
    #[tokio::test]
    async fn take_io_relays_bytes_through_the_child() {
        let _children = crate::test_support::CHILD_PROCESSES.read().await;
        let mut server = SpawnedServer::spawn("cat", Vec::new()).await.unwrap();
        let io = server.take_io().unwrap();

        let (mut stdout, mut stdin) = io.into_inner();
        stdin.write_all(b"hello, child").await.unwrap();
        drop(stdin);

        let mut echoed = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            stdout.read_to_end(&mut echoed),
        )
        .await
        .expect("read_to_end did not observe EOF from the child")
        .unwrap();
        assert_eq!(echoed, b"hello, child");
    }

    #[tokio::test]
    async fn take_io_twice_fails() {
        let _children = crate::test_support::CHILD_PROCESSES.read().await;
        let mut server = SpawnedServer::spawn("cat", Vec::new()).await.unwrap();
        server.take_io().unwrap();
        assert!(matches!(server.take_io(), Err(SpawnError::MissingStdio)));
    }

    #[tokio::test]
    async fn take_stderr_reads_the_childs_stderr_independently_of_stdio() {
        let _children = crate::test_support::CHILD_PROCESSES.read().await;
        let mut server = SpawnedServer::spawn(
            "sh",
            vec!["-c".to_string(), "echo hello, stderr >&2".to_string()],
        )
        .await
        .unwrap();
        let _io = server.take_io().unwrap();
        let mut stderr = server.take_stderr().unwrap();

        let mut buf = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            stderr.read_to_end(&mut buf),
        )
        .await
        .expect("read_to_end did not observe EOF from the child's stderr")
        .unwrap();
        assert_eq!(buf, b"hello, stderr\n");
    }

    #[tokio::test]
    async fn take_stderr_twice_fails() {
        let _children = crate::test_support::CHILD_PROCESSES.read().await;
        let mut server = SpawnedServer::spawn("cat", Vec::new()).await.unwrap();
        server.take_stderr().unwrap();
        assert!(matches!(
            server.take_stderr(),
            Err(SpawnError::MissingStdio)
        ));
    }

    #[tokio::test]
    async fn wait_resolves_once_the_child_exits() {
        let _children = crate::test_support::CHILD_PROCESSES.read().await;
        let mut server = SpawnedServer::spawn("true", Vec::new()).await.unwrap();
        let status = tokio::time::timeout(std::time::Duration::from_secs(5), server.wait())
            .await
            .expect("wait() did not resolve after the child exited")
            .unwrap();
        assert!(status.success());
    }

    #[tokio::test]
    async fn spawn_of_a_nonexistent_binary_fails() {
        let _children = crate::test_support::CHILD_PROCESSES.read().await;
        match SpawnedServer::spawn("/no/such/sockrelay-server.exe", Vec::new()).await {
            Err(SpawnError::Spawn { .. }) => {}
            other => panic!("expected SpawnError::Spawn, got {}", other.is_ok()),
        }
    }
}
