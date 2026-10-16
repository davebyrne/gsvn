//! End-to-end tests with the real `svn` client against a throw-away
//! `file://` repository. Every svn call (here and in the backend) uses a
//! temporary `--config-dir`, so `~/.subversion` is never touched.
//!
//! Repository history:
//!
//! | rev | author | change                                             |
//! |-----|--------|----------------------------------------------------|
//! | 1   | bob    | mkdir /trunk /branches /tags  "Create layout"      |
//! | 2   | bob    | add trunk/a.txt, trunk/dir/b.txt, trunk/sp ace@x.txt "Import alpha files" |
//! | 3   | alice  | modify trunk/a.txt  "Fix beta bug"                 |
//! | 4   | bob    | copy trunk → branches/feature  "Branch feature"    |
//! | 5   | alice  | modify branches/feature/a.txt  "Feature beta work" |
//! | 6   | bob    | propset on trunk/a.txt  "Set props"                |

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use svn_backend::*;

struct Repo {
    _dir: tempfile::TempDir,
    path: PathBuf,
    cfg: PathBuf,
    url: RepoUrl,
}

fn run(cfg: &Path, cwd: &Path, args: &[&str]) {
    let out = Command::new("svn")
        .arg("--non-interactive")
        .arg("--config-dir")
        .arg(cfg)
        .args(args)
        .current_dir(cwd)
        .env("LC_ALL", "C")
        .output()
        .expect("svn installed");
    assert!(
        out.status.success(),
        "svn {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn make_repo() -> Repo {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("repo");
    let cfg = dir.path().join("cfg");
    let wc = dir.path().join("wc");
    std::fs::create_dir(&cfg).unwrap();
    let st = Command::new("svnadmin").arg("create").arg(&path).status().unwrap();
    assert!(st.success());
    let url = RepoUrl::parse(&format!("file://{}", path.display())).unwrap();
    let u = url.as_str();
    let d = dir.path();
    let trunk = format!("{u}/trunk");
    run(&cfg, d, &["mkdir", "--username", "bob", "-m", "Create layout", &trunk, &format!("{u}/branches"), &format!("{u}/tags")]);
    run(&cfg, d, &["checkout", "--username", "bob", &trunk, wc.to_str().unwrap()]);
    std::fs::write(wc.join("a.txt"), "line1\nline2\nline3\n").unwrap();
    std::fs::create_dir(wc.join("dir")).unwrap();
    std::fs::write(wc.join("dir/b.txt"), "b\n").unwrap();
    std::fs::write(wc.join("sp ace@x.txt"), "space\n").unwrap();
    run(&cfg, &wc, &["add", "a.txt", "dir", "sp ace@x.txt@"]);
    run(&cfg, &wc, &["commit", "--username", "bob", "-m", "Import alpha files"]);
    std::fs::write(wc.join("a.txt"), "line1\nline2 fixed\nline3\n").unwrap();
    run(&cfg, &wc, &["commit", "--username", "alice", "-m", "Fix beta bug"]);
    run(&cfg, d, &["copy", "--username", "bob", "-m", "Branch feature", &trunk, &format!("{u}/branches/feature")]);
    let fwc = d.join("fwc");
    run(&cfg, d, &["checkout", &format!("{u}/branches/feature"), fwc.to_str().unwrap()]);
    std::fs::write(fwc.join("a.txt"), "line1\nline2 fixed\nline3\nfeature\n").unwrap();
    run(&cfg, &fwc, &["commit", "--username", "alice", "-m", "Feature beta work"]);
    run(&cfg, &wc, &["update"]);
    run(&cfg, &wc, &["propset", "custom:prop", "hello", "a.txt"]);
    run(&cfg, &wc, &["commit", "--username", "bob", "-m", "Set props"]);
    Repo {
        _dir: dir,
        path,
        cfg,
        url,
    }
}

fn svn_for(repo: &Repo) -> Svn {
    Svn::new(SvnConfig {
        config_dir: Some(repo.cfg.clone()),
        interactive: false,
        batch_size: 2,
        ..SvnConfig::default()
    })
}

/// Shared read-only repository (building it takes a moment).
fn repo() -> &'static Repo {
    static REPO: OnceLock<Repo> = OnceLock::new();
    REPO.get_or_init(make_repo)
}

const P: Priority = Priority::Normal;

#[test]
fn info_resolves_head() {
    let r = repo();
    let svn = svn_for(r);
    let i = svn.info(&r.url, &Rev::Head, P).wait().unwrap();
    assert_eq!(i.rev, 6);
    assert_eq!(i.root, r.url);
    assert_eq!(i.kind, NodeKind::Dir);
    assert_eq!(i.repos_path, "/");
    let f = svn
        .info(&r.url.join("trunk/a.txt"), &Rev::Number(3), P)
        .wait()
        .unwrap();
    assert_eq!(f.kind, NodeKind::File);
    assert_eq!(f.last_changed.rev, 3);
    assert_eq!(f.last_changed.author.as_deref(), Some("alice"));
    assert_eq!(f.size, Some(24));
}

