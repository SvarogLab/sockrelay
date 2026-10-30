use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::net::{UnixListener, UnixStream};

#[derive(Debug, thiserror::Error)]
pub enum ListenError {
    #[error("{path:?} is already in use by a live listener")]
    AlreadyInUse { path: PathBuf },
    #[error("failed to bind unix socket at {path:?}: {source}")]
    Bind {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to acquire the startup lock for {path:?}: {source}")]
    Lock {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Binds `socket_path` with permission bits `mode` (e.g. `0o600`), reclaiming a stale socket file
/// left over from an unclean previous exit if needed (see [`bind_reclaiming_stale`]). Split out
/// from [`accept_loop`] so a caller can do something — e.g. eagerly cold-starting the Windows
/// server — between claiming the socket and starting to accept connections on it.
pub async fn bind(socket_path: impl AsRef<Path>, mode: u32) -> Result<UnixListener, ListenError> {
    bind_reclaiming_stale(socket_path.as_ref(), mode).await
}

/// Calls `handle` (on its own spawned task) for every connection accepted on `listener`, for as
/// long as the process runs.
///
/// A transient `accept()` error (`EMFILE`/`ENFILE`/`ECONNABORTED` and the like) is logged and
/// retried after `error_backoff` rather than ending the loop — this daemon exists specifically to
/// survive bursts of connections (parallel Ansible SSH forks), and killing the whole thing because
/// one accept() call hit a momentary resource limit would be a worse outage than the bug this
/// project fixes. The listener itself isn't recreated on error since none of these errors indicate
/// it's actually broken. `error_backoff` should be long enough to not hot-spin burning CPU on a
/// sustained condition like fd exhaustion, short enough not to noticeably delay recovery once it
/// clears — configurable (CLI flag or env var, see `config.rs`) rather than hardcoded.
pub async fn accept_loop<F, Fut>(listener: UnixListener, error_backoff: Duration, handle: F)
where
    F: Fn(UnixStream) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let path = listener
        .local_addr()
        .ok()
        .and_then(|addr| addr.as_pathname().map(Path::to_path_buf));
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let handle = handle.clone();
                tokio::spawn(async move { handle(stream).await });
            }
            Err(source) => {
                tracing::warn!(path = ?path, error = %source, "accept() failed, retrying");
                tokio::time::sleep(error_backoff).await;
            }
        }
    }
}

