//! Pure, GTK-free logic of the log window: search-term splitting, the
//! paging cursor, revision-range parsing and mapping selections to the
//! diff/viewer targets. Unit-tested below.

use svn_backend::{ChangeAction, ChangedPath, DiffSpec, LogQuery, NodeKind, RepoUrl, Rev, RevNum};

/// Revisions per page (DESIGN.md §6).
pub const PAGE_SIZE: u32 = 200;

/// Revision-number window of the first search page. Searches page by
/// revision window rather than by `-l` because svn applies `-l` to the
/// revisions it *examines*, not to the matches, so a short search page says
/// nothing about where the next one should start.
pub const SEARCH_WINDOW_MIN: RevNum = 2_000;
/// Upper bound for the adaptive search window.
pub const SEARCH_WINDOW_MAX: RevNum = 32_000;
/// Fewer matches than this per window widens the next window.
const SEARCH_WANT: usize = 50;
/// More matches than this per window narrows the next window.
const SEARCH_TOO_MANY: usize = 400;

/// Split a search string into terms: whitespace separates terms, double
/// quotes group a phrase into one term. An unterminated quote runs to the
/// end of the input. Empty terms are dropped.
pub fn split_search_terms(input: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for c in input.chars() {
        match c {
            '"' => {
                if quoted || !current.is_empty() {
                    push_term(&mut terms, &mut current);
                }
                quoted = !quoted;
            }
            c if c.is_whitespace() && !quoted => push_term(&mut terms, &mut current),
            c => current.push(c),
        }
    }
    push_term(&mut terms, &mut current);
    terms
}

fn push_term(terms: &mut Vec<String>, current: &mut String) {
    let term = current.trim();
    if !term.is_empty() {
        terms.push(term.to_owned());
    }
    current.clear();
}

/// The user's optional revision range, normalised to `newest >= oldest`
/// and clamped to the window's peg revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RevRange {
    pub newest: RevNum,
    pub oldest: RevNum,
}

impl RevRange {
    /// Build from the two (optional) range entries. Either order is
    /// accepted; a missing bound means "peg" / "r0".
    pub fn new(a: Option<RevNum>, b: Option<RevNum>, peg: RevNum) -> Self {
        let (newest, oldest) = match (a, b) {
            (Some(a), Some(b)) => (a.max(b), a.min(b)),
            (Some(a), None) => (a, 0),
            (None, Some(b)) => (peg, b),
            (None, None) => (peg, 0),
        };
        let newest = newest.min(peg);
        Self {
            newest,
            oldest: oldest.min(newest),
        }
    }
}

/// What the next page request looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageRequest {
    pub start: RevNum,
    pub end: RevNum,
    /// `-l`; 0 = unlimited (search windows).
    pub limit: u32,
}

impl PageRequest {
    /// The full query for this page.
    pub fn query(&self, search: &[String], stop_on_copy: bool) -> LogQuery {
        LogQuery {
            start: Rev::Number(self.start),
            end: Rev::Number(self.end),
            limit: self.limit,
            search: search.to_vec(),
            stop_on_copy,
            verbose: false,
        }
    }
}

/// Paging cursor (DESIGN.md §6).
///
/// - **Plain:** `-r CURSOR:OLDEST -l 200`; the next page starts at
///   `last_rev - 1`; a short page ends the history. With stop-on-copy the
///   next page starts *at* `last_rev` (one overlapping entry, dropped):
///   starting below the copy revision would make svn follow the copy source.
/// - **Search:** `-r CURSOR:CURSOR-WINDOW+1`, unlimited, with an adaptive
///   window; ends when the window reaches `OLDEST`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pager {
    /// Newest revision of the next page; `None` once the history is exhausted.
    cursor: Option<RevNum>,
    oldest: RevNum,
    mode: Mode,
    /// Revision of an entry the next page repeats and must drop.
    overlap: Option<RevNum>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Plain { stop_on_copy: bool },
    Search { window: RevNum },
}

impl Pager {
    pub fn new(range: RevRange, searching: bool, stop_on_copy: bool) -> Self {
        Self {
            cursor: Some(range.newest),
            oldest: range.oldest,
            mode: if searching {
                Mode::Search {
                    window: SEARCH_WINDOW_MIN,
                }
            } else {
                Mode::Plain { stop_on_copy }
            },
            overlap: None,
        }
    }

    pub fn is_done(&self) -> bool {
        self.cursor.is_none()
    }

    /// Mark the history as exhausted (e.g. svn reports the path doesn't
    /// exist below the window start).
    pub fn finish(&mut self) {
        self.cursor = None;
    }

