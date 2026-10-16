mod common;

use common::*;
use svn_backend::parse;
use svn_backend::*;

fn list(name: &str) -> Vec<Entry> {
    let mut v = Vec::new();
    parse::list(fixture(name), |e| v.push(e)).unwrap();
    v
}

fn commit(rev: RevNum, author: &str, date: &str) -> CommitInfo {
    CommitInfo {
        rev,
        author: Some(author.into()),
        date: ts(date),
    }
}

#[test]
fn list_trunk() {
    let v = list("local_list_trunk.xml");
    let names: Vec<&str> = v.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "amp&co.txt",
            "at@sign.txt",
            "binary.bin",
            "crlf.txt",
            "dashes.txt",
            "hello.txt",
            "latin1.txt",
            "new-empty.txt",
            "noeol.txt",
            "sub dir",
            "unicodé-日本.txt",
        ]
    );
    assert_eq!(
        v[0],
        Entry {
            name: "amp&co.txt".into(),
            kind: NodeKind::File,
            size: Some(9),
            last_changed: commit(6, "bob", "2026-06-07T08:09:10Z"),
        }
    );
    assert_eq!(
        v[2].last_changed,
        commit(3, "carol", "2026-03-04T05:06:07Z")
    );
    assert_eq!(v[2].size, Some(20));
    assert_eq!(v[7].size, Some(0));
    assert_eq!(
        v[9],
        Entry {
            name: "sub dir".into(),
            kind: NodeKind::Dir,
            size: None,
            last_changed: commit(2, "bob", "2026-02-03T04:05:06.789012Z"),
        }
    );
}

#[test]
fn list_multiple_lists() {
    let v = list("local_list_multi.xml");
    assert_eq!(v.len(), 2);
    assert_eq!(v[0].name, "feature-1");
    assert_eq!(v[0].kind, NodeKind::Dir);
    assert_eq!(
        v[0].last_changed,
        commit(5, "alice", "2026-05-06T07:08:09.1Z")
    );
    assert_eq!(v[1].name, "nested.txt");
    assert_eq!(v[1].size, Some(7));
}

#[test]
fn list_asf() {
    let v = list("asf_list.xml");
    let got: Vec<(&str, NodeKind, Option<u64>, RevNum)> = v
        .iter()
        .map(|e| (e.name.as_str(), e.kind, e.size, e.last_changed.rev))
        .collect();
    assert_eq!(
        got,
        [
            ("README", NodeKind::File, Some(1256), 1903577),
            ("doxygen.conf", NodeKind::File, Some(62735), 1854600),
            ("programmer", NodeKind::Dir, None, 1849200),
            ("svn-square.jpg", NodeKind::File, Some(1440), 1663949),
            ("user", NodeKind::Dir, None, 1903577),
        ]
    );
    assert_eq!(v[0].last_changed.author.as_deref(), Some("hartmannathan"));
    assert_eq!(v[0].last_changed.date, ts("2022-08-19T16:31:35.591491Z"));
}

#[test]
fn list_tolerant() {
    // Missing author/date, unknown elements and attributes, a lock, numeric
    // character references and a bare <list> root.
    let xml = r#"<list path="x"><entry kind="file" extra="1">
<name>&#x41;&#66;&lt;&gt;&quot;&apos;</name><size>5</size>
<commit revision="3"/>
<lock><token>t</token><owner>o</owner><created>2020-01-01T00:00:00Z</created></lock>
<whatever><name>not me</name></whatever>
</entry></list>"#;
    let mut v = Vec::new();
    parse::list(xml.as_bytes(), |e| v.push(e)).unwrap();
    assert_eq!(
        v,
        vec![Entry {
            name: "AB<>\"'".into(),
            kind: NodeKind::File,
            size: Some(5),
            last_changed: CommitInfo {
                rev: 3,
                author: None,
                date: None
            },
        }]
    );
}

#[test]
fn list_emits_before_error() {
    // Truncated output (e.g. svn killed): entries before the cut are
    // delivered, then an error.
    let xml = r#"<lists><list path="x"><entry kind="dir"><name>a</name><commit revision="1"/></entry><entry kind="file"><name>b"#;
    let mut v = Vec::new();
    let err = parse::list(xml.as_bytes(), |e| v.push(e)).unwrap_err();
    assert_eq!(v.len(), 1);
    assert!(err.0.contains("svn list"), "{err}");
}

#[test]
fn list_malformed() {
    for bad in [
        "<lists><list><entry kind=\"file\"><name>a</nam></entry></list></lists>",
        "<lists><list><entry kind=\"file\"></entry></list></lists>",
        "<lists><list><entry kind=\"symlink\"><name>a</name></entry></list></lists>",
        "<lists><list><entry><name>a&bogus;</name></entry></list></lists>",
        "not xml",
    ] {
        let r = parse::list(bad.as_bytes(), |_| {});
        assert!(r.is_err(), "{bad}");
    }
}
