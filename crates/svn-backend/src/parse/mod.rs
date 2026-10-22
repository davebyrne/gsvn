//! Streaming parsers for svn's XML and unified-diff output.
//!
//! All parsers are pure functions over a `BufRead` so they can be fed a
//! child process's stdout directly or a fixture file in tests. Parsers that
//! may see huge outputs take an `emit` callback and call it once per item
//! as soon as the item is complete; they never buffer the whole document.
//!
//! Parsers must be tolerant: unknown elements/attributes are skipped,
//! missing optional fields become `None`.

use crate::model::*;
use std::io::BufRead;

mod blame;
mod diff;
mod info;
mod list;
mod log;
mod proplist;
mod xml;

/// `kind="file"` / `kind="dir"` → [`NodeKind`]; anything else → `None`.
fn parse_kind(s: Option<&str>) -> Option<NodeKind> {
    match s?.trim() {
        "file" => Some(NodeKind::File),
        "dir" => Some(NodeKind::Dir),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("parse error: {0}")]
pub struct ParseError(pub String);

/// `svn info --xml` (one or more `<entry>`).
pub fn info<R: BufRead>(r: R) -> Result<Vec<Info>, ParseError> {
    info::parse(r)
}

/// `svn list --xml` — calls `emit` per `<entry>`.
pub fn list<R: BufRead>(r: R, emit: impl FnMut(Entry)) -> Result<(), ParseError> {
    list::parse(r, emit)
}

/// `svn log --xml [-v]` — calls `emit` per `<logentry>`.
pub fn log<R: BufRead>(r: R, emit: impl FnMut(LogEntry)) -> Result<(), ParseError> {
    log::parse(r, emit)
}

/// `svn blame --xml` — calls `emit` per `<entry>`.
pub fn blame<R: BufRead>(r: R, emit: impl FnMut(BlameLine)) -> Result<(), ParseError> {
    blame::parse(r, emit)
}

/// `svn proplist -v --xml` for a single target.
pub fn proplist<R: BufRead>(r: R) -> Result<Vec<Property>, ParseError> {
    proplist::parse(r)
}

/// `svn diff --internal-diff` unified output — calls `emit` per file section.
/// Input is raw bytes; non-UTF-8 content is decoded lossily.
pub fn diff<R: BufRead>(r: R, emit: impl FnMut(FileDiff)) -> Result<(), ParseError> {
    diff::parse(r, emit)
}
