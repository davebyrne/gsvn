//! Pure render models for the diff window: per-file summaries for the
//! sidebar, and the text + per-line metadata shown in the unified and the
//! side-by-side views. No GTK here, so it is unit-tested.

use super::intra::intra_line;
use similar::{Algorithm, DiffTag};
use std::borrow::Cow;
use svn_backend::{DiffContent, DiffLineKind, FileDiff, Hunk, PropChange};

/// Files whose rendering would exceed this many rows need confirmation.
pub const LARGE_ROWS: usize = 50_000;

/// Text shown for svn's `\ No newline at end of file` marker.
pub const NO_NEWLINE: &str = "\\ No newline at end of file";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Deleted,
    Modified,
    Binary,
    /// Only properties changed.
    PropsOnly,
    /// No content and no properties (e.g. an empty file added or deleted
    /// in a way svn doesn't label).
    Empty,
}

impl FileStatus {
    pub fn letter(self) -> &'static str {
        match self {
            FileStatus::Added => "A",
            FileStatus::Deleted => "D",
            FileStatus::Modified => "M",
            FileStatus::Binary => "B",
            FileStatus::PropsOnly => "P",
            FileStatus::Empty => "E",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            FileStatus::Added => "Added",
            FileStatus::Deleted => "Deleted",
            FileStatus::Modified => "Modified",
            FileStatus::Binary => "Binary file changed",
            FileStatus::PropsOnly => "Properties changed",
            FileStatus::Empty => "No content changes",
        }
    }
}

/// What the sidebar shows for a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileSummary {
    pub status: FileStatus,
    pub added: usize,
    pub removed: usize,
    pub props: usize,
}

fn nonexistent(label: Option<&str>) -> bool {
    label.is_some_and(|l| l.trim_end().ends_with("(nonexistent)"))
}

pub fn summarize(f: &FileDiff) -> FileSummary {
    let old_missing = nonexistent(f.old_label.as_deref());
    let new_missing = nonexistent(f.new_label.as_deref());
    let (mut added, mut removed) = (0, 0);
    let status = match &f.content {
        DiffContent::Binary => FileStatus::Binary,
        DiffContent::Text(hunks) => {
            for line in hunks.iter().flat_map(|h| &h.lines) {
                match line.kind {
                    DiffLineKind::Added => added += 1,
                    DiffLineKind::Removed => removed += 1,
                    DiffLineKind::Context => {}
                }
            }
            if old_missing
                || (!hunks.is_empty() && hunks.iter().all(|h| h.old_start == 0 && h.old_len == 0))
            {
                FileStatus::Added
            } else if new_missing
                || (!hunks.is_empty() && hunks.iter().all(|h| h.new_start == 0 && h.new_len == 0))
            {
                FileStatus::Deleted
            } else {
                FileStatus::Modified
            }
        }
        DiffContent::None if old_missing => FileStatus::Added,
        DiffContent::None if new_missing => FileStatus::Deleted,
        DiffContent::None if !f.prop_changes.is_empty() => FileStatus::PropsOnly,
        DiffContent::None => FileStatus::Empty,
    };
    FileSummary {
        status,
        added,
        removed,
        props: f.prop_changes.len(),
    }
}

/// Upper bound of the rows a file renders to (both views), cheap to compute.
pub fn estimate_rows(f: &FileDiff) -> usize {
    let text = match &f.content {
        DiffContent::Text(hunks) => hunks
            .iter()
            .map(|h| 1 + h.lines.len() + h.lines.iter().filter(|l| l.no_newline).count())
            .sum(),
        _ => 0,
    };
    let props: usize = f
        .prop_changes
        .iter()
        .map(|p| value_lines(p.old.as_deref()).len() + value_lines(p.new.as_deref()).len() + 1)
        .sum();
    text + props + usize::from(!f.prop_changes.is_empty())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Context,
    Added,
    Removed,
    /// Hunk header, `@@ -a,b +c,d @@ section`
    Header,
    /// Side-by-side padding where the other side has more lines.
    Filler,
    /// `\ No newline at end of file`
    Meta,
    /// "Property changes" heading.
    Section,
    /// `Modified: svn:keywords`
    PropName,
}

/// Per buffer line metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    pub kind: RowKind,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
}

