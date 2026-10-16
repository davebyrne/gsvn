//! Process runner: spawning svn, cancellation, per-host concurrency limits,
//! auth/cert prompts and error classification (DESIGN.md §3.1, §9, §12).

use crate::error::ErrorKind;

/// Shared state behind [`crate::Svn`].
pub(crate) struct Inner {}

/// Classify svn stderr: `(kind, most specific E-code, message line)`.
pub(crate) fn classify_stderr(stderr: &str) -> (ErrorKind, Option<u32>, String) {
    let _ = stderr;
    todo!("classify_stderr")
}
