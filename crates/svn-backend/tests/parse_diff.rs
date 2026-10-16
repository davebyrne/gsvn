mod common;

use DiffLineKind::*;
use common::*;
use svn_backend::parse;
use svn_backend::*;

fn diff_bytes(b: &[u8]) -> Vec<FileDiff> {
    let mut v = Vec::new();
    parse::diff(b, |f| v.push(f)).unwrap();
    v
}

fn diff(name: &str) -> Vec<FileDiff> {
    let mut v = Vec::new();
    parse::diff(fixture(name), |f| v.push(f)).unwrap();
    v
}

fn l(kind: DiffLineKind, old: Option<u64>, new: Option<u64>, text: &str) -> DiffLine {
    DiffLine {
        kind,
        old_no: old,
        new_no: new,
        text: text.into(),
        no_newline: false,
    }
}
fn ctx(o: u64, n: u64, t: &str) -> DiffLine {
    l(Context, Some(o), Some(n), t)
}
fn del(o: u64, t: &str) -> DiffLine {
    l(Removed, Some(o), None, t)
}
fn add(n: u64, t: &str) -> DiffLine {
    l(Added, None, Some(n), t)
}
fn nonl(mut d: DiffLine) -> DiffLine {
    d.no_newline = true;
    d
}

fn hunks(f: &FileDiff) -> &[Hunk] {
    match &f.content {
        DiffContent::Text(h) => h,
        c => panic!("{}: expected text, got {c:?}", f.path),
    }
}

fn prop(name: &str, old: Option<&str>, new: Option<&str>) -> PropChange {
    PropChange {
        name: name.into(),
        old: old.map(Into::into),
        new: new.map(Into::into),
    }
}

#[test]
fn diff_r2_adds() {
    let v = diff("local_diff_r2.diff");
    let paths: Vec<&str> = v.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "trunk/a b.txt",
            "trunk/amp&co.txt",
            "trunk/at@sign.txt",
            "trunk/binary.bin",
            "trunk/crlf.txt",
            "trunk/dashes.txt",
            "trunk/empty.txt",
            "trunk/hello.txt",
            "trunk/latin1.txt",
            "trunk/noeol.txt",
            "trunk/sub dir/nested.txt",
            "trunk/unicodé-日本.txt",
        ]
    );
    let f = &v[0];
    assert_eq!(f.old_label.as_deref(), Some("trunk/a b.txt\t(nonexistent)"));
    assert_eq!(f.new_label.as_deref(), Some("trunk/a b.txt\t(revision 2)"));
    assert_eq!(
        hunks(f),
        [Hunk {
            old_start: 0,
            old_len: 0,
            new_start: 1,
            new_len: 1,
            section: String::new(),
            lines: vec![add(1, "spaces")],
        }]
    );
    assert!(f.prop_changes.is_empty());

    // Binary add with svn:mime-type property.
    let b = &v[3];
    assert_eq!(b.content, DiffContent::Binary);
    assert_eq!(b.old_label, None);
    assert_eq!(
        b.prop_changes,
        [prop(
            "svn:mime-type",
            None,
            Some("application/octet-stream")
        )]
    );

    // CRLF: the \r is stripped from the text.
    assert_eq!(
        hunks(&v[4])[0].lines,
        [add(1, "one"), add(2, "two"), add(3, "three")]
    );

    // Lines looking like headers inside an added file.
    assert_eq!(
        hunks(&v[5])[0].lines,
        [
            add(1, "keep"),
            add(2, "-- old dashes"),
            add(3, "middle"),
            add(4, "++ old plus"),
            add(5, "Index: not a header"),
            add(6, "@@ -1 +1 @@ not a hunk"),
            add(7, "end"),
        ]
    );

    // Empty file add: no labels, no content.
    assert_eq!(
        v[6],
        FileDiff {
            path: "trunk/empty.txt".into(),
            old_label: None,
            new_label: None,
            content: DiffContent::None,
            prop_changes: vec![],
        }
    );

    let h = &hunks(&v[7])[0];
    assert_eq!(
        (h.old_start, h.old_len, h.new_start, h.new_len),
        (0, 0, 1, 30)
    );
    assert_eq!(h.lines.len(), 30);
    assert_eq!(h.lines[29], add(30, "line 30"));

    // Latin-1 bytes are replaced.
    assert_eq!(
        hunks(&v[8])[0].lines,
        [add(1, "caf\u{FFFD}"), add(2, "na\u{FFFD}ve")]
    );

    // No newline at end of file.
    assert_eq!(
        hunks(&v[9])[0].lines,
        [add(1, "first"), nonl(add(2, "last without newline"))]
    );
    assert_eq!(hunks(&v[11])[0].lines, [add(1, "unicode")]);
}

