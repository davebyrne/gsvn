//! `svn blame --xml`.
//!
//! ```xml
//! <blame>
//! <target path="URL">
//! <entry line-number="1">
//! <commit revision="3"><author>carol</author><date>…</date></commit>
//! </entry>
//! <entry line-number="2">
//! </entry>                       <!-- line older than the -r range start -->
//! </target>
//! </blame>
//! ```
//!
//! An entry without `<commit>` gives `commit: None`. Merge info (`-g`,
//! `<merged>`) is ignored.

use super::ParseError;
use super::xml::{Ev, Tag, XmlParser, attr, non_empty, parse_date, parse_u64};
use crate::model::*;
use std::io::BufRead;

pub(super) fn parse<R: BufRead>(r: R, mut emit: impl FnMut(BlameLine)) -> Result<(), ParseError> {
    let mut p = XmlParser::new(r, "svn blame");
    let mut cur: Option<BlameLine> = None;
    // Inside the entry's own <commit> (not one under <merged>).
    let mut in_commit = false;
    loop {
        match p.next()? {
            Ev::Start { tag, parent, elem } => match (tag, parent) {
                (Tag::Entry, Tag::Target) => {
                    let Some(line_no) = attr(&elem, "line-number")?.as_deref().and_then(parse_u64)
                    else {
                        return Err(p.error("<entry> without a valid line-number"));
                    };
                    cur = Some(BlameLine {
                        line_no,
                        commit: None,
                    });
                }
                (Tag::Commit, Tag::Entry) => {
                    if let Some(l) = cur.as_mut() {
                        in_commit = true;
                        let rev = attr(&elem, "revision")?.as_deref().and_then(parse_u64);
                        l.commit = Some(CommitInfo {
                            rev: rev.unwrap_or(0),
                            author: None,
                            date: None,
                        });
                    }
                }
                _ => {}
            },
            Ev::End { tag, parent } => {
                let Some(l) = cur.as_mut() else { continue };
                match (tag, parent) {
                    (Tag::Commit, Tag::Entry) => in_commit = false,
                    (Tag::Author, Tag::Commit) if in_commit => {
                        if let Some(c) = l.commit.as_mut() {
                            c.author = non_empty(p.take_text());
                        }
                    }
                    (Tag::Date, Tag::Commit) if in_commit => {
                        if let Some(c) = l.commit.as_mut() {
                            c.date = parse_date(p.text());
                        }
                    }
                    (Tag::Entry, Tag::Target) => emit(cur.take().unwrap()),
                    _ => {}
                }
            }
            Ev::Other => {}
            Ev::Eof => return Ok(()),
        }
    }
}
