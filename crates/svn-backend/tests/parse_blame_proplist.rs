mod common;

use common::*;
use svn_backend::parse;
use svn_backend::*;

fn blame(name: &str) -> Vec<BlameLine> {
    let mut v = Vec::new();
    parse::blame(fixture(name), |l| v.push(l)).unwrap();
    v
}

#[test]
fn blame_full() {
    let v = blame("local_blame.xml");
    assert_eq!(v.len(), 29);
    let bob = CommitInfo {
        rev: 2,
        author: Some("bob".into()),
        date: ts("2026-02-03T04:05:06.789012Z"),
    };
    let carol = CommitInfo {
        rev: 3,
        author: Some("carol".into()),
        date: ts("2026-03-04T05:06:07Z"),
    };
    for (i, l) in v.iter().enumerate() {
        assert_eq!(l.line_no, i as u64 + 1);
        let want = if l.line_no == 3 || l.line_no == 25 {
            &carol
        } else {
            &bob
        };
        assert_eq!(l.commit.as_ref(), Some(want), "line {}", l.line_no);
    }
}

#[test]
fn blame_range_has_lines_without_commit() {
    let v = blame("local_blame_range.xml");
    assert_eq!(v.len(), 29);
    assert_eq!(
        v[0],
        BlameLine {
            line_no: 1,
            commit: None
        }
    );
    let with: Vec<u64> = v
        .iter()
        .filter(|l| l.commit.is_some())
        .map(|l| l.line_no)
        .collect();
    assert_eq!(with, [3, 25]);
    assert_eq!(
        v[2].commit.as_ref().unwrap().author.as_deref(),
        Some("carol")
    );
}

#[test]
fn blame_asf() {
    let v = blame("asf_blame.xml");
    assert_eq!(v.len(), 28);
    assert_eq!(
        v[0],
        BlameLine {
            line_no: 1,
            commit: Some(CommitInfo {
                rev: 839458,
                author: Some("sussman".into()),
                date: ts("2001-06-04T22:00:53Z"),
            })
        }
    );
    assert_eq!(v[2].commit.as_ref().unwrap().rev, 836496);
    assert!(v.iter().all(|l| l.commit.is_some()));
    assert_eq!(v[27].line_no, 28);
}

#[test]
fn blame_errors_and_tolerance() {
    let xml = r#"<blame><target path="x"><entry line-number="1"><commit revision="4"></commit><merged path="/b"><commit revision="2"><author>m</author></commit></merged></entry></target></blame>"#;
    let mut v = Vec::new();
    parse::blame(xml.as_bytes(), |l| v.push(l)).unwrap();
    assert_eq!(
        v,
        vec![BlameLine {
            line_no: 1,
            commit: Some(CommitInfo {
                rev: 4,
                author: None,
                date: None
            })
        }]
    );
    assert!(
        parse::blame(
            "<blame><target><entry></entry></target></blame>".as_bytes(),
            |_| {}
        )
        .is_err()
    );
    assert!(parse::blame("<blame><target>".as_bytes(), |_| {}).is_err());
}

fn props(name: &str) -> Vec<(String, String)> {
    parse::proplist(fixture(name))
        .unwrap()
        .into_iter()
        .map(|p| (p.name, p.value))
        .collect()
}

fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
    v.iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}

#[test]
fn proplist_file_with_base64_and_multiline() {
    assert_eq!(
        props("local_proplist_file.xml"),
        pairs(&[
            ("my:binary", "bin\x01ary\x02"),
            ("my:multi", "line one\nline two\n"),
            ("my:prop", "single value"),
        ])
    );
}

#[test]
fn proplist_dir_and_empty() {
    assert_eq!(
        props("local_proplist_dir.xml"),
        pairs(&[("svn:ignore", "*.o\n*.tmp\n")])
    );
    assert_eq!(props("local_proplist_none.xml"), pairs(&[]));
}

#[test]
fn proplist_escapes_and_bad_base64() {
    let xml = r#"<properties><target path="x">
<property name="a&amp;b">&lt;x&gt; &amp; y</property>
<property name="e"></property>
<property name="bad" encoding="base64">!!!</property>
</target></properties>"#;
    let v = parse::proplist(xml.as_bytes()).unwrap();
    assert_eq!(
        v,
        vec![
            Property {
                name: "a&b".into(),
                value: "<x> & y".into()
            },
            Property {
                name: "e".into(),
                value: "".into()
            },
            Property {
                name: "bad".into(),
                value: "!!!".into()
            },
        ]
    );
    assert!(
        parse::proplist(
            "<properties><target><property>x</property></target></properties>".as_bytes()
        )
        .is_err()
    );
}
