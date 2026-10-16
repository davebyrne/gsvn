//! Small pull-parser layer over `quick-xml` shared by the XML parsers.
//!
//! Tracks the element stack as a cheap [`Tag`] enum (no allocation per
//! element), accumulates the unescaped text of the current element, and
//! turns quick-xml errors and truncated documents into [`ParseError`]s with
//! the byte position.

use super::ParseError;
use jiff::Timestamp;
use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};
use std::io::BufRead;

/// Element names the parsers care about. Everything else is `Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tag {
    // info
    Info,
    Url,
    RelativeUrl,
    Repository,
    Root,
    Uuid,
    // shared
    Entry,
    Commit,
    Author,
    Date,
    Lock,
    // list
    Lists,
    List,
    Name,
    Size,
    // log
    Log,
    LogEntry,
    Paths,
    Path,
    Msg,
    // blame / proplist
    Blame,
    Target,
    Properties,
    Property,
    Other,
    /// Pseudo-tag used as the parent of the root element.
    None,
}

impl Tag {
    fn from_name(name: &str) -> Tag {
        match name {
            "info" => Tag::Info,
            "url" => Tag::Url,
            "relative-url" => Tag::RelativeUrl,
            "repository" => Tag::Repository,
            "root" => Tag::Root,
            "uuid" => Tag::Uuid,
            "entry" => Tag::Entry,
            "commit" => Tag::Commit,
            "author" => Tag::Author,
            "date" => Tag::Date,
            "lock" => Tag::Lock,
            "lists" => Tag::Lists,
            "list" => Tag::List,
            "name" => Tag::Name,
            "size" => Tag::Size,
            "log" => Tag::Log,
            "logentry" => Tag::LogEntry,
            "paths" => Tag::Paths,
            "path" => Tag::Path,
            "msg" => Tag::Msg,
            "blame" => Tag::Blame,
            "target" => Tag::Target,
            "properties" => Tag::Properties,
            "property" => Tag::Property,
            _ => Tag::Other,
        }
    }
}

pub(super) enum Ev<'b> {
    Start {
        tag: Tag,
        parent: Tag,
        elem: BytesStart<'b>,
    },
    /// Element closed. Its text content is available via [`XmlParser::text`]
    /// / [`XmlParser::take_text`] until the next call to `next`.
    End { tag: Tag, parent: Tag },
    /// Text, comments, declarations: nothing for the caller to do.
    Other,
    Eof,
}

pub(super) struct XmlParser<R: BufRead> {
    reader: Reader<R>,
    buf: Vec<u8>,
    stack: Vec<Tag>,
    text: String,
    what: &'static str,
    seen_root: bool,
}

impl<R: BufRead> XmlParser<R> {
    /// `what` names the command for error messages (e.g. "svn log").
    pub(super) fn new(r: R, what: &'static str) -> Self {
        let mut reader = Reader::from_reader(r);
        let cfg = reader.config_mut();
        cfg.expand_empty_elements = true;
        cfg.trim_text(false);
        cfg.check_end_names = true;
        Self {
            reader,
            buf: Vec::with_capacity(4096),
            stack: Vec::with_capacity(16),
            text: String::new(),
            what,
            seen_root: false,
        }
    }

    pub(super) fn error(&self, msg: impl std::fmt::Display) -> ParseError {
        ParseError(format!(
            "{} output: {msg} (near byte {})",
            self.what,
            self.reader.buffer_position()
        ))
    }

    /// Text content of the element that was just closed (or accumulated so
    /// far in the current element).
    pub(super) fn text(&self) -> &str {
        &self.text
    }

    pub(super) fn take_text(&mut self) -> String {
        std::mem::take(&mut self.text)
    }

    /// Next structural event. Text, references, comments etc. are folded
    /// into the current element's text and reported as [`Ev::Other`].
    pub(super) fn next(&mut self) -> Result<Ev<'_>, ParseError> {
        let what = self.what;
        self.buf.clear();
        let ev = match self.reader.read_event_into(&mut self.buf) {
            Ok(ev) => ev,
            Err(e) => {
                return Err(ParseError(format!(
                    "{what} output: malformed XML at byte {}: {e}",
                    self.reader.error_position()
                )));
            }
        };
        match ev {
            Event::Start(elem) => {
                let tag = Tag::from_name(elem.local_name().as_ref());
                let parent = self.stack.last().copied().unwrap_or(Tag::None);
                if parent == Tag::None {
                    if self.seen_root {
                        return Err(ParseError(format!(
                            "{what} output: more than one root element"
                        )));
                    }
                    self.seen_root = true;
                }
                self.stack.push(tag);
                self.text.clear();
                Ok(Ev::Start { tag, parent, elem })
            }
            Event::End(_) => {
                let tag = self.stack.pop().unwrap_or(Tag::None);
                let parent = self.stack.last().copied().unwrap_or(Tag::None);
                Ok(Ev::End { tag, parent })
            }
            Event::Text(t) => {
                self.text.push_str(&t.xml10_content());
                Ok(Ev::Other)
            }
            Event::CData(c) => {
                self.text.push_str(&c);
                Ok(Ev::Other)
            }
            Event::GeneralRef(r) => {
                let ch = r.resolve_char_ref().map_err(|e| {
                    ParseError(format!("{what} output: bad character reference: {e}"))
                })?;
                if let Some(ch) = ch {
                    self.text.push(ch);
                } else if let Some(s) = quick_xml::escape::resolve_predefined_entity(&r) {
                    self.text.push_str(s);
                } else {
                    let name: &str = &r;
                    return Err(ParseError(format!("{what} output: unknown entity &{name};")));
                }
                Ok(Ev::Other)
            }
            Event::Eof => {
                let pos = self.reader.buffer_position();
                if !self.stack.is_empty() {
                    Err(ParseError(format!(
                        "{what} output: unexpected end of output (truncated XML) at byte {pos}"
                    )))
                } else if !self.seen_root {
                    Err(ParseError(format!("{what} output: no XML document")))
                } else {
                    Ok(Ev::Eof)
                }
            }
            Event::Empty(_)
            | Event::Comment(_)
            | Event::Decl(_)
            | Event::PI(_)
            | Event::DocType(_) => Ok(Ev::Other),
        }
    }
}

/// Value of attribute `key` (unescaped), if present.
pub(super) fn attr(elem: &BytesStart<'_>, key: &str) -> Result<Option<String>, ParseError> {
    for a in elem.attributes() {
        let a = a.map_err(|e| ParseError(format!("malformed attribute: {e}")))?;
        if a.key.local_name().as_ref() == key {
            let v = a
                .normalized_value(XmlVersion::Implicit1_0)
                .map_err(|e| ParseError(format!("bad attribute value for {key}: {e}")))?;
            return Ok(Some(v.into_owned()));
        }
    }
    Ok(None)
}

pub(super) fn parse_u64(s: &str) -> Option<u64> {
    s.trim().parse().ok()
}

/// svn's `2026-10-15T18:45:04.289621Z`. Unparseable dates become `None`.
pub(super) fn parse_date(s: &str) -> Option<Timestamp> {
    s.trim().parse().ok()
}

/// `None` for an empty string (e.g. `<author></author>`).
pub(super) fn non_empty(s: String) -> Option<String> {
    if s.is_empty() { None } else { Some(s) }
}
