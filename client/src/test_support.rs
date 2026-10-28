//! State shared between the test modules of this binary.

/// Held for reading by every test that spawns a child process, and for writing by a test that
/// needs a socket it has just closed to stay closed. Between `fork` and `exec` a child holds a
/// copy of every open file descriptor - a listener another test has just dropped included - so
/// a connection to that listener still succeeds until the child execs.
pub(crate) static CHILD_PROCESSES: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());