/// An intra-line highlight: chars `start..end` of buffer line `row`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub row: u32,
    pub start: u32,
    pub end: u32,
}

/// Text for one `GtkTextBuffer` plus metadata for each of its lines.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rendered {
    /// Lines joined with `\n` (no trailing newline), so the buffer has
    /// exactly `rows.len()` lines (one empty line when there are no rows).
    pub text: String,
    pub rows: Vec<Row>,
    pub spans: Vec<Span>,
}

impl Rendered {
    fn push(&mut self, kind: RowKind, old_no: Option<u32>, new_no: Option<u32>, text: &str) -> u32 {
        if !self.rows.is_empty() {
            self.text.push('\n');
        }
        self.text.push_str(text);
        self.rows.push(Row { kind, old_no, new_no });
        (self.rows.len() - 1) as u32
    }

    fn span(&mut self, row: u32, ranges: Vec<std::ops::Range<u32>>) {
        self.spans
            .extend(ranges.into_iter().map(|r| Span { row, start: r.start, end: r.end }));
    }

    /// Text of buffer line `i` (tests and debugging).
    pub fn line(&self, i: usize) -> &str {
        self.text.split('\n').nth(i).unwrap_or("")
    }

    /// Largest old and new line numbers (gutter widths).
    pub fn max_numbers(&self) -> (u32, u32) {
        self.rows.iter().fold((0, 0), |(o, n), r| {
            (o.max(r.old_no.unwrap_or(0)), n.max(r.new_no.unwrap_or(0)))
        })
    }
}

/// Side-by-side model: `left.rows.len() == right.rows.len()` always.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Split {
    pub left: Rendered,
    pub right: Rendered,
}

/// Replace characters GtkTextBuffer treats as line breaks (or can't hold)
/// with visible symbols, keeping char offsets unchanged.
pub fn sanitize(s: &str) -> Cow<'_, str> {
    if !s.contains(['\r', '\0', '\u{2028}', '\u{2029}', '\n']) {
        return Cow::Borrowed(s);
    }
    Cow::Owned(
        s.chars()
            .map(|c| match c {
                '\r' => '\u{240D}',
                '\n' => '\u{2424}',
                '\0' => '\u{2400}',
                '\u{2028}' => '\u{2424}',
                '\u{2029}' => '\u{00B6}',
                c => c,
            })
            .collect(),
    )
}

fn no(n: Option<u64>) -> Option<u32> {
    n.map(|n| u32::try_from(n).unwrap_or(u32::MAX))
}

fn range(start: u64, len: u64) -> String {
    if len == 1 {
        start.to_string()
    } else {
        format!("{start},{len}")
    }
}

pub fn hunk_header(h: &Hunk) -> String {
    let mut s = format!(
        "@@ -{} +{} @@",
        range(h.old_start, h.old_len),
        range(h.new_start, h.new_len)
    );
    if !h.section.is_empty() {
        s.push(' ');
        s.push_str(&h.section);
    }
    sanitize(&s).into_owned()
}

/// Lines of a property value; one trailing newline doesn't make an extra line.
fn value_lines(v: Option<&str>) -> Vec<&str> {
    match v {
        None => Vec::new(),
        Some(v) => v.strip_suffix('\n').unwrap_or(v).split('\n').collect(),
    }
}

fn prop_title(p: &PropChange) -> String {
    let action = match (&p.old, &p.new) {
        (None, Some(_)) => "Added",
        (Some(_), None) => "Deleted",
        _ => "Modified",
    };
    sanitize(&format!("{action}: {}", p.name)).into_owned()
}

/// One side of a change block (a run of removed/added lines between
/// context lines).
#[derive(Debug)]
struct Item {
    kind: RowKind,
    no: Option<u32>,
    text: String,
}

/// Receives the parsed diff in order; implemented by the two layouts.
trait Sink {
    fn hunk(&mut self, header: &str);
    fn context(&mut self, old_no: Option<u32>, new_no: Option<u32>, text: &str);
    /// A full change block: removed and added items (lines or `Meta`).
    fn change(&mut self, removed: Vec<Item>, added: Vec<Item>);
    /// `Meta` row on both sides (no newline after a context line).
    fn meta_both(&mut self);
    fn heading(&mut self, kind: RowKind, text: &str);
}

