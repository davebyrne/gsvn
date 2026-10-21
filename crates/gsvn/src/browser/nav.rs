//! Pure navigation logic: history, breadcrumbs, location and revision
//! input parsing, natural sort keys. No GTK here, so it is unit-tested.

use std::cmp::Ordering;
use svn_backend::{RepoUrl, Rev, RevNum};

/// A place in the repository: a directory at a concrete revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub url: RepoUrl,
    pub rev: RevNum,
    /// The revision was resolved from HEAD; Refresh re-resolves it.
    pub follow_head: bool,
}

/// Back/forward history of a window.
#[derive(Debug, Default)]
pub struct History<T> {
    back: Vec<T>,
    current: Option<T>,
    forward: Vec<T>,
}

/// Maximum number of entries kept in each direction.
const HISTORY_LIMIT: usize = 100;

impl<T: Clone + PartialEq> History<T> {
    pub fn new() -> Self {
        Self {
            back: Vec::new(),
            current: None,
            forward: Vec::new(),
        }
    }

    #[cfg(test)]
    pub fn current(&self) -> Option<&T> {
        self.current.as_ref()
    }

    /// Visit a new place: the current one goes to the back stack and the
    /// forward stack is cleared. Visiting the current place again is a no-op.
    pub fn visit(&mut self, item: T) {
        if self.current.as_ref() == Some(&item) {
            return;
        }
        if let Some(cur) = self.current.replace(item) {
            self.back.push(cur);
            if self.back.len() > HISTORY_LIMIT {
                self.back.remove(0);
            }
        }
        self.forward.clear();
    }

    /// Replace the current place without touching the stacks (e.g. a
    /// location turned out to be a file and was redirected to its parent).
    pub fn replace(&mut self, item: T) {
        self.current = Some(item);
    }

    pub fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub fn can_go_forward(&self) -> bool {
        !self.forward.is_empty()
    }

    pub fn go_back(&mut self) -> Option<T> {
        let item = self.back.pop()?;
        if let Some(cur) = self.current.replace(item.clone()) {
            self.forward.push(cur);
        }
        Some(item)
    }

    pub fn go_forward(&mut self) -> Option<T> {
        let item = self.forward.pop()?;
        if let Some(cur) = self.current.replace(item.clone()) {
            self.back.push(cur);
        }
        Some(item)
    }
}

/// Breadcrumb segments from the repository root to `url`: `(label, url)`.
/// The first segment is the root, labelled with the repository name.
pub fn breadcrumbs(root: &RepoUrl, url: &RepoUrl) -> Vec<(String, RepoUrl)> {
    let mut out = Vec::new();
    let root_name = root.name();
    out.push((
        if root_name.is_empty() { "/".to_owned() } else { root_name },
        root.clone(),
    ));
    let Some(path) = url.path_from(root) else {
        return out;
    };
    let mut cur = root.clone();
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        cur = cur.join(seg);
        out.push((seg.to_owned(), cur.clone()));
    }
    out
}

/// Decoded path segments of `url` below `root` (empty for the root itself).
pub fn segments(root: &RepoUrl, url: &RepoUrl) -> Vec<String> {
    url.path_from(root)
        .map(|p| p.split('/').filter(|s| !s.is_empty()).map(str::to_owned).collect())
        .unwrap_or_default()
}

/// Whether `url` is `ancestor` or below it.
pub fn is_within(url: &RepoUrl, ancestor: &RepoUrl) -> bool {
    url.path_from(ancestor).is_some()
}

/// What the user typed into the path bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocationInput {
    pub url: RepoUrl,
    /// A peg revision given as `…@123` or `…@HEAD`.
    pub rev: Option<Rev>,
}

