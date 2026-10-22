//! Subversion access for gsvn.
//!
//! Runs the `svn` command-line client with `--xml --non-interactive` and
//! parses its output. Has no GTK dependency so it can be tested against
//! recorded XML fixtures. See DESIGN.md §3.
//!
//! Entry point is [`Svn`]. Every operation returns a [`Job`] (single result)
//! or a [`Stream`] (batches of results) immediately; the work happens on a
//! worker thread. Dropping a `Job`/`Stream` cancels it and kills the `svn`
//! child process.

mod cache;
pub mod error;
pub mod job;
pub mod model;
pub mod parse;
mod runner;
mod svn;
pub mod url;

pub use error::{ErrorKind, SvnError};
pub use job::{CancelToken, Job, Stream};
pub use model::*;
pub use svn::{AuthPrompt, CertPrompt, Credentials, Priority, Svn, SvnConfig};
pub use jiff::Timestamp;
pub use url::RepoUrl;
