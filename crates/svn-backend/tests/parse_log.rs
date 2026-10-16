mod common;

use common::*;
use svn_backend::parse;
use svn_backend::*;

fn log(name: &str) -> Vec<LogEntry> {
    let mut v = Vec::new();
    parse::log(fixture(name), |e| v.push(e)).unwrap();
    v
}

fn cp(action: ChangeAction, path: &str, kind: NodeKind, text: bool, props: bool) -> ChangedPath {
    ChangedPath {
        action,
        path: path.into(),
        kind: Some(kind),
        copy_from: None,
        text_mods: text,
        prop_mods: props,
    }
}

const MSG2: &str = "\n  Add files <b>&amp; \"stuff\"</b> 'q' & more\n\nUnicode: 日本語 — ü\n\n";

#[test]
fn log_plain() {
    let v = log("local_log.xml");
    let revs: Vec<RevNum> = v.iter().map(|e| e.rev).collect();
    assert_eq!(revs, [6, 5, 4, 3, 2, 1]);
    assert!(v.iter().all(|e| e.changed_paths.is_empty()));
    assert_eq!(
        v[0],
        LogEntry {
            rev: 6,
            author: Some("bob".into()),
            date: ts("2026-06-07T08:09:10Z"),
            message: "Delete, replace & add".into(),
            changed_paths: vec![],
        }
    );
    // r4 has no svn:author.
    assert_eq!(v[2].author, None);
    assert_eq!(v[2].date, ts("2026-04-05T06:07:08Z"));
    // r3 has an empty message.
    assert_eq!(v[3].message, "");
    assert_eq!(v[3].author.as_deref(), Some("carol"));
    // Markup, entities, unicode and surrounding newlines kept exactly.
    assert_eq!(v[4].message, MSG2);
    assert_eq!(v[5].date, ts("2026-01-02T10:00:00.000001Z"));
}

#[test]
fn log_verbose() {
    use ChangeAction::*;
    use NodeKind::*;
    let v = log("local_log_v.xml");
    assert_eq!(v.len(), 6);
    assert_eq!(
        v[0].changed_paths,
        vec![
            cp(Deleted, "/trunk/a b.txt", File, false, false),
            cp(Replaced, "/trunk/amp&co.txt", File, true, false),
            cp(Deleted, "/trunk/empty.txt", File, false, false),
            cp(Modified, "/trunk/hello.txt", File, false, true),
            cp(Added, "/trunk/new-empty.txt", File, true, false),
        ]
    );
    assert_eq!(
        v[1].changed_paths,
        vec![ChangedPath {
            copy_from: Some(("/trunk".into(), 4)),
            ..cp(Added, "/branches/feature-1", Dir, false, false)
        }]
    );
    assert_eq!(v[1].message, "Branch feature-1");
    assert_eq!(v[2].author, None);
    assert_eq!(
        v[2].changed_paths,
        vec![
            cp(Modified, "/trunk", Dir, false, true),
            cp(Modified, "/trunk/hello.txt", File, false, true),
        ]
    );
    assert_eq!(v[3].changed_paths.len(), 6);
    assert_eq!(v[4].message, MSG2);
    assert_eq!(v[4].changed_paths.len(), 13);
    assert_eq!(
        v[4].changed_paths[3],
        cp(Added, "/trunk/binary.bin", File, true, true)
    );
    assert_eq!(
        v[4].changed_paths[10],
        cp(Added, "/trunk/sub dir", Dir, false, false)
    );
    assert_eq!(v[4].changed_paths[12].path, "/trunk/unicodé-日本.txt");
    assert_eq!(v[5].changed_paths.len(), 3);
}

#[test]
fn log_asf() {
    let v = log("asf_log_v.xml");
    assert_eq!(v.len(), 20);
    assert_eq!(v[0].rev, 1939086);
    assert_eq!(v[0].author.as_deref(), Some("kotkov"));
    assert_eq!(v[0].date, ts("2026-10-13T09:35:07.729908Z"));
    assert_eq!(
        v[0].message,
        "* tools/dist/release-lines.yaml: Update recommended_release (to 1.15) and\n  supported_release_lines (to 1.14 and 1.15).\n"
    );
    assert_eq!(
        v[0].changed_paths,
        vec![cp(
            ChangeAction::Modified,
            "/subversion/trunk/tools/dist/release-lines.yaml",
            NodeKind::File,
            true,
            false
        )]
    );
    let r = v.iter().find(|e| e.rev == 1938248).unwrap();
    assert!(r.changed_paths.contains(&cp(
        ChangeAction::Deleted,
        "/subversion/trunk/subversion/libsvn_ra/wrapper_template.h",
        NodeKind::File,
        false,
        false
    )));
    assert_eq!(v[19].rev, 1938205);
    assert!(v[19].message.ends_with("current layout of INSTALL."));
    let total: usize = v.iter().map(|e| e.changed_paths.len()).sum();
    assert_eq!(total, 33);
}

#[test]
fn log_tolerant() {
    // Old servers: no kind / text-mods / prop-mods; unknown action skipped;
    // missing author/date/msg; nested -g entries ignored; CR via &#13;.
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<log>
<logentry revision="10">
<paths>
<path action="M">/a</path>
<path action="X">/weird</path>
<path action="A" copyfrom-path="/b" copyfrom-rev="bogus">/c</path>
</paths>
<logentry revision="7" reverse-merge="false">
<author>merger</author><date>2001-01-01T00:00:00Z</date>
<paths><path action="D">/nested</path></paths>
<msg>nested</msg>
</logentry>
<msg>a&#13;
b</msg>
<extra><author>not me</author></extra>
</logentry>
<logentry revision="9"></logentry>
</log>"#;
    let mut v = Vec::new();
    parse::log(xml.as_bytes(), |e| v.push(e)).unwrap();
    assert_eq!(v.len(), 2);
    assert_eq!(v[0].rev, 10);
    assert_eq!(v[0].author, None);
    assert_eq!(v[0].date, None);
    assert_eq!(v[0].message, "a\r\nb");
    assert_eq!(
        v[0].changed_paths,
        vec![
            ChangedPath {
                action: ChangeAction::Modified,
                path: "/a".into(),
                kind: None,
                copy_from: None,
                text_mods: false,
                prop_mods: false,
            },
            ChangedPath {
                action: ChangeAction::Added,
                path: "/c".into(),
                kind: None,
                copy_from: None,
                text_mods: false,
                prop_mods: false,
            },
        ]
    );
    assert_eq!(
        v[1],
        LogEntry {
            rev: 9,
            author: None,
            date: None,
            message: String::new(),
            changed_paths: vec![]
        }
    );
}

#[test]
fn log_errors() {
    let mut n = 0;
    let e = parse::log(
        "<log><logentry revision=\"3\"></logentry><logentry revision=\"2\"><msg>x</log>".as_bytes(),
        |_| n += 1,
    )
    .unwrap_err();
    assert_eq!(n, 1);
    assert!(e.0.contains("svn log"));
    assert!(parse::log("<log><logentry></logentry></log>".as_bytes(), |_| {}).is_err());
    assert!(parse::log("<log><logentry revision=\"1\">".as_bytes(), |_| {}).is_err());
}
