//! `svn info --xml`.
//!
//! ```xml
//! <info>
//! <entry kind="dir" path="trunk" revision="1939142" [size="N"]>
//! <url>https://…/trunk</url>
//! <relative-url>^/subversion/trunk</relative-url>
//! <repository><root>…</root><uuid>…</uuid></repository>
//! <commit revision="1939086"><author>…</author><date>…</date></commit>
//! [<lock>…</lock>] [<wc-info>…</wc-info>]
//! </entry>
//! </info>
//! ```

use super::xml::{Ev, Tag, XmlParser, attr, non_empty, parse_date, parse_u64};
use super::{ParseError, parse_kind};
use crate::model::*;
use crate::url::{RepoUrl, decode};
use std::io::BufRead;

#[derive(Default)]
struct Acc {
    kind: Option<String>,
    size: Option<u64>,
    rev: Option<u64>,
    url: Option<String>,
    relative_url: Option<String>,
    root: Option<String>,
    uuid: Option<String>,
    commit_rev: Option<u64>,
    author: Option<String>,
    date: Option<jiff::Timestamp>,
}

pub(super) fn parse<R: BufRead>(r: R) -> Result<Vec<Info>, ParseError> {
    let mut p = XmlParser::new(r, "svn info");
    let mut out = Vec::new();
    let mut cur: Option<Acc> = None;
    loop {
        match p.next()? {
            Ev::Start { tag, parent, elem } => match (tag, parent) {
                (Tag::Entry, Tag::Info) => {
                    cur = Some(Acc {
                        kind: attr(&elem, "kind")?,
                        size: attr(&elem, "size")?.as_deref().and_then(parse_u64),
                        rev: attr(&elem, "revision")?.as_deref().and_then(parse_u64),
                        ..Acc::default()
                    });
                }
                (Tag::Commit, Tag::Entry) => {
                    if let Some(c) = cur.as_mut() {
                        c.commit_rev = attr(&elem, "revision")?.as_deref().and_then(parse_u64);
                    }
                }
                _ => {}
            },
            Ev::End { tag, parent } => {
                if cur.is_none() {
                    continue;
                }
                match (tag, parent) {
                    (Tag::Url, Tag::Entry) => cur.as_mut().unwrap().url = Some(p.take_text()),
                    (Tag::RelativeUrl, Tag::Entry) => {
                        cur.as_mut().unwrap().relative_url = Some(p.take_text())
                    }
                    (Tag::Root, Tag::Repository) => cur.as_mut().unwrap().root = Some(p.take_text()),
                    (Tag::Uuid, Tag::Repository) => cur.as_mut().unwrap().uuid = Some(p.take_text()),
                    (Tag::Author, Tag::Commit) => {
                        cur.as_mut().unwrap().author = non_empty(p.take_text())
                    }
                    (Tag::Date, Tag::Commit) => cur.as_mut().unwrap().date = parse_date(p.text()),
                    (Tag::Entry, Tag::Info) => {
                        let acc = cur.take().unwrap();
                        out.push(finish(acc).map_err(|m| p.error(m))?);
                    }
                    _ => {}
                }
            }
            Ev::Other => {}
            Ev::Eof => return Ok(out),
        }
    }
}

fn finish(a: Acc) -> Result<Info, String> {
    let url_s = a.url.ok_or("entry without <url>")?;
    let url = RepoUrl::parse(&url_s).map_err(|e| e.to_string())?;
    let root_s = a.root.ok_or("entry without repository <root>")?;
    let root = RepoUrl::parse(&root_s).map_err(|e| e.to_string())?;
    let rev = a.rev.ok_or("entry without revision")?;
    let kind = parse_kind(a.kind.as_deref()).ok_or("entry with missing or unknown kind")?;
    let repos_path = match a.relative_url {
        Some(rel) => {
            let p = decode(rel.trim().strip_prefix('^').unwrap_or(rel.trim()));
            if p.is_empty() { "/".to_owned() } else { p }
        }
        None => url
            .path_from(&root)
            .ok_or_else(|| format!("url {url} is not under root {root}"))?,
    };
    Ok(Info {
        url,
        root,
        uuid: a.uuid.unwrap_or_default(),
        repos_path,
        rev,
        kind,
        last_changed: CommitInfo {
            rev: a.commit_rev.unwrap_or(0),
            author: a.author,
            date: a.date,
        },
        size: if kind == NodeKind::File { a.size } else { None },
    })
}
