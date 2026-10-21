//! Plain data types produced by the backend.

use crate::url::RepoUrl;
use jiff::Timestamp;

/// Revision number.
pub type RevNum = u64;

/// A revision as the user may specify it. The browser resolves this to a
/// [`RevNum`] once (via [`crate::Svn::info`]) and works with numbers after
/// that so cache keys stay immutable (DESIGN.md §4).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Rev {
    Head,
    Number(RevNum),
    /// Passed to svn as `{DATE}`, e.g. `2024-01-31` or `2024-01-31T12:00`.
    Date(String),
}

impl Rev {
    /// Format for svn's `-r` option or a peg suffix (`HEAD`, `123`, `{2024-01-31}`).
    pub fn to_svn_arg(&self) -> String {
        match self {
            Rev::Head => "HEAD".to_owned(),
            Rev::Number(n) => n.to_string(),
            Rev::Date(d) => format!("{{{d}}}"),
        }
    }
}

impl From<RevNum> for Rev {
    fn from(n: RevNum) -> Self {
        Rev::Number(n)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeKind {
    File,
    Dir,
}

/// The last commit that changed a node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    pub rev: RevNum,
    /// `None` when the revision has no `svn:author` (or it is unreadable).
    pub author: Option<String>,
    pub date: Option<Timestamp>,
}

/// Result of `svn info --xml URL@REV`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    pub url: RepoUrl,
    pub root: RepoUrl,
    pub uuid: String,
    /// Path inside the repository, starting with `/` (`/` for the root).
    pub repos_path: String,
    /// The operative revision (for `URL@HEAD` this is the HEAD revision number).
    pub rev: RevNum,
    pub kind: NodeKind,
    pub last_changed: CommitInfo,
    /// File size in bytes (files only; svn ≥ 1.9 reports it for URLs).
    pub size: Option<u64>,
}

/// One child of a directory from `svn list --xml --depth immediates`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Name only (no slashes), unescaped.
    pub name: String,
    pub kind: NodeKind,
    /// Files only.
    pub size: Option<u64>,
    pub last_changed: CommitInfo,
}

/// Parameters for `svn log`. Pages are requested by setting `start` to the
/// last revision of the previous page minus one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LogQuery {
    /// Newest revision of the range (log runs `-r start:end`, newest first).
    pub start: Rev,
    /// Oldest revision of the range; usually `Rev::Number(0)`.
    pub end: Rev,
    /// `-l`. 0 means unlimited (avoid).
    pub limit: u32,
    /// Each element becomes `--search`/`--search-and`: entries must match
    /// all terms. Empty = no filtering.
    pub search: Vec<String>,
    pub stop_on_copy: bool,
    /// `-v`: include changed paths. Normally false for paging (DESIGN.md §6).
    pub verbose: bool,
}

impl LogQuery {
    /// A first page of `limit` entries starting at `start`, no search.
    pub fn page(start: RevNum, limit: u32) -> Self {
        Self {
            start: Rev::Number(start),
            end: Rev::Number(0),
            limit,
            search: Vec::new(),
            stop_on_copy: false,
            verbose: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub rev: RevNum,
    pub author: Option<String>,
    pub date: Option<Timestamp>,
    /// Full commit message (may be empty).
    pub message: String,
    /// Empty unless the query was verbose.
    pub changed_paths: Vec<ChangedPath>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChangeAction {
    Added,
    Modified,
    Deleted,
    Replaced,
}

impl ChangeAction {
    pub fn letter(self) -> char {
        match self {
            ChangeAction::Added => 'A',
            ChangeAction::Modified => 'M',
            ChangeAction::Deleted => 'D',
            ChangeAction::Replaced => 'R',
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedPath {
    pub action: ChangeAction,
    /// Repository path starting with `/`.
    pub path: String,
    /// `None` if the server didn't report it (old servers).
    pub kind: Option<NodeKind>,
    /// Copy source `(path, rev)` if the node was copied.
    pub copy_from: Option<(String, RevNum)>,
    pub text_mods: bool,
    pub prop_mods: bool,
}

/// One line of `svn blame --xml`. Blame XML carries no text; the viewer
/// zips this with `svn cat` output by `line_no`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameLine {
    /// 1-based.
    pub line_no: u64,
    /// `None` for lines older than the blame range start (`-r X:N`).
    pub commit: Option<CommitInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Property {
    pub name: String,
    pub value: String,
}

/// Output of `svn cat`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatOutput {
    pub data: Vec<u8>,
    /// True if output was cut off at the requested `max_bytes`.
    pub truncated: bool,
}

/// What to diff (DESIGN.md §7.1).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DiffSpec {
    /// `svn diff -c REV URL@REV` — one commit, whole tree under `url` or one file.
    Change { url: RepoUrl, rev: RevNum },
    /// `svn diff -c REV URL@PEG` — one commit, where `url` names the node as
    /// it exists at `peg` (normally `peg >= rev`). svn traces the node back
    /// to `rev`, so unlike [`DiffSpec::Change`] this works for paths that
    /// were renamed or moved after `rev`. File paths in the output are the
    /// names at `rev`.
    ChangeAt {
        url: RepoUrl,
        peg: RevNum,
        rev: RevNum,
    },
    /// `svn diff -r FROM:TO URL@PEG` — a path between two revisions.
    Range {
        url: RepoUrl,
        peg: RevNum,
        from: RevNum,
        to: RevNum,
    },
}

/// One file section of a unified diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// Path from the `Index:` line, relative to the diff target.
    pub path: String,
    /// From the `--- ` / `+++ ` lines, e.g. `foo.c\t(revision 41)`; `None`
    /// when absent (property-only or binary changes).
    pub old_label: Option<String>,
    pub new_label: Option<String>,
    pub content: DiffContent,
    /// Property changes for this path (`Property changes on:` section).
    pub prop_changes: Vec<PropChange>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffContent {
    Text(Vec<Hunk>),
    /// svn printed "Cannot display: file marked as a binary type."
    Binary,
    /// Only properties changed (or an empty add/delete).
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u64,
    pub old_len: u64,
    pub new_start: u64,
    pub new_len: u64,
    /// Text after the second `@@` (function context), may be empty.
    pub section: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    /// Line number in the old file (Context/Removed).
    pub old_no: Option<u64>,
    /// Line number in the new file (Context/Added).
    pub new_no: Option<u64>,
    /// Text without the leading marker and without the trailing newline.
    /// Non-UTF-8 bytes are replaced (lossy).
    pub text: String,
    /// The line was followed by `\ No newline at end of file`.
    pub no_newline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropChange {
    pub name: String,
    pub old: Option<String>,
    pub new: Option<String>,
}
