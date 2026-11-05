# Code style

## Formatting

`rustfmt` with the repository's `rustfmt.toml`; `cargo fmt --all -- --check` passes. Markdown passes
`markdownlint-cli2` with `.markdownlint-cli2.jsonc`.

## Lints

`cargo clippy --workspace --all-targets --all-features -- -D warnings` passes on Linux, and
`cargo clippy -p sockrelay-server --all-targets` passes for the Windows target (see the README's
Testing section for the environment it needs). Workspace-wide lints live in `[workspace.lints]` in
`Cargo.toml`; a threshold goes into `clippy.toml` only with a comment naming the code it is for.

## Layout

- A module with submodules lives in `name.rs` beside a `name/` directory, never in `name/mod.rs`
  (`clippy::mod_module_files` enforces it).
- Code that only builds on Windows gates its whole file with `#![cfg(windows)]`, so the workspace
  still checks and tests on Linux.
- Tests sit in a `tests` module at the bottom of the file they test, or in `name/tests.rs` when they
  outgrow it. A test that drives the crate only from outside, through its public API or its built
  binary, goes in `tests/`.

## Naming

Types and functions say what they are in the domain: the `socket` is the unix socket on the Linux
side, the `pipe` the Windows named pipe, a `stream` one yamux stream relaying one connection between
the two. Test names are sentences stating the behavior they hold.

## Errors

- Each module that can fail has its own `thiserror` enum; errors cross a module boundary through a
  `From` impl or a `#[source]` field, not by stringifying.
- `unwrap` belongs in tests. Elsewhere, an `expect` states the invariant that makes it unreachable.
- A failure the process can't recover from (the server is gone, the socket systemd passed is
  unusable) ends the process with a non-zero status; a supervisor restarts it.

## Tests

- Every test must be able to fail: `cargo mutants` reports no mutant missed, for the client on
  Linux and for the server on Windows. A mutant no test can tell from the real code goes into
  `exclude_re` in `.cargo/mutants.toml`, with a comment giving the reason; nothing in the code
  marks it.
- A test no mutant needs stays only if its doc comment names the behavior it holds: a documented
  promise, or a contract of the OS, of WSL or of systemd.
- A test that starts a process owns it before it can fail, so a failed test never leaves one
  behind holding its output open.

## Comments

Code says what it does; a comment says why, when the reason lives outside the code: a quirk of
Windows, of WSL or of the protocol being relayed, with the behavior it forces.

## Dependencies

Requirements in `[workspace.dependencies]` name the current release. A test-only dependency on a
crate's unstable API is acceptable; a runtime one is not.