/// Walk `f` and feed `sink`.
fn walk(f: &FileDiff, sink: &mut impl Sink) {
    if let DiffContent::Text(hunks) = &f.content {
        for h in hunks {
            sink.hunk(&hunk_header(h));
            let mut removed = Vec::new();
            let mut added = Vec::new();
            for line in &h.lines {
                let text = sanitize(&line.text).into_owned();
                match line.kind {
                    DiffLineKind::Context => {
                        if !removed.is_empty() || !added.is_empty() {
                            sink.change(std::mem::take(&mut removed), std::mem::take(&mut added));
                        }
                        sink.context(no(line.old_no), no(line.new_no), &text);
                        if line.no_newline {
                            sink.meta_both();
                        }
                        continue;
                    }
                    DiffLineKind::Removed => {
                        removed.push(Item { kind: RowKind::Removed, no: no(line.old_no), text });
                        if line.no_newline {
                            removed.push(meta());
                        }
                    }
                    DiffLineKind::Added => {
                        added.push(Item { kind: RowKind::Added, no: no(line.new_no), text });
                        if line.no_newline {
                            added.push(meta());
                        }
                    }
                }
            }
            if !removed.is_empty() || !added.is_empty() {
                sink.change(removed, added);
            }
        }
    }

    if f.prop_changes.is_empty() {
        return;
    }
    sink.heading(RowKind::Section, "Property changes");
    for p in &f.prop_changes {
        sink.heading(RowKind::PropName, &prop_title(p));
        let old: Vec<String> = value_lines(p.old.as_deref()).into_iter().map(|l| sanitize(l).into_owned()).collect();
        let new: Vec<String> = value_lines(p.new.as_deref()).into_iter().map(|l| sanitize(l).into_owned()).collect();
        let item = |kind, text: &String| Item { kind, no: None, text: text.clone() };
        for op in similar::capture_diff_slices(Algorithm::Myers, &old, &new) {
            let (tag, o, n) = op.as_tag_tuple();
            if tag == DiffTag::Equal {
                for line in &old[o] {
                    sink.context(None, None, line);
                }
            } else {
                sink.change(
                    old[o].iter().map(|t| item(RowKind::Removed, t)).collect(),
                    new[n].iter().map(|t| item(RowKind::Added, t)).collect(),
                );
            }
        }
    }
}

fn meta() -> Item {
    Item { kind: RowKind::Meta, no: None, text: NO_NEWLINE.to_owned() }
}

/// Lines (not `Meta` rows) of a change block, for pairing.
fn lines(items: &[Item]) -> impl Iterator<Item = (usize, &Item)> {
    items.iter().enumerate().filter(|(_, i)| i.kind != RowKind::Meta)
}

struct UnifiedSink(Rendered);

impl Sink for UnifiedSink {
    fn hunk(&mut self, header: &str) {
        self.0.push(RowKind::Header, None, None, header);
    }

    fn context(&mut self, old_no: Option<u32>, new_no: Option<u32>, text: &str) {
        self.0.push(RowKind::Context, old_no, new_no, text);
    }

    fn change(&mut self, removed: Vec<Item>, added: Vec<Item>) {
        let r = &mut self.0;
        let removed_rows: Vec<u32> = removed
            .iter()
            .map(|i| r.push(i.kind, i.no.filter(|_| i.kind == RowKind::Removed), None, &i.text))
            .collect();
        let added_rows: Vec<u32> = added
            .iter()
            .map(|i| r.push(i.kind, None, i.no.filter(|_| i.kind == RowKind::Added), &i.text))
            .collect();
        // Pair the k-th removed line with the k-th added line.
        for ((ri, old), (ai, new)) in lines(&removed).zip(lines(&added)) {
            if let Some((o, n)) = intra_line(&old.text, &new.text) {
                r.span(removed_rows[ri], o);
                r.span(added_rows[ai], n);
            }
        }
    }

    fn meta_both(&mut self) {
        self.0.push(RowKind::Meta, None, None, NO_NEWLINE);
    }

    fn heading(&mut self, kind: RowKind, text: &str) {
        self.0.push(kind, None, None, text);
    }
}

struct SplitSink(Split);