#[test]
fn diff_r3_modifications() {
    let v = diff("local_diff_r3.diff");
    assert_eq!(v.len(), 6);

    assert_eq!(v[0].path, "trunk/binary.bin");
    assert_eq!(v[0].content, DiffContent::Binary);
    assert!(v[0].prop_changes.is_empty());

    assert_eq!(
        v[1].old_label.as_deref(),
        Some("trunk/crlf.txt\t(revision 2)")
    );
    assert_eq!(
        v[1].new_label.as_deref(),
        Some("trunk/crlf.txt\t(revision 3)")
    );
    assert_eq!(
        hunks(&v[1])[0].lines,
        [
            ctx(1, 1, "one"),
            del(2, "two"),
            add(2, "TWO"),
            ctx(3, 3, "three")
        ]
    );

    assert_eq!(
        hunks(&v[2]),
        [Hunk {
            old_start: 1,
            old_len: 7,
            new_start: 1,
            new_len: 7,
            section: String::new(),
            lines: vec![
                ctx(1, 1, "keep"),
                del(2, "-- old dashes"),
                add(2, "--- new dashes"),
                ctx(3, 3, "middle"),
                del(4, "++ old plus"),
                del(5, "Index: not a header"),
                add(4, "+++ new plus"),
                add(5, "Index: still not a header"),
                ctx(6, 6, "@@ -1 +1 @@ not a hunk"),
                ctx(7, 7, "end"),
            ],
        }]
    );

    // Multi-hunk with a deletion shifting line numbers.
    let h = hunks(&v[3]);
    assert_eq!(h.len(), 2);
    assert_eq!(
        h[0].lines,
        [
            ctx(1, 1, "line 1"),
            ctx(2, 2, "line 2"),
            del(3, "line 3"),
            add(3, "line three"),
            ctx(4, 4, "line 4"),
            ctx(5, 5, "line 5"),
            ctx(6, 6, "line 6"),
        ]
    );
    assert_eq!(
        (h[1].old_start, h[1].old_len, h[1].new_start, h[1].new_len),
        (22, 9, 22, 8)
    );
    assert_eq!(
        h[1].lines,
        [
            ctx(22, 22, "line 22"),
            ctx(23, 23, "line 23"),
            ctx(24, 24, "line 24"),
            del(25, "line 25"),
            add(25, "line twenty-five"),
            ctx(26, 26, "line 26"),
            del(27, "line 27"),
            ctx(28, 27, "line 28"),
            ctx(29, 28, "line 29"),
            ctx(30, 29, "line 30"),
        ]
    );

    assert_eq!(
        hunks(&v[4])[0].lines,
        [
            ctx(1, 1, "caf\u{FFFD}"),
            del(2, "na\u{FFFD}ve"),
            add(2, "na\u{FFFD}ve!")
        ]
    );

    assert_eq!(
        hunks(&v[5])[0].lines,
        [
            ctx(1, 1, "first"),
            nonl(del(2, "last without newline")),
            nonl(add(2, "last changed, still no newline")),
        ]
    );
}

#[test]
fn diff_r4_property_only() {
    let v = diff("local_diff_r4.diff");
    assert_eq!(
        v,
        [
            FileDiff {
                path: "trunk/hello.txt".into(),
                old_label: Some("trunk/hello.txt\t(revision 3)".into()),
                new_label: Some("trunk/hello.txt\t(revision 4)".into()),
                content: DiffContent::None,
                prop_changes: vec![
                    prop("my:binary", None, Some("bin\x01ary\x02")),
                    prop("my:multi", None, Some("line one\nline two\n")),
                    prop("my:prop", None, Some("single value")),
                ],
            },
            FileDiff {
                path: "trunk".into(),
                old_label: Some("trunk\t(revision 3)".into()),
                new_label: Some("trunk\t(revision 4)".into()),
                content: DiffContent::None,
                prop_changes: vec![prop("svn:ignore", None, Some("*.o\n*.tmp\n"))],
            },
        ]
    );
}

