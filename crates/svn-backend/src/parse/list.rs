//! `svn list --xml`.
//!
//! ```xml
//! <lists>
//! <list path="URL">
//! <entry kind="file">
//! <name>a&amp;b.txt</name>
//! <size>9</size>
//! <commit revision="6"><author>bob</author><date>…</date></commit>
//! [<lock>…</lock>]
//! </entry>
//! </list>
//! [<list …>…</list>]
//! </lists>
//! ```
//!
//! Entries of all `<list>`s are emitted in document order. A bare `<list>`
//! root (without `<lists>`) is accepted too.

use super::xml::{Ev, Tag, XmlParser, attr, non_empty, parse_date, parse_u64};
use super::{ParseError, parse_kind};
use crate::model::*;
use std::io::BufRead;

struct Acc {
    kind: Option<NodeKind>,
    name: Option<String>,
    size: Option<u64>,
    commit: CommitInfo,
}

pub(super) fn parse<R: BufRead>(r: R, mut emit: impl FnMut(Entry)) -> Result<(), ParseError> {
    let mut p = XmlParser::new(r, "svn list");
    let mut cur: Option<Acc> = None;
    loop {
        match p.next()? {
            Ev::Start { tag, parent, elem } => match (tag, parent) {
                (Tag::Entry, Tag::List) => {
                    let kind = attr(&elem, "kind")?;
                    cur = Some(Acc {
                        kind: parse_kind(kind.as_deref()),
                        name: None,
                        size: None,
                        commit: CommitInfo {
                            rev: 0,
                            author: None,
                            date: None,
                        },
                    });
                }
                (Tag::Commit, Tag::Entry) => {
                    if let Some(c) = cur.as_mut() {
                        c.commit.rev = attr(&elem, "revision")?
                            .as_deref()
                            .and_then(parse_u64)
                            .unwrap_or(0);
                    }
                }
                _ => {}
            },
            Ev::End { tag, parent } => {
                let Some(c) = cur.as_mut() else { continue };
                match (tag, parent) {
                    (Tag::Name, Tag::Entry) => c.name = Some(p.take_text()),
                    (Tag::Size, Tag::Entry) => c.size = parse_u64(p.text()),
                    (Tag::Author, Tag::Commit) => c.commit.author = non_empty(p.take_text()),
                    (Tag::Date, Tag::Commit) => c.commit.date = parse_date(p.text()),
                    (Tag::Entry, Tag::List) => {
                        let Acc {
                            kind,
                            name,
                            size,
                            commit,
                        } = cur.take().unwrap();
                        let Some(name) = name else {
                            return Err(p.error("<entry> without <name>"));
                        };
                        let Some(kind) = kind else {
                            return Err(p.error(format!("entry {name:?} has no valid kind")));
                        };
                        emit(Entry {
                            name,
                            kind,
                            size: if kind == NodeKind::File { size } else { None },
                            last_changed: commit,
                        });
                    }
                    _ => {}
                }
            }
            Ev::Other => {}
            Ev::Eof => return Ok(()),
        }
    }
}
