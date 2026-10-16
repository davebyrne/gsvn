//! `svn log --xml [-v]`.
//!
//! ```xml
//! <log>
//! <logentry revision="5">
//! <author>alice</author>
//! <date>…</date>
//! <paths>
//! <path action="A" kind="dir" text-mods="false" prop-mods="false"
//!       copyfrom-path="/trunk" copyfrom-rev="4">/branches/feature-1</path>
//! </paths>
//! <msg>Branch feature-1</msg>
//! </logentry>
//! </log>
//! ```
//!
//! The message is kept exactly as svn prints it (leading/trailing newlines
//! included); a missing `<msg>` gives an empty message. Nested
//! `<logentry>`s (from `-g`) are skipped. A `<path>` with an unknown
//! `action` is skipped.

use super::xml::{Ev, Tag, XmlParser, attr_with, each_attr, non_empty, parse_date, parse_u64};
use super::{ParseError, parse_kind};
use crate::model::*;
use std::io::BufRead;

/// Attributes of the `<path>` element being read.
struct PathAttrs {
    action: Option<ChangeAction>,
    kind: Option<NodeKind>,
    copy_path: Option<String>,
    copy_rev: Option<RevNum>,
    text_mods: bool,
    prop_mods: bool,
}

pub(super) fn parse<R: BufRead>(r: R, mut emit: impl FnMut(LogEntry)) -> Result<(), ParseError> {
    let mut p = XmlParser::new(r, "svn log");
    let mut cur: Option<LogEntry> = None;
    // Nesting level of <logentry> (> 1 inside `-g` merged revisions).
    let mut depth = 0usize;
    let mut path_attrs: Option<PathAttrs> = None;
    loop {
        match p.next()? {
            Ev::Start { tag, parent, elem } => match tag {
                Tag::LogEntry => {
                    depth += 1;
                    if depth == 1 && parent == Tag::Log {
                        let Some(rev) = attr_with(&elem, "revision", parse_u64)? else {
                            return Err(p.error("<logentry> without a valid revision"));
                        };
                        cur = Some(LogEntry {
                            rev,
                            author: None,
                            date: None,
                            message: String::new(),
                            changed_paths: Vec::new(),
                        });
                    }
                }
                Tag::Path if parent == Tag::Paths && depth == 1 => {
                    let mut a = PathAttrs {
                        action: None,
                        kind: None,
                        copy_path: None,
                        copy_rev: None,
                        text_mods: false,
                        prop_mods: false,
                    };
                    each_attr(&elem, |k, v| match k {
                        "action" => a.action = parse_action(v),
                        "kind" => a.kind = parse_kind(Some(v)),
                        "copyfrom-path" => a.copy_path = Some(v.to_owned()),
                        "copyfrom-rev" => a.copy_rev = parse_u64(v),
                        "text-mods" => a.text_mods = v == "true",
                        "prop-mods" => a.prop_mods = v == "true",
                        _ => {}
                    })?;
                    path_attrs = Some(a);
                }
                _ => {}
            },
            Ev::End { tag, parent } => {
                if tag == Tag::LogEntry {
                    depth = depth.saturating_sub(1);
                    if depth == 0
                        && let Some(e) = cur.take()
                    {
                        emit(e);
                    }
                    continue;
                }
                if depth != 1 {
                    continue;
                }
                let Some(e) = cur.as_mut() else { continue };
                match (tag, parent) {
                    (Tag::Author, Tag::LogEntry) => e.author = non_empty(p.take_text()),
                    (Tag::Date, Tag::LogEntry) => e.date = parse_date(p.text()),
                    (Tag::Msg, Tag::LogEntry) => e.message = p.take_text(),
                    (Tag::Path, Tag::Paths) => {
                        if let Some(a) = path_attrs.take()
                            && let Some(action) = a.action
                        {
                            e.changed_paths.push(ChangedPath {
                                action,
                                path: p.take_text(),
                                kind: a.kind,
                                copy_from: a.copy_path.zip(a.copy_rev),
                                text_mods: a.text_mods,
                                prop_mods: a.prop_mods,
                            });
                        }
                    }
                    _ => {}
                }
            }
            Ev::Other => {}
            Ev::Eof => return Ok(()),
        }
    }
}

fn parse_action(s: &str) -> Option<ChangeAction> {
    match s.trim() {
        "A" => Some(ChangeAction::Added),
        "M" => Some(ChangeAction::Modified),
        "D" => Some(ChangeAction::Deleted),
        "R" => Some(ChangeAction::Replaced),
        _ => None,
    }
}