impl Sink for SplitSink {
    fn hunk(&mut self, header: &str) {
        self.heading(RowKind::Header, header);
    }

    fn context(&mut self, old_no: Option<u32>, new_no: Option<u32>, text: &str) {
        self.0.left.push(RowKind::Context, old_no, None, text);
        self.0.right.push(RowKind::Context, None, new_no, text);
    }

    fn change(&mut self, removed: Vec<Item>, added: Vec<Item>) {
        // Pair lines row by row; a `Meta` row pairs with whatever is
        // opposite. The shorter side is padded with fillers.
        let Split { left, right } = &mut self.0;
        let rows = removed.len().max(added.len());
        for k in 0..rows {
            let l = removed.get(k);
            let a = added.get(k);
            let lrow = match l {
                Some(i) => left.push(i.kind, i.no.filter(|_| i.kind == RowKind::Removed), None, &i.text),
                None => left.push(RowKind::Filler, None, None, ""),
            };
            let rrow = match a {
                Some(i) => right.push(i.kind, None, i.no.filter(|_| i.kind == RowKind::Added), &i.text),
                None => right.push(RowKind::Filler, None, None, ""),
            };
            if let (Some(l), Some(a)) = (l, a)
                && l.kind == RowKind::Removed
                && a.kind == RowKind::Added
                && let Some((o, n)) = intra_line(&l.text, &a.text)
            {
                left.span(lrow, o);
                right.span(rrow, n);
            }
        }
    }

    fn meta_both(&mut self) {
        self.heading(RowKind::Meta, NO_NEWLINE);
    }

    fn heading(&mut self, kind: RowKind, text: &str) {
        self.0.left.push(kind, None, None, text);
        self.0.right.push(kind, None, None, text);
    }
}

/// Unified model: hunk headers, context, removed and added lines in diff
/// order with both line numbers, then the property changes.
pub fn unified(f: &FileDiff) -> Rendered {
    let mut sink = UnifiedSink(Rendered::default());
    walk(f, &mut sink);
    sink.0
}