    /// Raise the oldest revision (search + stop-on-copy: the branch start).
    pub fn set_oldest(&mut self, oldest: RevNum) {
        self.oldest = self.oldest.max(oldest);
        if self.cursor.is_some_and(|c| c < self.oldest) {
            self.cursor = None;
        }
    }

    /// Lowest revision the next page can reach, for "searched back to rN".
    pub fn searched_to(&self) -> Option<RevNum> {
        self.cursor.map(|c| c + 1)
    }

    pub fn next_request(&self) -> Option<PageRequest> {
        let start = self.cursor?;
        Some(match self.mode {
            Mode::Plain { .. } => PageRequest {
                start,
                end: self.oldest,
                limit: PAGE_SIZE + u32::from(self.overlap.is_some()),
            },
            Mode::Search { window } => PageRequest {
                start,
                end: start.saturating_sub(window - 1).max(self.oldest),
                limit: 0,
            },
        })
    }

    /// Feed the result of `req`. Returns how many leading entries of
    /// `revs` to skip (an overlapping entry already shown).
    pub fn complete(&mut self, req: PageRequest, revs: &[RevNum]) -> usize {
        let skip = usize::from(self.overlap.is_some() && revs.first() == self.overlap.as_ref());
        self.overlap = None;
        match &mut self.mode {
            Mode::Plain { stop_on_copy } => {
                let last = revs.last().copied();
                self.cursor = match last {
                    Some(last) if revs.len() >= req.limit as usize && last > self.oldest => {
                        if *stop_on_copy {
                            self.overlap = Some(last);
                            Some(last)
                        } else {
                            Some(last - 1)
                        }
                    }
                    _ => None,
                };
            }
            Mode::Search { window } => {
                let matches = revs.len();
                if matches < SEARCH_WANT {
                    *window = (*window * 4).min(SEARCH_WINDOW_MAX);
                } else if matches > SEARCH_TOO_MANY {
                    *window = (*window / 2).max(SEARCH_WINDOW_MIN);
                }
                self.cursor = (req.end > self.oldest).then(|| req.end - 1);
            }
        }
        skip
    }
}

/// Whether an svn error code on a *follow-up* page means "the path doesn't
/// exist that far back", i.e. the end of its history.
pub fn is_end_of_history(code: Option<u32>) -> bool {
    matches!(code, Some(195012 | 160013))
}

/// The diff to open for the selected revisions of the log of `url@peg`
/// (sorted or not). `None` unless exactly one or two are selected.
///
/// One revision diffs as `-c N URL@PEG` ([`DiffSpec::ChangeAt`]) rather
/// than `-c N URL@N`: the log follows renames, and `URL@N` doesn't exist for
/// revisions from before a rename, whereas a peg at `PEG` lets svn trace the
/// node back.
pub fn revision_diff(url: &RepoUrl, peg: RevNum, selected: &[RevNum]) -> Option<DiffSpec> {
    match *selected {
        [rev] if rev > 0 => Some(DiffSpec::ChangeAt {
            url: url.clone(),
            peg,
            rev,
        }),
        [a, b] if a != b => Some(DiffSpec::Range {
            url: url.clone(),
            peg,
            from: a.min(b),
            to: a.max(b),
        }),
        _ => None,
    }
}

/// Where the actions on a changed path of revision `rev` point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathTarget {
    pub url: RepoUrl,
    /// Revision at which the node exists: `rev`, or `rev - 1` for a
    /// deletion (the path is gone in `rev`).
    pub rev: RevNum,
    pub diff: Option<DiffSpec>,
    /// Content/blame make sense (files, or unknown kind).
    pub viewable: bool,
}

/// Map a changed path of `rev` to its action targets. Deleted paths don't
/// exist at `rev`, so view/blame/log use `rev - 1`, and the diff runs
/// `-r N-1:N URL@N-1` (verified against svn 1.14: `-c N URL@N` also works
/// for a deleted file, but the explicit peg is unambiguous).
pub fn path_target(root: &RepoUrl, rev: RevNum, cp: &ChangedPath) -> PathTarget {
    let url = root.join(&cp.path);
    let viewable = cp.kind != Some(NodeKind::Dir);
    if cp.action == ChangeAction::Deleted {
        let prev = rev.saturating_sub(1);
        PathTarget {
            diff: (rev > 0).then(|| DiffSpec::Range {
                url: url.clone(),
                peg: prev,
                from: prev,
                to: rev,
            }),
            url,
            rev: prev,
            viewable,
        }
    } else {
        PathTarget {
            diff: Some(DiffSpec::Change { url: url.clone(), rev }),
            url,
            rev,
            viewable,
        }
    }
}