/// Binds `path`, reclaiming it first if it's a stale socket file left behind by a previous,
/// uncleanly-terminated instance.
///
/// `bind()` failing with `AddrInUse` is ambiguous on its own: it means *either* a live process is
/// already listening at `path`, *or* the path merely still exists on disk with nothing behind it.
/// The two cases are told apart by attempting a client connection to the existing path:
/// `ConnectionRefused` means nothing is listening (safe to unlink and retry), any other outcome
/// (a successful connect, most notably) means a real listener owns it and we must not steal it.
///
/// The whole check-then-act sequence is protected by a startup-only file lock (`{path}.lock`,
/// via `std::fs::File::lock` — no external crate, and released automatically on drop/process
/// exit): without it, two instances starting at nearly the same moment could both see the stale
/// state and race to unlink+rebind, with the loser deleting the path out from under the winner's
/// now-live listener. This is deliberately narrow in scope — a one-time daemon-startup sequence,
/// not a per-connection guard exercised on every request.
///
/// Every actual `bind()` call goes through [`bind_with_restrictive_umask`] rather than a
/// bind-then-`chmod` sequence: unix sockets otherwise pick up plain umask-derived permissions
/// (typically group/other-readable), and a `chmod` *after* `bind()` would leave a real — if
/// short — window where the socket exists with those looser permissions before being tightened.
/// Restricting the umask before creation instead means the file never exists with the wrong
/// permissions at all. This is the only thing standing between "any local user can drive this
/// agent relay" and not (beyond `mode` itself being restrictive) — relying on the containing
/// directory being locked down (true for the default `~/.ssh`, not guaranteed for a
/// `SOCKRELAY_SOCKET` override) isn't enough on its own.
async fn bind_reclaiming_stale(path: &Path, mode: u32) -> Result<UnixListener, ListenError> {
    let _lock = acquire_startup_lock(path).await?;

    match bind_with_restrictive_umask(path, mode) {
        Ok(listener) => Ok(listener),
        Err(source) if source.kind() == io::ErrorKind::AddrInUse => {
            match UnixStream::connect(path).await {
                Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
                    std::fs::remove_file(path).map_err(|source| ListenError::Bind {
                        path: path.to_path_buf(),
                        source,
                    })?;
                    bind_with_restrictive_umask(path, mode).map_err(|source| ListenError::Bind {
                        path: path.to_path_buf(),
                        source,
                    })
                }
                _ => Err(ListenError::AlreadyInUse {
                    path: path.to_path_buf(),
                }),
            }
        }
        Err(source) => Err(ListenError::Bind {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Binds `path` with the process umask temporarily restricted so the resulting file permissions
/// come out as exactly `mode` (any bit *not* set in `mode` goes into the umask), restoring the
/// previous umask immediately afterward.
///
/// Deliberately synchronous with no `.await` inside the guarded scope: umask is process-wide,
/// mutable state, not per-task, so if this spanned an `.await` point, any *other* task creating a
/// file concurrently on another worker thread during that yield could transiently pick up the
/// wrong permissions too. `UnixListener::bind` itself is a plain syscall (no `.await` needed), so
/// the guard's window is exactly one syscall wide.
fn bind_with_restrictive_umask(path: &Path, mode: u32) -> io::Result<UnixListener> {
    let _umask = RestrictiveUmask::install(mode);
    UnixListener::bind(path)
}

struct RestrictiveUmask {
    previous: libc::mode_t,
}

impl RestrictiveUmask {
    fn install(mode: u32) -> Self {
        // SAFETY: `umask(2)` has no failure mode and only affects this process's file-creation
        // mask; it's `unsafe` in the `libc` binding purely because every raw FFI call is.
        let previous = unsafe { libc::umask((!mode & 0o777) as libc::mode_t) };
        Self { previous }
    }
}

impl Drop for RestrictiveUmask {
    fn drop(&mut self) {
        unsafe { libc::umask(self.previous) };
    }
}

/// Acquires an exclusive lock on `{socket_path}.lock`, blocking (via `spawn_blocking`, so this
/// doesn't stall the runtime) until any concurrently-starting instance's lock is released. Held
/// for the lifetime of the returned `File` — dropping it (at the end of
/// [`bind_reclaiming_stale`]) releases the lock.
///
/// The lock file itself is deliberately **never** removed (only ever created-if-missing and
/// reused): unlinking a still-referenced flock file is a classic bug — the lock is tied to the
/// open file description, not the path, so a concurrent racer already blocked on opening/locking
/// it would end up locking an unlinked inode while a third racer creates a fresh file at the same
/// path, and the two would no longer exclude each other. A single small file lingering forever is
/// a much smaller cost than that.
async fn acquire_startup_lock(socket_path: &Path) -> Result<std::fs::File, ListenError> {
    let mut lock_path = socket_path.as_os_str().to_owned();
    lock_path.push(".lock");
    let lock_path = PathBuf::from(lock_path);

    tokio::task::spawn_blocking(move || {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false) // it's a lock marker, not a data file — never overwrite its content
            .write(true)
            .open(&lock_path)
            .map_err(|source| ListenError::Lock {
                path: lock_path.clone(),
                source,
            })?;
        file.lock().map_err(|source| ListenError::Lock {
            path: lock_path.clone(),
            source,
        })?;
        Ok(file)
    })
    .await
    .expect("spawn_blocking task panicked")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::LazyLock;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::mpsc;

    /// `umask` is process-wide state, and cargo runs `#[tokio::test]` functions in parallel on
    /// separate OS threads within the same process — every test in this module goes through
    /// `bind()`, which briefly changes the umask (see `RestrictiveUmask`). Without serializing
    /// them against each other, one test's temporary umask change could be clobbered mid-flight
    /// by another running concurrently, corrupting *both* tests' socket permissions in a way
    /// that can't happen in production (where `bind()` is only ever called once, at startup).
    static UMASK_TEST_LOCK: LazyLock<tokio::sync::Mutex<()>> =
        LazyLock::new(|| tokio::sync::Mutex::new(()));

    #[tokio::test]
    async fn accept_loop_invokes_the_handler_for_each_accepted_connection() {
        let _guard = UMASK_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.sock");

        let listener = bind(&path, 0o600).await.unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        tokio::spawn(async move {
            let _ = accept_loop(listener, Duration::from_millis(100), move |mut stream| {
                let tx = tx.clone();
                async move {
                    let mut buf = Vec::new();
                    let _ = stream.read_to_end(&mut buf).await;
                    let _ = tx.send(buf);
                }
            })
            .await;
        });

        let mut client = tokio::net::UnixStream::connect(&path).await.unwrap();
        client.write_all(b"hello").await.unwrap();
        client.shutdown().await.unwrap();

        let received = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("handler was not invoked")
            .unwrap();
        assert_eq!(received, b"hello");
    }

    #[tokio::test]
    async fn bind_reclaims_a_stale_socket_file() {
        let _guard = UMASK_TEST_LOCK.lock().await;
        let _no_children = crate::test_support::CHILD_PROCESSES.write().await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.sock");

        let listener = UnixListener::bind(&path).unwrap();
        drop(listener); // no unlink — this is exactly the "unclean exit" scenario
        assert!(
            path.exists(),
            "test setup: socket file should linger after drop"
        );

        bind_reclaiming_stale(&path, 0o600)
            .await
            .expect("should reclaim the stale socket file");
    }

    #[tokio::test]
    async fn bind_refuses_to_steal_a_live_listener() {
        let _guard = UMASK_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.sock");

        let _live_listener = UnixListener::bind(&path).unwrap();

        match bind_reclaiming_stale(&path, 0o600).await {
            Err(ListenError::AlreadyInUse { .. }) => {}
            other => panic!("expected AlreadyInUse, got {}", other.is_ok()),
        }
    }

    #[tokio::test]
    async fn bind_creates_the_socket_with_the_requested_mode() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = UMASK_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.sock");

        let _listener = bind(&path, 0o600).await.unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[tokio::test]
    async fn bind_restores_the_previous_umask_afterward() {
        let _guard = UMASK_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.sock");

        // SAFETY: umask(2) is a plain, infallible syscall; reading and restoring it around the
        // test is the only way to assert `bind` doesn't leak a changed umask.
        let original = unsafe { libc::umask(0o022) };
        let _listener = bind(&path, 0o600).await.unwrap();
        let after = unsafe { libc::umask(original) };
        assert_eq!(after, 0o022, "bind() must restore the umask it found");
    }
}