#[test]
fn list_and_cache_hit() {
    // Own repository: this test moves it away temporarily.
    let r = make_repo();
    let svn = svn_for(&r);
    let trunk = r.url.join("trunk");
    let mut s = svn.list(&trunk, 2, P);
    let mut all = Vec::new();
    while let Some(b) = s.next_blocking() {
        let b = b.unwrap();
        assert!(b.len() <= 2);
        all.extend(b);
    }
    let mut names: Vec<_> = all.iter().map(|e| e.name.as_str()).collect();
    names.sort();
    assert_eq!(names, ["a.txt", "dir", "sp ace@x.txt"]);
    let a = all.iter().find(|e| e.name == "a.txt").unwrap();
    assert_eq!((a.kind, a.size, a.last_changed.rev), (NodeKind::File, Some(18), 2));
    let d = all.iter().find(|e| e.name == "dir").unwrap();
    assert_eq!((d.kind, d.size), (NodeKind::Dir, None));
    // Sub-directory with special characters in a sibling's name works via join.
    let sp = svn
        .info(&trunk.join("sp ace@x.txt"), &Rev::Number(2), P)
        .wait()
        .unwrap();
    assert_eq!(sp.kind, NodeKind::File);

    // Take the repository away: cached results still work, others fail.
    let moved = r.path.with_extension("moved");
    std::fs::rename(&r.path, &moved).unwrap();
    let again = svn.list(&trunk, 2, P).collect_blocking().unwrap();
    assert_eq!(again, all);
    let e = svn.list(&trunk, 3, P).collect_blocking().unwrap_err();
    assert_eq!(e.kind, ErrorKind::NotFound, "{e:?}");
    svn.clear_cache();
    let e = svn.list(&trunk, 2, P).collect_blocking().unwrap_err();
    assert_eq!(e.kind, ErrorKind::NotFound);
    std::fs::rename(&moved, &r.path).unwrap();
    assert_eq!(svn.list(&trunk, 2, P).collect_blocking().unwrap().len(), 3);
}

fn revs(v: &[LogEntry]) -> Vec<u64> {
    v.iter().map(|e| e.rev).collect()
}

#[test]
fn log_paging() {
    let r = repo();
    let svn = svn_for(r);
    let p1 = svn.log(&r.url, 6, &LogQuery::page(6, 2), P).collect_blocking().unwrap();
    assert_eq!(revs(&p1), [6, 5]);
    assert_eq!(p1[0].message, "Set props");
    assert_eq!(p1[0].author.as_deref(), Some("bob"));
    assert!(p1[0].changed_paths.is_empty());
    let p2 = svn
        .log(&r.url, 6, &LogQuery::page(p1[1].rev - 1, 2), P)
        .collect_blocking()
        .unwrap();
    assert_eq!(revs(&p2), [4, 3]);
    let all = svn.log(&r.url, 6, &LogQuery::page(6, 0), P).collect_blocking().unwrap();
    assert_eq!(revs(&all), [6, 5, 4, 3, 2, 1]);
}

#[test]
fn log_search_all_terms_must_match() {
    let r = repo();
    let svn = svn_for(r);
    let q = |terms: &[&str]| LogQuery {
        search: terms.iter().map(|s| s.to_string()).collect(),
        ..LogQuery::page(6, 0)
    };
    let hits = svn.log(&r.url, 6, &q(&["BETA"]), P).collect_blocking().unwrap();
    assert_eq!(revs(&hits), [5, 3]);
    let hits = svn.log(&r.url, 6, &q(&["beta", "fix"]), P).collect_blocking().unwrap();
    assert_eq!(revs(&hits), [3]);
    let hits = svn.log(&r.url, 6, &q(&["beta", "alice"]), P).collect_blocking().unwrap();
    assert_eq!(revs(&hits), [5, 3]);
    let hits = svn.log(&r.url, 6, &q(&["beta", "nomatch"]), P).collect_blocking().unwrap();
    assert!(hits.is_empty());
    // Non-ASCII terms must not trip svn's native-encoding conversion.
    let hits = svn.log(&r.url, 6, &q(&["grüße"]), P).collect_blocking().unwrap();
    assert!(hits.is_empty());
}

#[test]
fn log_stop_on_copy_and_verbose() {
    let r = repo();
    let svn = svn_for(r);
    let feature = r.url.join("branches/feature");
    let q = LogQuery {
        stop_on_copy: true,
        verbose: true,
        ..LogQuery::page(6, 0)
    };
    let v = svn.log(&feature, 6, &q, P).collect_blocking().unwrap();
    assert_eq!(revs(&v), [5, 4]);
    assert_eq!(v[0].changed_paths[0].path, "/branches/feature/a.txt");
}

