//! Error type for all backend operations (DESIGN.md §12).

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorKind {
    /// Path or revision doesn't exist (E160013, E170000 "path not found",
    /// E195012, E160006 "no such revision", ...).
    NotFound,
    /// Authentication failed or is required (E170001, E215004, 401/403).
    /// Only returned after the auth prompt was declined or failed.
    Auth,
    /// TLS certificate not trusted (E230001). Only returned after the cert
    /// prompt was declined.
    Cert,
    /// DNS, connection refused/reset, timeout (E670002, E175002, E170013, ...).
    Network,
    /// The job was cancelled (dropped or [`crate::CancelToken::cancel`]).
    Cancelled,
    /// The `svn` binary could not be started.
    SvnNotFound,
    /// svn's output could not be parsed.
    Parse,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct SvnError {
    pub kind: ErrorKind,
    /// The most specific `E######` code found in stderr, if any.
    pub code: Option<u32>,
    /// One-line human-readable message (the most relevant svn error line,
    /// without the `svn: E######: ` prefix).
    pub message: String,
    /// The command line that was run, for the "Details" expander. Never
    /// contains passwords.
    pub command: String,
    /// Full stderr of the svn process.
    pub stderr: String,
}

impl SvnError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            code: None,
            message: message.into(),
            command: String::new(),
            stderr: String::new(),
        }
    }

    pub fn cancelled() -> Self {
        Self::new(ErrorKind::Cancelled, "Cancelled")
    }

    pub fn is_cancelled(&self) -> bool {
        self.kind == ErrorKind::Cancelled
    }
}

impl fmt::Display for SvnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.code {
            Some(code) => write!(f, "E{code:06}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

/// Classify an svn failure from its stderr. Implemented in the runner.
pub fn classify(stderr: &str) -> (ErrorKind, Option<u32>, String) {
    crate::runner::classify_stderr(stderr)
}
