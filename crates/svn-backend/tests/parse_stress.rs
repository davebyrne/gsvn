//! Synthetic large inputs. Timings are printed (run with `--nocapture`);
//! a generous upper bound is asserted only in release builds.

use std::fmt::Write as _;
use std::time::{Duration, Instant};
use svn_backend::parse;
use svn_backend::*;

fn check_time(what: &str, d: Duration, release_limit: Duration) {
    println!("{what}: {:.1} ms", d.as_secs_f64() * 1000.0);
    if cfg!(not(debug_assertions)) {
        assert!(d < release_limit, "{what} took {d:?}");
    }
}

#[test]
fn list_100k_entries() {
    const N: usize = 100_000;
    let mut xml = String::with_capacity(N * 220);
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<lists>\n<list\n   path=\"https://h/r/big\">\n");
    for i in 0..N {
        if i % 10 == 0 {
            write!(
                xml,
                "<entry\n   kind=\"dir\">\n<name>dir &amp; {i}</name>\n<commit\n   revision=\"{}\">\n<author>user{}</author>\n<date>2026-10-15T18:45:04.289621Z</date>\n</commit>\n</entry>\n",
                i + 1,
                i % 7
            )
            .unwrap();
        } else {
            write!(
                xml,
                "<entry\n   kind=\"file\">\n<name>file-{i}.c</name>\n<size>{i}</size>\n<commit\n   revision=\"{}\">\n<author>user{}</author>\n<date>2026-10-15T18:45:04.289621Z</date>\n</commit>\n</entry>\n",
                i + 1,
                i % 7
            )
            .unwrap();
        }
    }
    xml.push_str("</list>\n</lists>\n");
    println!("list XML: {:.1} MB", xml.len() as f64 / 1e6);

    let date: Timestamp = "2026-10-15T18:45:04.289621Z".parse().unwrap();
    let t = Instant::now();
    let mut all = Vec::with_capacity(N);
    parse::list(std::io::BufReader::new(xml.as_bytes()), |e| all.push(e)).unwrap();
    let d = t.elapsed();
    check_time("list 100k", d, Duration::from_millis(500));

    let mut n = 0usize;
    for e in all {
        if n.is_multiple_of(10) {
            assert_eq!(e.kind, NodeKind::Dir);
            assert_eq!(e.name, format!("dir & {n}"));
            assert_eq!(e.size, None);
        } else {
            assert_eq!(e.kind, NodeKind::File);
            assert_eq!(e.size, Some(n as u64));
        }
        assert_eq!(e.last_changed.rev, n as u64 + 1);
        assert_eq!(e.last_changed.date, Some(date));
        n += 1;
    }
    assert_eq!(n, N);
}

/// Reader that tracks how much of the input has been handed out.
struct Counting<'a> {
    data: &'a [u8],
    pos: std::rc::Rc<std::cell::Cell<usize>>,
}

impl std::io::Read for Counting<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let p = self.pos.get();
        let n = buf.len().min(self.data.len() - p).min(4096);
        buf[..n].copy_from_slice(&self.data[p..p + n]);
        self.pos.set(p + n);
        Ok(n)
    }
}

#[test]
fn parsers_emit_incrementally() {
    let mut xml = String::from("<lists><list path=\"x\">");
    for i in 0..20_000 {
        write!(
            xml,
            "<entry kind=\"file\"><name>f{i}</name><size>1</size><commit revision=\"1\"/></entry>"
        )
        .unwrap();
    }
    xml.push_str("</list></lists>");
    let pos = std::rc::Rc::new(std::cell::Cell::new(0));
    let r = std::io::BufReader::with_capacity(
        4096,
        Counting {
            data: xml.as_bytes(),
            pos: pos.clone(),
        },
    );
    let mut first_seen_at = None;
    parse::list(r, |_| {
        first_seen_at.get_or_insert(pos.get());
    })
    .unwrap();
    // The first entry arrived after reading only the first block.
    assert!(first_seen_at.unwrap() <= 8192, "{first_seen_at:?}");
}

#[test]
fn log_entry_with_50k_paths() {
    const N: usize = 50_000;
    let mut xml = String::with_capacity(N * 120);
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<log>\n<logentry\n   revision=\"42\">\n<author>importer</author>\n<date>2026-10-15T18:45:04.289621Z</date>\n<paths>\n");
    for i in 0..N {
        write!(
            xml,
            "<path\n   text-mods=\"true\"\n   kind=\"file\"\n   action=\"A\"\n   prop-mods=\"false\">/trunk/import/d{}/f{i}.txt</path>\n",
            i / 1000
        )
        .unwrap();
    }
    xml.push_str("</paths>\n<msg>Huge import</msg>\n</logentry>\n</log>\n");

    let t = Instant::now();
    let mut entries = Vec::new();
    parse::log(std::io::BufReader::new(xml.as_bytes()), |e| entries.push(e)).unwrap();
    let d = t.elapsed();
    assert_eq!(entries.len(), 1);
    let e = &entries[0];
    assert_eq!(e.rev, 42);
    assert_eq!(e.message, "Huge import");
    assert_eq!(e.changed_paths.len(), N);
    for (i, p) in e.changed_paths.iter().enumerate() {
        assert_eq!(p.path, format!("/trunk/import/d{}/f{i}.txt", i / 1000));
        assert_eq!(p.action, ChangeAction::Added);
        assert!(p.text_mods && !p.prop_mods);
    }
    check_time("log 50k paths", d, Duration::from_millis(500));
}

#[test]
fn diff_200k_lines() {
    const FILES: usize = 100;
    const LINES: usize = 2_000;
    let mut s = String::new();
    for f in 0..FILES {
        write!(
            s,
            "Index: f{f}.txt\n===================================================================\n--- f{f}.txt\t(revision 1)\n+++ f{f}.txt\t(revision 2)\n@@ -1,{LINES} +1,{LINES} @@\n"
        )
        .unwrap();
        for i in 0..LINES / 2 {
            write!(s, "-old {i}\n+new {i}\n").unwrap();
        }
    }
    let t = Instant::now();
    let mut files = 0;
    let mut lines = 0;
    parse::diff(std::io::BufReader::new(s.as_bytes()), |fd| {
        files += 1;
        if let DiffContent::Text(h) = &fd.content {
            lines += h[0].lines.len();
            assert_eq!(h[0].lines.last().unwrap().new_no, Some(LINES as u64 / 2));
        }
    })
    .unwrap();
    let d = t.elapsed();
    assert_eq!(files, FILES);
    assert_eq!(lines, FILES * LINES);
    check_time("diff 200k lines", d, Duration::from_millis(1000));
}