/// Parse path bar input: a full URL, or a path relative to the repository
/// root (`trunk/src`, `/trunk/src` or svn's `^/trunk/src`). A trailing
/// `@N` / `@HEAD` selects a revision; other `@` are part of the path.
pub fn parse_location(input: &str, root: &RepoUrl) -> Result<LocationInput, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("Enter a URL or a path".to_owned());
    }
    let (body, rev) = split_peg(input);
    let lower = body.to_ascii_lowercase();
    let url = if lower.starts_with("http://") || lower.starts_with("https://") {
        crate::common::parse_url_input(body)?
    } else if lower.contains("://") {
        return Err("Only http:// and https:// URLs are supported".to_owned());
    } else {
        let rel = body.strip_prefix('^').unwrap_or(body);
        root.join(rel)
    };
    Ok(LocationInput { url, rev })
}

fn split_peg(input: &str) -> (&str, Option<Rev>) {
    if let Some((body, peg)) = input.rsplit_once('@') {
        if peg.eq_ignore_ascii_case("head") {
            return (body, Some(Rev::Head));
        }
        if !peg.is_empty() && peg.bytes().all(|b| b.is_ascii_digit()) && let Ok(n) = peg.parse() {
            return (body, Some(Rev::Number(n)));
        }
    }
    (input, None)
}

/// Parse the revision popover input: `HEAD`, `123`, `r123`, `{2024-01-31}`
/// or a bare date `2024-01-31` (optionally with a time).
pub fn parse_rev_target(input: &str) -> Result<Rev, String> {
    let input = input.trim();
    if let Some(date) = input.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
        let date = date.trim();
        if date.is_empty() {
            return Err("Enter a date such as 2024-01-31".to_owned());
        }
        return Ok(Rev::Date(date.to_owned()));
    }
    if looks_like_date(input) {
        return Ok(Rev::Date(input.to_owned()));
    }
    match crate::common::parse_rev_input(input)? {
        Some(n) => Ok(Rev::Number(n)),
        None => Ok(Rev::Head),
    }
}

/// `YYYY-MM-DD` optionally followed by more (time).
fn looks_like_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 10
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[7] == b'-'
        && b[8..10].iter().all(u8::is_ascii_digit)
}

/// Text of the revision button.
pub fn rev_label(rev: RevNum, follow_head: bool) -> String {
    if follow_head {
        format!("HEAD r{rev}")
    } else {
        format!("r{rev}")
    }
}

/// A key for natural, case-insensitive ordering, comparable bytewise:
/// lower-cased, with each run of digits replaced by its length (3 digits)
/// and the digits without leading zeros, so `f2` < `f10`.
pub fn natural_key(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 8);
    let mut chars = name.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            let mut digits = String::new();
            digits.push(c);
            while let Some(&d) = chars.peek() {
                if !d.is_ascii_digit() {
                    break;
                }
                digits.push(d);
                chars.next();
            }
            let trimmed = digits.trim_start_matches('0');
            let trimmed = if trimmed.is_empty() { "0" } else { trimmed };
            // '0' sorts before letters, like plain digits do.
            out.push('0');
            out.push_str(&format!("{:03}", trimmed.len().min(999)));
            out.push_str(trimmed);
        } else {
            out.extend(c.to_lowercase());
        }
    }
    out
}

/// Natural order of two names with precomputed keys; ties broken by the
/// raw names so the order is total.
pub fn natural_cmp(a_key: &str, a_name: &str, b_key: &str, b_name: &str) -> Ordering {
    a_key.cmp(b_key).then_with(|| a_name.cmp(b_name))
}

