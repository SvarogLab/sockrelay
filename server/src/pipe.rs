//! Connects each accepted stream to a fresh handle on the ssh-agent named pipe. Windows-only,
//! gated on the whole file so the workspace still checks and tests on Linux.
#![cfg(windows)]

use std::io;
use std::time::Duration;

use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY};
use windows_sys::Win32::System::Pipes::{NMPWAIT_NOWAIT, NMPWAIT_USE_DEFAULT_WAIT, WaitNamedPipeW};

/// How `connect` waits for a saturated agent; the right values depend on the agent's capacity.
#[derive(Debug, Clone, Copy)]
pub struct PipeSettings {
    /// How long `connect` keeps retrying a busy pipe before failing.
    pub connect_timeout: Duration,
    /// Upper bound of the random pause between busy retries, in milliseconds.
    pub jitter_max_ms: u64,
}

impl Default for PipeSettings {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(5),
            jitter_max_ms: 50,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PipeError {
    #[error("failed to connect to named pipe {path:?}: {source}")]
    Connect {
        path: String,
        #[source]
        source: io::Error,
    },
}

/// Blocks until `path` has a free instance, or the wait times out. `NMPWAIT_USE_DEFAULT_WAIT` takes
/// the timeout the agent set when it created the pipe instead of inventing one.
fn wait_for_free_instance(path: &str) -> io::Result<()> {
    let wide_path: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide_path` is a valid null-terminated UTF-16 string live for the duration of this
    // call; `WaitNamedPipeW` reads it but does not retain the pointer past returning.
    let succeeded = unsafe { WaitNamedPipeW(wide_path.as_ptr(), NMPWAIT_USE_DEFAULT_WAIT) };
    if succeeded == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Whether a pipe exists at `path` at all, free or not, without waiting. With `NMPWAIT_NOWAIT` a
/// pipe whose every instance is busy still fails, but with `ERROR_SEM_TIMEOUT`; only a name nothing
/// listens on fails with `ERROR_FILE_NOT_FOUND`.
pub fn exists(path: &str) -> bool {
    let wide_path: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: as in `wait_for_free_instance`.
    let succeeded = unsafe { WaitNamedPipeW(wide_path.as_ptr(), NMPWAIT_NOWAIT) };
    if succeeded != 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() != Some(ERROR_FILE_NOT_FOUND as i32)
}

/// Opens a fresh client handle on `path`, retrying while the agent answers `ERROR_PIPE_BUSY`: wait
/// for a free instance, pause a random jitter, open again, until `settings.connect_timeout`.
///
/// One wait and one retry is not enough under contention: every waiter is woken by the same freed
/// instance and only one of them gets it, so the rest must keep trying, and the jitter keeps them
/// from piling up on the next freed instance in the same instant.
pub async fn connect(path: &str, settings: PipeSettings) -> Result<NamedPipeClient, PipeError> {
    let attempt = async {
        loop {
            match ClientOptions::new().open(path) {
                Ok(pipe) => return Ok(pipe),
                Err(source) if source.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                    let wait_path = path.to_string();
                    // Whether the wait saw a free instance or timed out, the open decides.
                    let _ = tokio::task::spawn_blocking(move || wait_for_free_instance(&wait_path))
                        .await
                        .expect("wait_for_free_instance blocking task panicked");

                    let jitter_ms = rand::random_range(0..=settings.jitter_max_ms);
                    tokio::time::sleep(Duration::from_millis(jitter_ms)).await;
                }
                Err(source) => return Err(source),
            }
        }
    };

    match tokio::time::timeout(settings.connect_timeout, attempt).await {
        Ok(result) => result.map_err(|source| PipeError::Connect {
            path: path.to_string(),
            source,
        }),
        Err(_elapsed) => Err(PipeError::Connect {
            path: path.to_string(),
            source: io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "pipe stayed busy for the whole {:?} connect timeout",
                    settings.connect_timeout
                ),
            ),
        }),
    }
}

/// Relays one stream to a fresh pipe handle. Errors are logged, not returned: one bad stream must
/// not end the others.
pub async fn relay_stream(pipe_path: &str, stream: yamux::Stream, settings: PipeSettings) {
    use tokio_util::compat::FuturesAsyncReadCompatExt;

    let pipe = match connect(pipe_path, settings).await {
        Ok(pipe) => pipe,
        Err(e) => {
            tracing::error!(error = %e, "failed to connect to the ssh-agent named pipe");
            return;
        }
    };
    relay(stream.compat(), pipe).await;
}

/// Copies both ways and ends as soon as either direction does, rather than waiting for both as
/// `tokio::io::copy_bidirectional` does. A named pipe has no half-close (`poll_shutdown` is a no-op)
/// and the agent keeps its end open after answering, so waiting for both would leak a stream and a
/// pipe handle per connection until yamux's stream limit ends the whole server.
async fn relay<C, P>(client: C, pipe: P)
where
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite,
    P: tokio::io::AsyncRead + tokio::io::AsyncWrite,
{
    let (mut client_read, mut client_write) = tokio::io::split(client);
    let (mut pipe_read, mut pipe_write) = tokio::io::split(pipe);

    tokio::select! {
        result = tokio::io::copy(&mut client_read, &mut pipe_write) => {
            if let Err(e) = result {
                tracing::warn!(error = %e, "error relaying client -> agent");
            }
        }
        result = tokio::io::copy(&mut pipe_read, &mut client_write) => {
            if let Err(e) = result {
                tracing::warn!(error = %e, "error relaying agent -> client");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::windows::named_pipe::ServerOptions;

    fn unique_pipe_path(name: &str) -> String {
        format!(r"\\.\pipe\sockrelay-test-{}-{}", name, std::process::id())
    }

    /// The agent answers once and never closes, as the real one does; the client closes after
    /// reading its answer.
    #[tokio::test]
    async fn relay_terminates_once_the_client_closes_even_though_the_agent_never_does() {
        let path = unique_pipe_path("relay-terminates");
        let mut agent = ServerOptions::new().create(&path).unwrap();

        tokio::spawn(async move {
            agent.connect().await.unwrap();
            let mut buf = [0u8; 5];
            agent.read_exact(&mut buf).await.unwrap();
            agent.write_all(b"reply").await.unwrap();
            std::future::pending::<()>().await
        });

        let pipe = connect(&path, PipeSettings::default()).await.unwrap();
        let (mut client_side, server_side) = tokio::io::duplex(64);

        let relay_task = tokio::spawn(relay(server_side, pipe));

        client_side.write_all(b"hello").await.unwrap();
        let mut reply = [0u8; 5];
        client_side.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"reply");

        client_side.shutdown().await.unwrap();
        drop(client_side);

        tokio::time::timeout(std::time::Duration::from_secs(5), relay_task)
            .await
            .expect("relay() hung instead of returning once the client side closed")
            .unwrap();
    }

    #[tokio::test]
    async fn connect_relays_bytes_in_both_directions() {
        let path = unique_pipe_path("connect-echo");
        let mut server = ServerOptions::new().create(&path).unwrap();

        let server_task = tokio::spawn(async move {
            server.connect().await.unwrap();
            let mut buf = [0u8; 5];
            server.read_exact(&mut buf).await.unwrap();
            server.write_all(&buf).await.unwrap();
        });

        let mut client = connect(&path, PipeSettings::default()).await.unwrap();
        client.write_all(b"hello").await.unwrap();
        let mut buf = [0u8; 5];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");

        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn connect_fails_for_a_nonexistent_pipe() {
        let path = unique_pipe_path("does-not-exist");
        match connect(&path, PipeSettings::default()).await {
            Err(PipeError::Connect { .. }) => {}
            Ok(_) => panic!("expected connecting to a nonexistent pipe to fail"),
        }
    }

    #[test]
    fn exists_is_false_for_a_nonexistent_pipe() {
        let path = unique_pipe_path("exists-check-missing");
        assert!(!exists(&path));
    }

    #[tokio::test]
    async fn exists_is_true_for_a_pipe_with_a_free_instance() {
        let path = unique_pipe_path("exists-check-free");
        let _server = ServerOptions::new().create(&path).unwrap();
        assert!(exists(&path));
    }

    #[tokio::test]
    async fn exists_is_true_even_when_every_instance_is_busy() {
        let path = unique_pipe_path("exists-check-busy");
        let _server = ServerOptions::new().max_instances(1).create(&path).unwrap();
        let _client = ClientOptions::new().open(&path).unwrap();

        assert!(exists(&path));
    }

    #[tokio::test]
    async fn connect_waits_for_a_busy_instance_to_free_up_and_then_succeeds() {
        let path = unique_pipe_path("busy-then-free");
        let server = ServerOptions::new().max_instances(1).create(&path).unwrap();

        // A client's open completes without the server calling `connect()` first.
        let client_a = ClientOptions::new().open(&path).unwrap();

        match ClientOptions::new().open(&path) {
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {}
            other => panic!("expected the sole instance to be busy, got {other:?}"),
        }

        let connect_task =
            tokio::spawn(async move { connect(&path, PipeSettings::default()).await });

        // Lets the task hit the busy pipe and start waiting before the instance is freed.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        // A disconnected instance takes a new connection only after another `connect()`.
        server.disconnect().unwrap();
        drop(client_a);
        server.connect().await.unwrap();

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), connect_task)
            .await
            .expect("connect() did not return promptly after the instance was freed")
            .unwrap();
        result.expect("connect() should succeed once the busy instance is freed");
    }
}