#[test]
fn diff_r5_copy() {
    let v = diff("local_diff_r5.diff");
    assert_eq!(v.len(), 13);
    assert!(v.iter().all(|f| f.path.starts_with("branches/feature-1")));
    assert_eq!(v[7].path, "branches/feature-1/hello.txt");
    assert_eq!(hunks(&v[7])[0].lines.len(), 29);
    assert_eq!(v[7].prop_changes.len(), 3);
    assert_eq!(v[12].path, "branches/feature-1");
    assert_eq!(v[12].content, DiffContent::None);
}

#[test]
fn diff_r6_delete_replace_props() {
    let v = diff("local_diff_r6.diff");
    let paths: Vec<&str> = v.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "trunk/a b.txt",
            "trunk/amp&co.txt",
            "trunk/hello.txt",
            "trunk/new-empty.txt"
        ]
    );
    assert_eq!(
        v[0].new_label.as_deref(),
        Some("trunk/a b.txt\t(nonexistent)")
    );
    assert_eq!(
        hunks(&v[0]),
        [Hunk {
            old_start: 1,
            old_len: 1,
            new_start: 0,
            new_len: 0,
            section: String::new(),
            lines: vec![del(1, "spaces")],
        }]
    );
    assert_eq!(hunks(&v[1])[0].lines, [del(1, "amp"), add(1, "replaced")]);
    assert_eq!(v[2].content, DiffContent::None);
    assert_eq!(
        v[2].prop_changes,
        [
            prop("my:multi", Some("line one\nline two\n"), None),
            prop("my:prop", Some("single value"), Some("changed value")),
        ]
    );
    assert_eq!(v[3].content, DiffContent::None);
    assert_eq!(v[3].old_label, None);
}

#[test]
fn diff_r8_mergeinfo() {
    // svn prints a human-readable summary instead of the raw mergeinfo.
    let v = diff("local_diff_r8.diff");
    assert_eq!(
        v,
        [FileDiff {
            path: "trunk".into(),
            old_label: Some("trunk\t(revision 7)".into()),
            new_label: Some("trunk\t(revision 8)".into()),
            content: DiffContent::None,
            prop_changes: vec![prop(
                "svn:mergeinfo",
                None,
                Some("  Merged /branches/feature-1:r7\n")
            )],
        }]
    );
}

#[test]
fn diff_range_and_empty() {
    let v = diff("local_diff_r1_6.diff");
    assert_eq!(v.len(), 12);
    let dot = v.last().unwrap();
    assert_eq!(dot.path, ".");
    assert_eq!(
        dot.prop_changes,
        [prop("svn:ignore", None, Some("*.o\n*.tmp\n"))]
    );
    let hello = v.iter().find(|f| f.path == "hello.txt").unwrap();
    assert_eq!(hunks(hello)[0].lines.len(), 29);
    assert_eq!(
        hello.prop_changes,
        [
            prop("my:binary", None, Some("bin\x01ary\x02")),
            prop("my:prop", None, Some("changed value")),
        ]
    );

    assert!(diff("local_diff_empty.diff").is_empty());
    assert!(diff_bytes(b"").is_empty());
}

#[test]
fn diff_asf() {
    let v = diff("asf_diff.diff");
    assert_eq!(v.len(), 1);
    let f = &v[0];
    assert_eq!(f.path, "subversion/svnserve/logger.c");
    assert_eq!(
        f.old_label.as_deref(),
        Some("subversion/svnserve/logger.c\t(revision 1938821)")
    );
    let h = hunks(f);
    assert_eq!(h.len(), 1);
    assert_eq!(
        (h[0].old_start, h[0].old_len, h[0].new_start, h[0].new_len),
        (104, 10, 104, 11)
    );
    assert_eq!(h[0].lines.len(), 12);
    assert_eq!(h[0].lines[0], ctx(104, 104, ""));
    assert_eq!(
        h[0].lines[3],
        del(
            107,
            "      char *errstr = apr_palloc(logger->pool, errstr_size);"
        )
    );
    assert_eq!(h[0].lines[4], add(107, "      char *errstr;"));
    assert_eq!(
        h[0].lines[8],
        add(111, "      errstr = apr_palloc(logger->pool, errstr_size);")
    );
    assert_eq!(h[0].lines[11].old_no, Some(113));
    assert_eq!(h[0].lines[11].new_no, Some(114));
}

