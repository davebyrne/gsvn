//! The total diff size limit (DESIGN.md §7.3).
//!
//! Every received [`FileDiff`] stays in memory (only the selected one is
//! rendered), so a huge commit would grow without bound. Past
//! [`LIMIT_BYTES`] of diff text or [`LIMIT_LINES`] diff lines the window
//! stops the stream and asks before loading the rest. Loading the rest
//! re-runs svn and skips the files it already has ([`Resume`]).

use svn_backend::{DiffContent, FileDiff};

/// Diff text (approximate size of svn's output) after which loading pauses.
pub const LIMIT_BYTES: u64 = 5 * 1024 * 1024;
/// Diff lines (hunk lines, all files) after which loading pauses.
pub const LIMIT_LINES: u64 = 50_000;

/// `Index:` line plus the `====` separator line.
const FILE_HEADER: u64 = "Index: \n".len() as u64 + 68;

/// Approximate size of a file's section in svn's output: (bytes, lines).
pub fn text_size(f: &FileDiff) -> (u64, u64) {
    let len = |s: &str| s.len() as u64;
    let mut bytes = FILE_HEADER + len(&f.path);
    for label in [&f.old_label, &f.new_label].into_iter().flatten() {
        bytes += 5 + len(label);
    }
    let mut lines = 0;
    if let DiffContent::Text(hunks) = &f.content {
        for h in hunks {
            // `@@ -a,b +c,d @@ section`
            bytes += 32 + len(&h.section);
            for l in &h.lines {
                bytes += 2 + len(&l.text);
                if l.no_newline {
                    bytes += 30;
                }
            }
            lines += h.lines.len() as u64;
        }
    }
    for p in &f.prop_changes {
        bytes += 16 + len(&p.name);
        for v in [&p.old, &p.new].into_iter().flatten() {
            bytes += 4 + len(v);
            lines += 1 + v.matches('\n').count() as u64;
        }
    }
    (bytes, lines)
}

/// Running total of the received diff against the limit.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    limit_bytes: u64,
    limit_lines: u64,
    pub bytes: u64,
    pub lines: u64,
}

impl Budget {
    pub fn new(limit_bytes: u64, limit_lines: u64) -> Self {
        Self {
            limit_bytes,
            limit_lines,
            bytes: 0,
            lines: 0,
        }
    }

    /// No limit (the user chose to load everything).
    pub fn unlimited() -> Self {
        Self::new(u64::MAX, u64::MAX)
    }

    pub fn add(&mut self, f: &FileDiff) {
        let (bytes, lines) = text_size(f);
        self.bytes = self.bytes.saturating_add(bytes);
        self.lines = self.lines.saturating_add(lines);
    }

    /// The files added so far reached the limit: the next one should not
    /// be loaded. (A single file over the limit is still loaded; the
    /// large-file prompt guards its rendering.)
    pub fn exceeded(&self) -> bool {
        self.bytes >= self.limit_bytes || self.lines >= self.limit_lines
    }
}

/// What to do with a file received while re-running the diff after a pause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Incoming {
    /// Already in the list.
    Skip,
    /// New: append it.
    Keep,
    /// The output differs from the first run at this index: drop the
    /// stored files from here on, then append it.
    Diverged,
}

/// Matches the re-run's output against the files kept from the first run.
#[derive(Debug, Clone, Copy)]
pub struct Resume {
    /// Files kept from the first run (still to be matched).
    have: u32,
    /// Files received by this run.
    seen: u32,
}

impl Resume {
    pub fn new(have: u32) -> Self {
        Self { have, seen: 0 }
    }

    /// Classify the next received file; `same(i)` tells whether it is the
    /// stored file at index `i`.
    pub fn next(&mut self, same: impl FnOnce(u32) -> bool) -> Incoming {
        let index = self.seen;
        self.seen += 1;
        if index >= self.have {
            return Incoming::Keep;
        }
        if same(index) {
            Incoming::Skip
        } else {
            self.have = index;
            Incoming::Diverged
        }
    }

