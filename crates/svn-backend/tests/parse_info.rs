mod common;

use common::*;
use svn_backend::parse;
use svn_backend::*;

fn url(s: &str) -> RepoUrl {
    RepoUrl::parse(s).unwrap()
}

#[test]
fn info_root() {
    let v = parse::info(fixture("local_info_root.xml")).unwrap();
    assert_eq!(
        v,
        vec![Info {
            url: url(FAKE_ROOT),
            root: url(FAKE_ROOT),
            uuid: UUID.into(),
            repos_path: "/".into(),
            rev: 6,
            kind: NodeKind::Dir,
            last_changed: CommitInfo {
                rev: 6,
                author: Some("bob".into()),
                date: ts("2026-06-07T08:09:10Z"),
            },
            size: None,
        }]
    );
}

#[test]
fn info_multi_targets() {
    let v = parse::info(fixture("local_info_multi.xml")).unwrap();
    assert_eq!(v.len(), 4);
    let bob2 = CommitInfo {
        rev: 2,
        author: Some("bob".into()),
        date: ts("2026-02-03T04:05:06.789012Z"),
    };

    assert_eq!(v[0].url.as_str(), format!("{FAKE_ROOT}/trunk/a%20b.txt"));
    assert_eq!(v[0].repos_path, "/trunk/a b.txt");
    assert_eq!(v[0].rev, 5);
    assert_eq!(v[0].kind, NodeKind::File);
    assert_eq!(v[0].size, Some(7));
    assert_eq!(v[0].last_changed, bob2);
    assert_eq!(v[0].root, url(FAKE_ROOT));
    assert_eq!(v[0].uuid, UUID);

    assert_eq!(v[1].repos_path, "/trunk/unicodé-日本.txt");
    assert_eq!(
        v[1].url.as_str(),
        format!("{FAKE_ROOT}/trunk/unicod%C3%A9-%E6%97%A5%E6%9C%AC.txt")
    );
    assert_eq!(v[1].url.name(), "unicodé-日本.txt");
    assert_eq!(v[1].size, Some(8));

    assert_eq!(v[2].repos_path, "/trunk/at@sign.txt");
    assert_eq!(v[2].size, Some(3));

    assert_eq!(v[3].repos_path, "/branches");
    assert_eq!(v[3].kind, NodeKind::Dir);
    assert_eq!(v[3].size, None);
    assert_eq!(v[3].rev, 6);
    assert_eq!(v[3].last_changed.rev, 5);
    assert_eq!(v[3].last_changed.author.as_deref(), Some("alice"));
    assert_eq!(v[3].last_changed.date, ts("2026-05-06T07:08:09.1Z"));
}

#[test]
fn info_asf() {
    let v = parse::info(fixture("asf_info.xml")).unwrap();
    assert_eq!(v.len(), 1);
    let i = &v[0];
    assert_eq!(i.url.as_str(), "https://svn.apache.org/repos/asf/subversion/trunk");
    assert_eq!(i.root.as_str(), "https://svn.apache.org/repos/asf");
    assert_eq!(i.uuid, "13f79535-47bb-0310-9956-ffa450edef68");
    assert_eq!(i.repos_path, "/subversion/trunk");
    assert_eq!(i.rev, 1939142);
    assert_eq!(i.kind, NodeKind::Dir);
    assert_eq!(i.last_changed.rev, 1939086);
    assert_eq!(i.last_changed.author.as_deref(), Some("kotkov"));
    assert_eq!(i.last_changed.date, ts("2026-10-13T09:35:07.729908Z"));
}

#[test]
fn info_without_relative_url_and_with_lock() {
    // Older svn (< 1.8) has no <relative-url>; locks have their own <date>.
    let xml = r#"<?xml version="1.0"?>
<info><entry kind="file" path="x" revision="9" size="12" unknown="1">
<url>http://h/r/a%26b/x%20y</url>
<repository><root>http://h/r</root><uuid>u</uuid></repository>
<commit revision="7"><author>me</author><date>2020-01-01T00:00:00.000000Z</date></commit>
<lock><token>t</token><owner>o</owner><created>2021-01-01T00:00:00.000000Z</created><date>bogus</date></lock>
<future-element><date>bogus</date></future-element>
</entry></info>"#;
    let v = parse::info(xml.as_bytes()).unwrap();
    assert_eq!(v[0].repos_path, "/a&b/x y");
    assert_eq!(v[0].last_changed.date, ts("2020-01-01T00:00:00Z"));
    assert_eq!(v[0].size, Some(12));
}

#[test]
fn info_errors() {
    for bad in [
        "",
        "<info><entry kind=\"dir\" revision=\"1\"><url>http://h/r</url>",
        "<info><entry></info>",
        "<info><entry kind=\"dir\" revision=\"1\"></entry></info>",
    ] {
        let e = parse::info(bad.as_bytes()).unwrap_err();
        assert!(e.0.contains("svn info"), "{e}");
    }
}
