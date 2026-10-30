use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

/// Where every diagnostic this process produces ends up — both `tracing`'s own formatted output
/// (via the `MakeWriter` impl below) and raw bytes forwarded from the spawned Windows server's own
/// stderr (see `main.rs`'s server-log forwarding task), so the two interleave in one place instead
/// of going through separate, inconsistent channels.
///
/// Two independent questions decide where a given write actually ends up, both checked on every
/// write, in parallel — not a single either/or choice:
///
/// - **Is a log file configured (`--log-file`)?** If so, every write goes there. If not, `file` is
///   `None` and nothing is opened or written — there's no implicit fallback path (like
///   `/dev/null`) standing in for "no file".
/// - **Are we still attached to the terminal?** While `attached_to_terminal` is true (from
///   startup until a successful daemonized start's `detach_stdio`, see `main.rs` — this is
///   *never* flipped in `--foreground`, which has no such transition), every write also mirrors to
///   stderr.
///
/// So all four combinations are meaningful: both, either, or neither can apply to a given run,
/// entirely independently of each other.
#[derive(Clone)]
pub struct LogSink {
    file: Option<Arc<Mutex<std::fs::File>>>,
    attached_to_terminal: Arc<AtomicBool>,
}

impl LogSink {
    /// `path: None` means no `--log-file` was given — this is a valid, always-successful case: no
    /// file is opened, and every write simply skips that half of `Write::write` below.
    pub fn open(path: Option<&Path>) -> io::Result<Self> {
        let file = path
            .map(|path| {
                let file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)?;
                Ok::<_, io::Error>(Arc::new(Mutex::new(file)))
            })
            .transpose()?;
        Ok(Self {
            file,
            attached_to_terminal: Arc::new(AtomicBool::new(true)),
        })
    }

    /// Whether `tracing` may color its output: only when every write lands on a terminal, since
    /// the journal (stderr under systemd) and a log file both keep escape sequences verbatim.
    pub fn wants_color(&self, stderr_is_terminal: bool) -> bool {
        stderr_is_terminal && self.file.is_none()
    }

    /// Stops mirroring further writes to stderr — called once, right alongside `detach_stdio`,
    /// once a successful daemonized start means the invoking terminal is no longer relevant. Never
    /// called at all in `--foreground` (see this type's own doc comment).
    pub fn detach(&self) {
        self.attached_to_terminal.store(false, Ordering::Relaxed);
    }
}

impl Write for LogSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(file) = &self.file {
            file.lock()
                .expect("log file mutex poisoned")
                .write_all(buf)?;
        }
        if self.attached_to_terminal.load(Ordering::Relaxed) {
            // Best-effort: a full stderr (e.g. a closed terminal) must not stop logging.
            let _ = io::stderr().write_all(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(file) = &self.file {
            file.lock().expect("log file mutex poisoned").flush()?;
        }
        Ok(())
    }
}

/// Marks every line the spawned server writes to its stderr as the server's, so both processes'
/// diagnostics can share one sink and still be told apart.
const SERVER_LINE_PREFIX: &str = "server: ";

/// Copies the spawned server's stderr into `sink` line by line, each line prefixed with
/// [`SERVER_LINE_PREFIX`], until the server closes it. The line itself is passed through as the
/// server wrote it — its own `tracing` setup already formatted it — apart from a trailing `\r`,
/// which Windows processes may end lines with.
pub async fn forward_server_lines(server_stderr: impl AsyncRead + Unpin, mut sink: impl Write) {
    let mut server_stderr = BufReader::new(server_stderr);
    let mut raw = Vec::new();
    loop {
        raw.clear();
        match server_stderr.read_until(b'\n', &mut raw).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                let line = String::from_utf8_lossy(&raw);
                let line = line.strip_suffix('\n').unwrap_or(&line);
                let line = line.strip_suffix('\r').unwrap_or(line);
                // One write per line, so a line can't interleave with this process's own output.
                let _ = sink.write_all(format!("{SERVER_LINE_PREFIX}{line}\n").as_bytes());
            }
        }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogSink {
    type Writer = LogSink;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_a_terminal_with_no_log_file() {
        assert!(LogSink::open(None).unwrap().wants_color(true));
    }

    #[test]
    fn does_not_color_stderr_that_is_not_a_terminal() {
        assert!(!LogSink::open(None).unwrap().wants_color(false));
    }

    #[tokio::test]
    async fn prefixes_each_server_line() {
        let mut sink = Vec::new();
        forward_server_lines(&b"first\r\nsecond\n\xffthird"[..], &mut sink).await;
        assert_eq!(
            String::from_utf8(sink).unwrap(),
            "server: first\nserver: second\nserver: \u{fffd}third\n"
        );
    }

    #[test]
    fn does_not_color_a_log_file() {
        let _guard = crate::test_support::UMASK.blocking_lock();
        let dir = tempfile::tempdir().unwrap();
        let sink = LogSink::open(Some(&dir.path().join("client.log"))).unwrap();
        assert!(!sink.wants_color(true));
    }
}
