//! State shared between the test modules of this binary.

/// Held for reading by every test that spawns a child process, and for writing by a test that
/// needs a socket it has just closed to stay closed. Between `fork` and `exec` a child holds a
/// copy of every open file descriptor - a listener another test has just dropped included - so
/// a connection to that listener still succeeds until the child execs.
pub(crate) static CHILD_PROCESSES: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

/// Held by every test that creates a file. `umask` is process-wide and `unix_listener::bind`
/// briefly restricts it (see `RestrictiveUmask`), so a file another test creates in that window —
/// a temporary directory included — comes out with the restricted permissions, and a directory
/// without its search bit can't be entered.
pub(crate) static UMASK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