    /// Index from which stored files are dropped after [`Incoming::Diverged`].
    pub fn kept(&self) -> u32 {
        self.have
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use svn_backend::{DiffLine, DiffLineKind, Hunk, PropChange};

    fn file(path: &str, lines: &[&str]) -> FileDiff {
        FileDiff {
            path: path.to_owned(),
            old_label: Some(format!("{path}\t(revision 1)")),
            new_label: Some(format!("{path}\t(revision 2)")),
            content: DiffContent::Text(vec![Hunk {
                old_start: 1,
                old_len: 0,
                new_start: 1,
                new_len: lines.len() as u64,
                section: String::new(),
                lines: lines
                    .iter()
                    .enumerate()
                    .map(|(i, t)| DiffLine {
                        kind: DiffLineKind::Added,
                        old_no: None,
                        new_no: Some(i as u64 + 1),
                        text: (*t).to_owned(),
                        no_newline: false,
                    })
                    .collect(),
            }]),
            prop_changes: vec![],
        }
    }

    #[test]
    fn size_tracks_svn_output() {
        // What svn prints for this file:
        let out = "Index: a.c\n\
                   ===================================================================\n\
                   --- a.c\t(revision 1)\n\
                   +++ a.c\t(revision 2)\n\
                   @@ -1,0 +1,2 @@\n\
                   +hello\n\
                   +world\n";
        let (bytes, lines) = text_size(&file("a.c", &["hello", "world"]));
        assert_eq!(lines, 2);
        let real = out.len() as u64;
        assert!(bytes >= real && bytes < real + 32, "{bytes} vs {real}");

        let mut props = file("p", &[]);
        props.content = DiffContent::None;
        props.prop_changes = vec![PropChange {
            name: "svn:ignore".into(),
            old: None,
            new: Some("a\nb\nc".into()),
        }];
        assert_eq!(text_size(&props).1, 3);
        let mut binary = props.clone();
        binary.content = DiffContent::Binary;
        binary.prop_changes.clear();
        assert_eq!(text_size(&binary).1, 0);
    }

    #[test]
    fn budget_by_lines_and_bytes() {
        let f = file("f", &["x"; 10]);
        let mut b = Budget::new(u64::MAX, 25);
        assert!(!b.exceeded());
        b.add(&f);
        b.add(&f);
        assert!(!b.exceeded());
        b.add(&f);
        assert!(b.exceeded());
        assert_eq!(b.lines, 30);

        let mut b = Budget::new(text_size(&f).0 * 2, u64::MAX);
        b.add(&f);
        assert!(!b.exceeded());
        b.add(&f);
        assert!(b.exceeded());

        let mut b = Budget::unlimited();
        for _ in 0..1000 {
            b.add(&f);
        }
        assert!(!b.exceeded());

        // A single file over the limit is loaded, the next one isn't.
        let mut b = Budget::new(LIMIT_BYTES, LIMIT_LINES);
        assert!(!b.exceeded());
        b.add(&file("big", &["y"; LIMIT_LINES as usize + 1]));
        assert!(b.exceeded());
    }

    #[test]
    fn resume_skips_known_files() {
        let stored = ["a", "b", "c"];
        let at = |p: &'static str| move |i: u32| stored.get(i as usize) == Some(&p);
        let mut r = Resume::new(3);
        assert_eq!(r.next(at("a")), Incoming::Skip);
        assert_eq!(r.next(at("b")), Incoming::Skip);
        assert_eq!(r.next(at("c")), Incoming::Skip);
        assert_eq!(r.next(at("d")), Incoming::Keep);
        assert_eq!(r.next(at("e")), Incoming::Keep);
        assert_eq!(r.kept(), 3);
    }

    #[test]
    fn resume_detects_divergence() {
        let stored = ["a", "b", "c"];
        let at = |p: &'static str| move |i: u32| stored.get(i as usize) == Some(&p);
        let mut r = Resume::new(3);
        assert_eq!(r.next(at("a")), Incoming::Skip);
        assert_eq!(r.next(at("x")), Incoming::Diverged);
        assert_eq!(r.kept(), 1);
        // Everything after a divergence is new.
        assert_eq!(r.next(at("c")), Incoming::Keep);
        assert_eq!(r.next(at("d")), Incoming::Keep);

        let mut r = Resume::new(0);
        assert_eq!(r.next(|_| false), Incoming::Keep);
    }
}
