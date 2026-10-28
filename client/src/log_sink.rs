use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

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

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogSink {
    type Writer = LogSink;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