#[test]
fn log_entry_changed_paths() {
    let r = repo();
    let svn = svn_for(r);
    let e = svn.log_entry(&r.url, 4, P).wait().unwrap();
    assert_eq!(e.rev, 4);
    assert_eq!(e.message, "Branch feature");
    assert_eq!(e.changed_paths.len(), 1);
    let cp = &e.changed_paths[0];
    assert_eq!(cp.action, ChangeAction::Added);
    assert_eq!(cp.path, "/branches/feature");
    assert_eq!(cp.copy_from, Some(("/trunk".to_owned(), 3)));
    let e2 = svn.log_entry(&r.url, 2, P).wait().unwrap();
    let mut paths: Vec<_> = e2.changed_paths.iter().map(|c| c.path.as_str()).collect();
    paths.sort();
    assert_eq!(
        paths,
        ["/trunk/a.txt", "/trunk/dir", "/trunk/dir/b.txt", "/trunk/sp ace@x.txt"]
    );
    // A path not touched in that revision has no entry.
    let e = svn
        .log_entry(&r.url.join("tags"), 3, P)
        .wait()
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::NotFound);
}

#[test]
fn cat_file() {
    let r = repo();
    let svn = svn_for(r);
    let a = r.url.join("trunk/a.txt");
    let out = svn.cat(&a, 3, None, P).wait().unwrap();
    assert_eq!(out.data, b"line1\nline2 fixed\nline3\n");
    assert!(!out.truncated);
    let out = svn.cat(&a, 2, Some(5), P).wait().unwrap();
    assert_eq!((out.data.as_slice(), out.truncated), (&b"line1"[..], true));
    let out = svn.cat(&r.url.join("trunk/sp ace@x.txt"), 6, None, P).wait().unwrap();
    assert_eq!(out.data, b"space\n");
    let e = svn.cat(&r.url.join("trunk"), 6, None, P).wait().unwrap_err();
    assert_eq!(e.kind, ErrorKind::Other, "{e:?}");
}

#[test]
fn blame_file() {
    let r = repo();
    let svn = svn_for(r);
    let a = r.url.join("branches/feature/a.txt");
    let lines = svn.blame(&a, 5, None, P).collect_blocking().unwrap();
    let revs: Vec<_> = lines.iter().map(|l| l.commit.as_ref().map(|c| c.rev)).collect();
    assert_eq!(revs, [Some(2), Some(3), Some(2), Some(5)]);
    assert_eq!(lines[3].line_no, 4);
    let lines = svn.blame(&a, 5, Some(4), P).collect_blocking().unwrap();
    let revs: Vec<_> = lines.iter().map(|l| l.commit.as_ref().map(|c| c.rev)).collect();
    assert_eq!(revs, [None, None, None, Some(5)]);
}

#[test]
fn proplist_file() {
    let r = repo();
    let svn = svn_for(r);
    let props = svn.proplist(&r.url.join("trunk/a.txt"), 6, P).wait().unwrap();
    assert_eq!(
        props,
        vec![Property {
            name: "custom:prop".into(),
            value: "hello".into()
        }]
    );
    let none = svn.proplist(&r.url.join("trunk/a.txt"), 5, P).wait().unwrap();
    assert!(none.is_empty());
}

#[test]
#[ignore = "needs parse::diff from backend/parsers"]
fn diff_change_and_range() {
    let r = repo();
    let svn = svn_for(r);
    let files = svn
        .diff(
            &DiffSpec::Change {
                url: r.url.clone(),
                rev: 3,
            },
            P,
        )
        .collect_blocking()
        .unwrap();
    assert_eq!(files.len(), 1);
    assert!(files[0].path.ends_with("a.txt"), "{:?}", files[0].path);
    let DiffContent::Text(hunks) = &files[0].content else {
        panic!("expected text diff: {:?}", files[0].content);
    };
    let added: Vec<_> = hunks[0]
        .lines
        .iter()
        .filter(|l| l.kind == DiffLineKind::Added)
        .map(|l| l.text.as_str())
        .collect();
    assert_eq!(added, ["line2 fixed"]);

    let files = svn
        .diff(
            &DiffSpec::Range {
                url: r.url.join("trunk/a.txt"),
                peg: 6,
                from: 2,
                to: 6,
            },
            P,
        )
        .collect_blocking()
        .unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].prop_changes.len(), 1);
}

#[test]
fn not_found_errors() {
    let r = repo();
    let svn = svn_for(r);
    let e = svn.info(&r.url.join("nope"), &Rev::Head, P).wait().unwrap_err();
    assert_eq!(e.kind, ErrorKind::NotFound, "{e:?}");
    assert!(e.command.contains("info --xml"));
    let e = svn.info(&r.url, &Rev::Number(99), P).wait().unwrap_err();
    assert_eq!((e.kind, e.code), (ErrorKind::NotFound, Some(160006)));
    let e = svn.list(&r.url.join("nope"), 6, P).collect_blocking().unwrap_err();
    assert_eq!(e.kind, ErrorKind::NotFound, "{e:?}");
    let e = svn
        .log(&r.url.join("nope"), 6, &LogQuery::page(6, 10), P)
        .collect_blocking()
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::NotFound, "{e:?}");
    let missing = RepoUrl::parse("file:///nonexistent/gsvn/repo").unwrap();
    let e = svn.info(&missing, &Rev::Head, P).wait().unwrap_err();
    assert_eq!(e.kind, ErrorKind::NotFound, "{e:?}");
}