/// Side-by-side model: old on the left, new on the right, rows aligned.
pub fn split(f: &FileDiff) -> Split {
    let mut sink = SplitSink(Split::default());
    walk(f, &mut sink);
    sink.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use svn_backend::{DiffLine, parse};
    use RowKind::*;

    fn fixture(name: &str) -> Vec<FileDiff> {
        let path = format!(
            "{}/../svn-backend/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        let data = std::fs::read(&path).unwrap();
        let mut files = Vec::new();
        parse::diff(&data[..], |f| files.push(f)).unwrap();
        files
    }

    fn parse_str(s: &str) -> Vec<FileDiff> {
        let mut files = Vec::new();
        parse::diff(s.as_bytes(), |f| files.push(f)).unwrap();
        files
    }

    fn kinds(r: &Rendered) -> Vec<RowKind> {
        r.rows.iter().map(|r| r.kind).collect()
    }

    fn line(kind: DiffLineKind, old_no: Option<u64>, new_no: Option<u64>, text: &str) -> DiffLine {
        DiffLine { kind, old_no, new_no, text: text.into(), no_newline: false }
    }

    fn file(content: DiffContent) -> FileDiff {
        FileDiff {
            path: "a.c".into(),
            old_label: Some("a.c\t(revision 1)".into()),
            new_label: Some("a.c\t(revision 2)".into()),
            content,
            prop_changes: Vec::new(),
        }
    }

    const MULTI: &str = "\
Index: src/main.c
===================================================================
--- src/main.c\t(revision 10)
+++ src/main.c\t(revision 11)
@@ -1,5 +1,6 @@ int main
 #include <stdio.h>
-int x = 1;
-int y = 2;
+int x = 10;
+int y = 2;
+int z = 3;

 int f(void)
@@ -20,3 +21,2 @@ int g(void)
 a
-b
-c
+c2
";

    #[test]
    fn unified_rows_and_numbers() {
        let f = &parse_str(MULTI)[0];
        let r = unified(f);
        assert_eq!(
            kinds(&r),
            [Header, Context, Removed, Removed, Added, Added, Added, Context, Context, Header, Context, Removed, Removed, Added]
        );
        assert_eq!(r.line(0), "@@ -1,5 +1,6 @@ int main");
        assert_eq!(r.line(2), "int x = 1;");
        assert_eq!(r.line(9), "@@ -20,3 +21,2 @@ int g(void)");
        assert_eq!(r.text.lines().count(), r.rows.len());
        let nos: Vec<(Option<u32>, Option<u32>)> = r.rows.iter().map(|r| (r.old_no, r.new_no)).collect();
        assert_eq!(
            nos,
            [
                (None, None),
                (Some(1), Some(1)),
                (Some(2), None),
                (Some(3), None),
                (None, Some(2)),
                (None, Some(3)),
                (None, Some(4)),
                (Some(4), Some(5)),
                (Some(5), Some(6)),
                (None, None),
                (Some(20), Some(21)),
                (Some(21), None),
                (Some(22), None),
                (None, Some(22)),
            ]
        );
        assert_eq!(r.max_numbers(), (22, 22));
        // Intra-line: "1" → "10" on rows 2/4; "int y = 2;" pairs with an
        // identical line and "b" with "c2" (nothing in common): no spans.
        assert_eq!(
            r.spans,
            [Span { row: 2, start: 8, end: 9 }, Span { row: 4, start: 8, end: 10 }]
        );
    }

    #[test]
    fn split_alignment() {
        let f = &parse_str(MULTI)[0];
        let s = split(f);
        assert_eq!(s.left.rows.len(), s.right.rows.len());
        assert_eq!(s.left.text.lines().count(), s.left.rows.len());
        assert_eq!(
            kinds(&s.left),
            [Header, Context, Removed, Removed, Filler, Context, Context, Header, Context, Removed, Removed]
        );
        assert_eq!(
            kinds(&s.right),
            [Header, Context, Added, Added, Added, Context, Context, Header, Context, Added, Filler]
        );
        assert_eq!(s.left.line(2), "int x = 1;");
        assert_eq!(s.right.line(2), "int x = 10;");
        assert_eq!(s.left.line(4), "");
        assert_eq!(s.right.line(4), "int z = 3;");
        assert_eq!(s.left.line(9), "b");
        assert_eq!(s.right.line(9), "c2");
        assert_eq!(s.left.line(10), "c");
        // Left only has old numbers, right only new numbers.
        assert!(s.left.rows.iter().all(|r| r.new_no.is_none()));
        assert!(s.right.rows.iter().all(|r| r.old_no.is_none()));
        let left: Vec<Option<u32>> = s.left.rows.iter().map(|r| r.old_no).collect();
        let right: Vec<Option<u32>> = s.right.rows.iter().map(|r| r.new_no).collect();
        assert_eq!(left, [None, Some(1), Some(2), Some(3), None, Some(4), Some(5), None, Some(20), Some(21), Some(22)]);
        assert_eq!(right, [None, Some(1), Some(2), Some(3), Some(4), Some(5), Some(6), None, Some(21), Some(22), None]);
        // "1"→"10" paired row 2; "b"/"c2" are too different → no spans.
        assert_eq!(s.left.spans, [Span { row: 2, start: 8, end: 9 }]);
        assert_eq!(s.right.spans, [Span { row: 2, start: 8, end: 10 }]);
    }

    #[test]
    fn no_newline_markers() {
        let f = &parse_str(
            "\
Index: x
===================================================================
--- x\t(revision 1)
+++ x\t(revision 2)
@@ -1,2 +1,2 @@
 a
-old last
\\ No newline at end of file
+new last
",
        )[0];
        let u = unified(f);
        assert_eq!(kinds(&u), [Header, Context, Removed, Meta, Added]);
        assert_eq!(u.line(3), NO_NEWLINE);
        assert_eq!(u.rows[3].old_no, None);
        assert_eq!(u.spans.len(), 2, "old last/new last still paired: {:?}", u.spans);

        let s = split(f);
        assert_eq!(kinds(&s.left), [Header, Context, Removed, Meta]);
        assert_eq!(kinds(&s.right), [Header, Context, Added, Filler]);
        assert_eq!(s.left.spans.len(), 1);
        assert_eq!(s.right.spans.len(), 1);
    }

    #[test]
    fn context_without_newline() {
        let mut lines = vec![line(DiffLineKind::Removed, Some(1), None, "x")];
        let mut ctx = line(DiffLineKind::Context, Some(2), Some(1), "end");
        ctx.no_newline = true;
        lines.push(ctx);
        let f = file(DiffContent::Text(vec![Hunk {
            old_start: 1,
            old_len: 2,
            new_start: 1,
            new_len: 1,
            section: String::new(),
            lines,
        }]));
        assert_eq!(kinds(&unified(&f)), [Header, Removed, Context, Meta]);
        let s = split(&f);
        assert_eq!(kinds(&s.left), [Header, Removed, Context, Meta]);
        assert_eq!(kinds(&s.right), [Header, Filler, Context, Meta]);
    }

    #[test]
    fn sanitizes_line_breaks() {
        assert_eq!(sanitize("plain"), Cow::Borrowed("plain"));
        assert_eq!(sanitize("a\rb\0c\u{2029}d\u{2028}"), "a\u{240D}b\u{2400}c\u{00B6}d\u{2424}");
        let f = file(DiffContent::Text(vec![Hunk {
            old_start: 1,
            old_len: 1,
            new_start: 1,
            new_len: 1,
            section: "sec\rtion".into(),
            lines: vec![
                line(DiffLineKind::Removed, Some(1), None, "one\rtwo"),
                line(DiffLineKind::Added, None, Some(1), "one\rthree"),
            ],
        }]));
        let u = unified(&f);
        assert_eq!(u.text.split('\n').count(), u.rows.len());
        assert!(!u.text.contains('\r'));
        assert_eq!(u.line(0), "@@ -1 +1 @@ sec\u{240D}tion");
    }

    #[test]
    fn summaries_from_fixtures() {
        let files = fixture("local_diff_r6.diff");
        let by_path = |p: &str| summarize(files.iter().find(|f| f.path == p).unwrap());
        let s = by_path("trunk/a b.txt");
        assert_eq!((s.status, s.added, s.removed), (FileStatus::Deleted, 0, 1));
        let s = by_path("trunk/amp&co.txt");
        assert_eq!((s.status, s.added, s.removed), (FileStatus::Modified, 1, 1));
        let s = by_path("trunk/hello.txt");
        assert_eq!((s.status, s.props), (FileStatus::PropsOnly, 2));
        let s = by_path("trunk/new-empty.txt");
        assert_eq!(s.status, FileStatus::Empty);
    }

    #[test]
    fn summary_status_inference() {
        let hunk = |os, ol, ns, nl| Hunk {
            old_start: os,
            old_len: ol,
            new_start: ns,
            new_len: nl,
            section: String::new(),
            lines: vec![],
        };
        // Added / deleted from hunk ranges when labels don't say.
        let mut f = file(DiffContent::Text(vec![hunk(0, 0, 1, 3)]));
        assert_eq!(summarize(&f).status, FileStatus::Added);
        f.content = DiffContent::Text(vec![hunk(1, 3, 0, 0)]);
        assert_eq!(summarize(&f).status, FileStatus::Deleted);
        f.content = DiffContent::Text(vec![hunk(1, 3, 1, 4)]);
        assert_eq!(summarize(&f).status, FileStatus::Modified);
        // ... and from labels.
        f.old_label = Some("a.c\t(nonexistent)".into());
        assert_eq!(summarize(&f).status, FileStatus::Added);
        f.content = DiffContent::None;
        assert_eq!(summarize(&f).status, FileStatus::Added);
        f.old_label = Some("a.c\t(revision 1)".into());
        f.new_label = Some("a.c\t(nonexistent)".into());
        assert_eq!(summarize(&f).status, FileStatus::Deleted);
        f.content = DiffContent::Binary;
        assert_eq!(summarize(&f).status, FileStatus::Binary);
    }

    #[test]
    fn props_section() {
        let files = fixture("local_diff_r6.diff");
        let f = files.iter().find(|f| f.path == "trunk/hello.txt").unwrap();
        let u = unified(f);
        assert_eq!(kinds(&u), [Section, PropName, Removed, Removed, PropName, Removed, Added]);
        assert_eq!(u.line(1), "Deleted: my:multi");
        assert_eq!(u.line(2), "line one");
        assert_eq!(u.line(4), "Modified: my:prop");
        assert_eq!(u.line(5), "single value");
        assert_eq!(u.line(6), "changed value");
        assert!(u.rows.iter().all(|r| r.old_no.is_none() && r.new_no.is_none()));
        assert_eq!(u.spans, [Span { row: 5, start: 0, end: 6 }, Span { row: 6, start: 0, end: 7 }]);

        let s = split(f);
        assert_eq!(kinds(&s.left), [Section, PropName, Removed, Removed, PropName, Removed]);
        assert_eq!(kinds(&s.right), [Section, PropName, Filler, Filler, PropName, Added]);
        assert_eq!(estimate_rows(f) >= u.rows.len(), true);
    }

    #[test]
    fn mergeinfo_value_diff() {
        // Multi-line value: unchanged lines become context rows.
        let mut f = file(DiffContent::None);
        f.prop_changes.push(PropChange {
            name: "svn:mergeinfo".into(),
            old: Some("/branches/a:1-5\n/branches/b:3\n".into()),
            new: Some("/branches/a:1-5\n/branches/b:3-9\n/branches/c:7\n".into()),
        });
        let u = unified(&f);
        assert_eq!(kinds(&u), [Section, PropName, Context, Removed, Added, Added]);
        assert_eq!(u.line(1), "Modified: svn:mergeinfo");
        assert_eq!(u.line(2), "/branches/a:1-5");
        let s = split(&f);
        assert_eq!(kinds(&s.left), [Section, PropName, Context, Removed, Filler]);
        assert_eq!(kinds(&s.right), [Section, PropName, Context, Added, Added]);
        assert_eq!(s.right.line(3), "/branches/b:3-9");
    }

    #[test]
    fn text_and_props_together() {
        let files = fixture("local_diff_r2.diff");
        for f in &files {
            let u = unified(f);
            let s = split(f);
            assert_eq!(u.text.split('\n').count(), u.rows.len().max(1), "{}", f.path);
            assert_eq!(s.left.rows.len(), s.right.rows.len(), "{}", f.path);
            assert!(estimate_rows(f) >= u.rows.len(), "{}", f.path);
            assert!(estimate_rows(f) >= s.left.rows.len(), "{}", f.path);
            let sum = summarize(f);
            let added = u.rows.iter().filter(|r| r.kind == Added && r.new_no.is_some()).count();
            let removed = u.rows.iter().filter(|r| r.kind == Removed && r.old_no.is_some()).count();
            assert_eq!((sum.added, sum.removed), (added, removed), "{}", f.path);
        }
    }

    /// Every fixture: invariants that the views rely on.
    #[test]
    fn invariants_on_all_fixtures() {
        let dir = format!("{}/../svn-backend/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
        let mut checked = 0;
        for entry in std::fs::read_dir(dir).unwrap() {
            let name = entry.unwrap().file_name().into_string().unwrap();
            if !name.ends_with(".diff") {
                continue;
            }
            for f in fixture(&name) {
                let u = unified(&f);
                let s = split(&f);
                for r in [&u, &s.left, &s.right] {
                    let n = if r.rows.is_empty() { 1 } else { r.rows.len() };
                    assert_eq!(r.text.split('\n').count(), n, "{name} {}", f.path);
                    for span in &r.spans {
                        let len = r.line(span.row as usize).chars().count() as u32;
                        assert!(span.start < span.end && span.end <= len, "{name} {span:?}");
                        assert!(matches!(r.rows[span.row as usize].kind, Added | Removed));
                    }
                }
                assert_eq!(s.left.rows.len(), s.right.rows.len(), "{name} {}", f.path);
                for (l, r) in s.left.rows.iter().zip(&s.right.rows) {
                    // Headings and context rows line up on both sides.
                    if matches!(l.kind, Header | Section | PropName | Context) {
                        assert_eq!(l.kind, r.kind, "{name} {}", f.path);
                    }
                    assert!(!matches!(l.kind, Added), "{name}");
                    assert!(!matches!(r.kind, Removed), "{name}");
                }
                assert!(estimate_rows(&f) >= u.rows.len(), "{name} {}", f.path);
                assert!(estimate_rows(&f) >= s.left.rows.len(), "{name} {}", f.path);
                checked += 1;
            }
        }
        assert!(checked > 20, "only {checked} files checked");
    }
}