#[test]
fn diff_synthetic_edge_cases() {
    let input = b"garbage before\n\
Index: a.c\n\
===================================================================\n\
--- a.c\t(revision 1)\n\
+++ a.c\t(revision 2)\n\
@@ -10 +10,2 @@ int main(void)\n\
---- not a label\n\
++++ not a label\n\
+Index: not a file\n\
@@ -1,2 +1,2 @@\n\
-@@ -9 +9 @@\n\
+@@ -8 +8 @@\n\
\x20\n\
\\ No newline at end of file\n\
Index: crlf.txt\r\n\
===================================================================\r\n\
--- crlf.txt\t(revision 1)\r\n\
+++ crlf.txt\t(revision 2)\r\n\
@@ -1 +1 @@\r\n\
-a\r\r\n\
+b\r\n\
\r\n\
Property changes on: crlf.txt\r\n\
___________________________________________________________________\r\n\
Modified: svn:eol-style\r\n\
## -1 +1 ##\r\n\
-native\r\n\
\\ No newline at end of property\r\n\
+CRLF\r\n\
\\ No newline at end of property\r\n\
Modified: svn:mergeinfo\n\
## -0,0 +0,1 ##\n\
\x20\x20\x20Merged /branches/x:r3-5\n\
Index: old.bin\n\
===================================================================\n\
\n\
Property changes on: old.bin\n\
___________________________________________________________________\n\
Name: svn:mime-type\n\
\x20\x20\x20- text/plain\n\
\x20\x20\x20+ application/octet-stream\n\
Added: multi\n\
\x20\x20\x20+ first\n\
second\n\
\n\
Deleted: gone\n\
\x20\x20\x20- bye\n\
trailing garbage\n";
    let v = diff_bytes(input);
    assert_eq!(v.len(), 3);
    assert_eq!(
        hunks(&v[0]),
        [
            Hunk {
                old_start: 10,
                old_len: 1,
                new_start: 10,
                new_len: 2,
                section: "int main(void)".into(),
                lines: vec![
                    del(10, "--- not a label"),
                    add(10, "+++ not a label"),
                    add(11, "Index: not a file"),
                ],
            },
            Hunk {
                old_start: 1,
                old_len: 2,
                new_start: 1,
                new_len: 2,
                section: String::new(),
                lines: vec![
                    del(1, "@@ -9 +9 @@"),
                    add(1, "@@ -8 +8 @@"),
                    nonl(ctx(2, 2, "")),
                ],
            },
        ]
    );
    assert_eq!(v[1].path, "crlf.txt");
    assert_eq!(v[1].new_label.as_deref(), Some("crlf.txt\t(revision 2)"));
    // Only one \r is stripped.
    assert_eq!(hunks(&v[1])[0].lines, [del(1, "a\r"), add(1, "b")]);
    assert_eq!(
        v[1].prop_changes,
        [
            prop("svn:eol-style", Some("native"), Some("CRLF")),
            prop(
                "svn:mergeinfo",
                Some(""),
                Some("  Merged /branches/x:r3-5\n")
            ),
        ]
    );
    assert_eq!(v[2].path, "old.bin");
    assert_eq!(v[2].content, DiffContent::None);
    assert_eq!(
        v[2].prop_changes,
        [
            prop(
                "svn:mime-type",
                Some("text/plain"),
                Some("application/octet-stream")
            ),
            prop("multi", None, Some("first\nsecond")),
            prop("gone", Some("bye\ntrailing garbage"), None),
        ]
    );
}

#[test]
fn diff_truncated_hunk_is_tolerated() {
    let v = diff_bytes(b"Index: x\n=====\n--- x\n+++ x\n@@ -1,5 +1,5 @@\n a\n-b\n");
    assert_eq!(v.len(), 1);
    assert_eq!(hunks(&v[0])[0].lines, [ctx(1, 1, "a"), del(2, "b")]);
}