/// First line of a commit message (for the list column).
pub fn first_line(message: &str) -> &str {
    message.trim_start().lines().next().unwrap_or("").trim_end()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_terms() {
        assert_eq!(split_search_terms(""), Vec::<String>::new());
        assert_eq!(split_search_terms("  "), Vec::<String>::new());
        assert_eq!(split_search_terms("alice"), ["alice"]);
        assert_eq!(split_search_terms(" alice  fix\tcrash "), ["alice", "fix", "crash"]);
        assert_eq!(split_search_terms(r#"bob "fix crash" io"#), ["bob", "fix crash", "io"]);
        assert_eq!(split_search_terms(r#""unterminated phrase"#), ["unterminated phrase"]);
        assert_eq!(split_search_terms(r#"a"b c"d"#), ["a", "b c", "d"]);
        assert_eq!(split_search_terms(r#""" x"#), ["x"]);
        assert_eq!(split_search_terms("jürgen BUG-12"), ["jürgen", "BUG-12"]);
    }

    #[test]
    fn rev_range() {
        assert_eq!(RevRange::new(None, None, 50), RevRange { newest: 50, oldest: 0 });
        assert_eq!(RevRange::new(Some(10), Some(40), 50), RevRange { newest: 40, oldest: 10 });
        assert_eq!(RevRange::new(Some(40), Some(10), 50), RevRange { newest: 40, oldest: 10 });
        assert_eq!(RevRange::new(Some(90), None, 50), RevRange { newest: 50, oldest: 0 });
        assert_eq!(RevRange::new(None, Some(20), 50), RevRange { newest: 50, oldest: 20 });
        assert_eq!(RevRange::new(None, Some(70), 50), RevRange { newest: 50, oldest: 50 });
    }

    fn revs(from: RevNum, n: usize) -> Vec<RevNum> {
        (0..n as RevNum).map(|i| from - i).collect()
    }

    #[test]
    fn plain_paging() {
        let mut p = Pager::new(RevRange::new(None, None, 1000), false, false);
        let r = p.next_request().unwrap();
        assert_eq!(r, PageRequest { start: 1000, end: 0, limit: 200 });
        assert_eq!(p.complete(r, &revs(1000, 200)), 0);
        let r = p.next_request().unwrap();
        assert_eq!(r, PageRequest { start: 800, end: 0, limit: 200 });
        // Sparse history: revision numbers skip, the cursor follows the last entry.
        let page: Vec<RevNum> = (0..200).map(|i| 790 - i * 3).collect();
        p.complete(r, &page);
        assert_eq!(p.next_request().unwrap().start, 790 - 199 * 3 - 1);
        // A short page ends the history.
        let r = p.next_request().unwrap();
        p.complete(r, &[10, 5, 1]);
        assert!(p.is_done());
        assert_eq!(p.next_request(), None);
    }

    #[test]
    fn plain_paging_stops_at_oldest() {
        let mut p = Pager::new(RevRange::new(Some(399), Some(200), 1000), false, false);
        let r = p.next_request().unwrap();
        assert_eq!(r, PageRequest { start: 399, end: 200, limit: 200 });
        p.complete(r, &revs(399, 200));
        assert!(p.is_done(), "last rev == oldest");
        // A full page ending at r0 ends too.
        let mut p = Pager::new(RevRange::new(None, None, 199), false, false);
        let r = p.next_request().unwrap();
        p.complete(r, &revs(199, 200));
        assert!(p.is_done());
        // Empty page.
        let mut p = Pager::new(RevRange::new(None, None, 5), false, false);
        let r = p.next_request().unwrap();
        p.complete(r, &[]);
        assert!(p.is_done());
    }

    #[test]
    fn stop_on_copy_paging_overlaps() {
        let mut p = Pager::new(RevRange::new(None, None, 1000), false, true);
        let r = p.next_request().unwrap();
        assert_eq!(r.limit, 200);
        assert_eq!(p.complete(r, &revs(1000, 200)), 0);
        // Next page starts *at* the last revision and asks for one more.
        let r = p.next_request().unwrap();
        assert_eq!(r, PageRequest { start: 801, end: 0, limit: 201 });
        // The copy happened at r801: svn returns just that entry → done.
        assert_eq!(p.complete(r, &[801]), 1);
        assert!(p.is_done());

        let mut p = Pager::new(RevRange::new(None, None, 1000), false, true);
        let r = p.next_request().unwrap();
        p.complete(r, &revs(1000, 200));
        let r = p.next_request().unwrap();
        assert_eq!(p.complete(r, &revs(801, 201)), 1);
        assert_eq!(p.next_request().unwrap().start, 601);
    }

    #[test]
    fn search_paging_windows() {
        let mut p = Pager::new(RevRange::new(None, None, 200_000), true, false);
        let r = p.next_request().unwrap();
        assert_eq!(r, PageRequest { start: 200_000, end: 198_001, limit: 0 });
        // Few matches: window widens.
        p.complete(r, &[199_990, 199_000]);
        let r = p.next_request().unwrap();
        assert_eq!(r, PageRequest { start: 198_000, end: 190_001, limit: 0 });
        // Many matches: narrows again.
        p.complete(r, &revs(198_000, 1000));
        let r = p.next_request().unwrap();
        assert_eq!(r.start - r.end + 1, 4_000);
        assert_eq!(p.searched_to(), Some(190_001));
        // Window growth is capped, and the search ends at r0 in few steps.
        let mut n = 0;
        while let Some(r) = p.next_request() {
            assert!(r.start - r.end < SEARCH_WINDOW_MAX);
            p.complete(r, &[]);
            n += 1;
        }
        assert!(n < 10, "{n}");
        assert!(p.is_done());
    }

    #[test]
    fn search_paging_respects_oldest() {
        let mut p = Pager::new(RevRange::new(Some(10_000), Some(9_000), 50_000), true, true);
        let r = p.next_request().unwrap();
        assert_eq!(r, PageRequest { start: 10_000, end: 9_000, limit: 0 });
        p.complete(r, &[]);
        assert!(p.is_done());

        let mut p = Pager::new(RevRange::new(None, None, 50_000), true, true);
        p.set_oldest(49_500);
        let r = p.next_request().unwrap();
        assert_eq!(r.end, 49_500);
        p.set_oldest(60_000);
        assert!(p.is_done());
    }

    #[test]
    fn end_of_history_codes() {
        assert!(is_end_of_history(Some(195012)));
        assert!(is_end_of_history(Some(160013)));
        assert!(!is_end_of_history(Some(170001)));
        assert!(!is_end_of_history(None));
    }

    fn url(s: &str) -> RepoUrl {
        RepoUrl::parse(s).unwrap()
    }

    #[test]
    fn selection_to_diff() {
        let u = url("http://h/r/trunk");
        assert_eq!(revision_diff(&u, 50, &[]), None);
        assert_eq!(revision_diff(&u, 50, &[0]), None);
        assert_eq!(
            revision_diff(&u, 50, &[17]),
            Some(DiffSpec::ChangeAt { url: u.clone(), peg: 50, rev: 17 })
        );
        // The newest revision is its own peg.
        assert_eq!(
            revision_diff(&u, 50, &[50]),
            Some(DiffSpec::ChangeAt { url: u.clone(), peg: 50, rev: 50 })
        );
        let two = Some(DiffSpec::Range { url: u.clone(), peg: 50, from: 12, to: 40 });
        assert_eq!(revision_diff(&u, 50, &[40, 12]), two);
        assert_eq!(revision_diff(&u, 50, &[12, 40]), two);
        assert_eq!(revision_diff(&u, 50, &[12, 12]), None);
        assert_eq!(revision_diff(&u, 50, &[1, 2, 3]), None);
    }

    fn cp(action: ChangeAction, path: &str, kind: Option<NodeKind>) -> ChangedPath {
        ChangedPath {
            action,
            path: path.to_owned(),
            kind,
            copy_from: None,
            text_mods: true,
            prop_mods: false,
        }
    }

    #[test]
    fn changed_path_targets() {
        let root = url("http://h/r");
        let file = url("http://h/r/trunk/a%20b.c");
        let t = path_target(&root, 24, &cp(ChangeAction::Modified, "/trunk/a b.c", Some(NodeKind::File)));
        assert_eq!(
            t,
            PathTarget {
                url: file.clone(),
                rev: 24,
                diff: Some(DiffSpec::Change { url: file.clone(), rev: 24 }),
                viewable: true,
            }
        );
        let t = path_target(&root, 24, &cp(ChangeAction::Deleted, "/trunk/a b.c", Some(NodeKind::File)));
        assert_eq!(t.rev, 23);
        assert_eq!(
            t.diff,
            Some(DiffSpec::Range { url: file, peg: 23, from: 23, to: 24 })
        );
        assert!(t.viewable);
        let t = path_target(&root, 5, &cp(ChangeAction::Added, "/trunk/dir", Some(NodeKind::Dir)));
        assert!(!t.viewable);
        assert_eq!(t.rev, 5);
        let t = path_target(&root, 5, &cp(ChangeAction::Replaced, "/x", None));
        assert!(t.viewable);
        assert_eq!(t.rev, 5);
    }

    #[test]
    fn message_first_line() {
        assert_eq!(first_line(""), "");
        assert_eq!(first_line("Fix it"), "Fix it");
        assert_eq!(first_line("\n\n  Fix it  \nmore"), "Fix it");
        assert_eq!(first_line("a\r\nb"), "a");
    }
}