/// Visual indentation of a tree row: `(indent levels, show depth badge)`.
/// Indentation stops growing after `cap` levels.
pub fn tree_indent(depth: u32, cap: u32) -> (u32, bool) {
    if depth > cap { (cap, true) } else { (depth, false) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> RepoUrl {
        RepoUrl::parse(s).unwrap()
    }

    #[test]
    fn history_back_forward() {
        let mut h = History::new();
        assert!(!h.can_go_back());
        h.visit(1);
        h.visit(2);
        h.visit(2);
        h.visit(3);
        assert_eq!(h.current(), Some(&3));
        assert_eq!(h.go_back(), Some(2));
        assert_eq!(h.go_back(), Some(1));
        assert_eq!(h.go_back(), None);
        assert_eq!(h.current(), Some(&1));
        assert!(h.can_go_forward());
        assert_eq!(h.go_forward(), Some(2));
        h.visit(4);
        assert!(!h.can_go_forward());
        assert_eq!(h.go_back(), Some(2));
        h.replace(5);
        assert_eq!(h.go_forward(), Some(4));
        assert_eq!(h.go_back(), Some(5));
    }

    #[test]
    fn history_is_bounded() {
        let mut h = History::new();
        for i in 0..250 {
            h.visit(i);
        }
        let mut n = 0;
        while h.go_back().is_some() {
            n += 1;
        }
        assert_eq!(n, HISTORY_LIMIT);
    }

    #[test]
    fn crumbs() {
        let root = u("http://h/svn/small");
        let c = breadcrumbs(&root, &root.join("trunk/doc dir"));
        let labels: Vec<_> = c.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, ["small", "trunk", "doc dir"]);
        assert_eq!(c[2].1.as_str(), "http://h/svn/small/trunk/doc%20dir");
        assert_eq!(breadcrumbs(&root, &root).len(), 1);
        assert_eq!(segments(&root, &root.join("a/b")), ["a", "b"]);
        assert!(segments(&root, &root).is_empty());
        assert!(is_within(&root.join("a"), &root));
        assert!(!is_within(&u("http://h/svn/smaller"), &root));
    }

    #[test]
    fn location_input() {
        let root = u("http://h/r");
        let p = |s| parse_location(s, &root);
        assert_eq!(p("trunk/a b").unwrap().url.as_str(), "http://h/r/trunk/a%20b");
        assert_eq!(p("/trunk/").unwrap().url.as_str(), "http://h/r/trunk");
        assert_eq!(p("^/trunk").unwrap().url.as_str(), "http://h/r/trunk");
        assert_eq!(p("/").unwrap().url, root);
        let full = p("https://other/x/y@42").unwrap();
        assert_eq!(full.url.as_str(), "https://other/x/y");
        assert_eq!(full.rev, Some(Rev::Number(42)));
        assert_eq!(p("trunk@HEAD").unwrap().rev, Some(Rev::Head));
        let at = p("trunk/assets/icon@2x.txt").unwrap();
        assert_eq!(at.url.as_str(), "http://h/r/trunk/assets/icon@2x.txt");
        assert_eq!(at.rev, None);
        assert!(p("").is_err());
        assert!(p("svn://h/r").is_err());
    }

    #[test]
    fn rev_targets() {
        assert_eq!(parse_rev_target("HEAD"), Ok(Rev::Head));
        assert_eq!(parse_rev_target(""), Ok(Rev::Head));
        assert_eq!(parse_rev_target("r12"), Ok(Rev::Number(12)));
        assert_eq!(parse_rev_target("{2024-01-31}"), Ok(Rev::Date("2024-01-31".into())));
        assert_eq!(
            parse_rev_target("2024-01-31T12:00"),
            Ok(Rev::Date("2024-01-31T12:00".into()))
        );
        assert!(parse_rev_target("{}").is_err());
        assert!(parse_rev_target("yesterday").is_err());
        assert_eq!(rev_label(5, true), "HEAD r5");
        assert_eq!(rev_label(5, false), "r5");
    }

    #[test]
    fn natural_order() {
        let mut names = vec!["f10.txt", "F2.txt", "f1.txt", "a", "B", "f02.txt", "f010"];
        names.sort_by(|a, b| natural_cmp(&natural_key(a), a, &natural_key(b), b));
        assert_eq!(names, ["a", "B", "f1.txt", "F2.txt", "f02.txt", "f010", "f10.txt"]);
        assert!(natural_key("x9") < natural_key("x10"));
        assert!(natural_key("x") < natural_key("x0"));
        assert_eq!(natural_key("ÄB"), "äb");
    }

    #[test]
    fn indent_cap() {
        assert_eq!(tree_indent(3, 12), (3, false));
        assert_eq!(tree_indent(12, 12), (12, false));
        assert_eq!(tree_indent(199, 12), (12, true));
    }
}
