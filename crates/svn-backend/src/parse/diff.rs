//! `svn diff --internal-diff` unified output.
//!
//! ```text
//! Index: trunk/hello.txt
//! ===================================================================
//! --- trunk/hello.txt	(revision 2)
//! +++ trunk/hello.txt	(revision 3)
//! @@ -1,6 +1,6 @@ optional section
//!  context
//! -removed
//! +added
//! \ No newline at end of file
//!
//! Property changes on: trunk/hello.txt
//! ___________________________________________________________________
//! Modified: my:prop
//! ## -1 +1 ##
//! -old
//! \ No newline at end of property
//! +new
//! \ No newline at end of property
//! ```
//!
//! The input is read line by line with a reused buffer; each file section
//! is emitted when the next `Index:` line (or EOF) is reached.
//!
//! Hunk bodies are consumed **by count** using the `@@` header, so content
//! lines that look like headers (`--- x`, `+++ x`, `Index: x`, `@@ …`)
//! never confuse the parser.
//!
//! Decisions:
//! - Each line is decoded lossily (invalid UTF-8 → U+FFFD).
//! - Line endings: the trailing `\n` is removed and then **one** trailing
//!   `\r`, so `DiffLine::text` never ends in `\r` even for CRLF files (the
//!   CRLF-ness of a file is not represented).
//! - Property values are reassembled from the `## … ##` hunk lines: each
//!   line contributes `text + "\n"`, and `\ No newline at end of property`
//!   removes the final `\n`. Context lines (` x`, e.g. svn's
//!   `   Merged /branch:r1-3` lines for `svn:mergeinfo`) go to both sides.
//!   The pre-1.7 format (`   + value` / `   - value`) is understood too.
//! - Anything unrecognised outside a hunk is ignored, so trailing garbage
//!   and text before the first `Index:` are tolerated.

use super::ParseError;
use crate::model::*;
use std::io::BufRead;

/// Which side of a property change the last value line went to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Old,
    New,
    Both,
}

#[derive(Default)]
struct State {
    file: Option<FileDiff>,
    /// Remaining old/new lines of the current text hunk.
    hunk_old: u64,
    hunk_new: u64,
    /// Next old/new line number in the current text hunk.
    old_no: u64,
    new_no: u64,
    /// Inside a `Property changes on:` block.
    in_props: bool,
    /// Remaining old/new lines of the current `## … ##` property hunk.
    prop_old: u64,
    prop_new: u64,
    /// Last property value line target (for `\ No newline at end of property`
    /// and old-style continuation lines).
    prop_side: Option<Side>,
    /// Old-style (`   + value`) value being read.
    old_style: bool,
}

pub(super) fn parse<R: BufRead>(mut r: R, mut emit: impl FnMut(FileDiff)) -> Result<(), ParseError> {
    let mut buf = Vec::with_capacity(256);
    let mut st = State::default();
    loop {
        buf.clear();
        let n = r
            .read_until(b'\n', &mut buf)
            .map_err(|e| ParseError(format!("reading svn diff output: {e}")))?;
        if n == 0 {
            break;
        }
        let mut line: &[u8] = &buf;
        if let Some(l) = line.strip_suffix(b"\n") {
            line = l;
        }
        if let Some(l) = line.strip_suffix(b"\r") {
            line = l;
        }
        st.line(line, &mut emit);
    }
    if let Some(f) = st.file.take() {
        emit(f);
    }
    Ok(())
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

impl State {
    fn line(&mut self, line: &[u8], emit: &mut impl FnMut(FileDiff)) {
        if self.hunk_old > 0 || self.hunk_new > 0 {
            if self.hunk_line(line) {
                return;
            }
            // Malformed hunk: drop the counts and treat as a header line.
            self.hunk_old = 0;
            self.hunk_new = 0;
        }
        if self.prop_old > 0 || self.prop_new > 0 {
            if self.prop_hunk_line(line) {
                return;
            }
            self.prop_old = 0;
            self.prop_new = 0;
        }

        if let Some(path) = line.strip_prefix(b"Index: ") {
            if let Some(f) = self.file.take() {
                emit(f);
            }
            *self = State::default();
            self.file = Some(FileDiff {
                path: lossy(path),
                old_label: None,
                new_label: None,
                content: DiffContent::None,
                prop_changes: Vec::new(),
            });
            return;
        }
        let Some(file) = self.file.as_mut() else {
            return; // before the first Index: line
        };

        if line.starts_with(b"\\ ") {
            // "\ No newline at end of file" / "... of property" after the
            // hunk's counts were used up.
            if line.ends_with(b"property") {
                self.no_newline_prop();
            } else if let DiffContent::Text(hunks) = &mut file.content
                && let Some(l) = hunks.last_mut().and_then(|h| h.lines.last_mut())
            {
                l.no_newline = true;
            }
            return;
        }

        if self.in_props {
            self.props_line(line);
            return;
        }

        if let Some(label) = line.strip_prefix(b"--- ") {
            file.old_label = Some(lossy(label));
        } else if let Some(label) = line.strip_prefix(b"+++ ") {
            file.new_label = Some(lossy(label));
        } else if line.starts_with(b"@@ ") {
            if let Some((os, ol, ns, nl, rest)) = parse_range_header(line, b"@@") {
                let hunk = Hunk {
                    old_start: os,
                    old_len: ol,
                    new_start: ns,
                    new_len: nl,
                    section: lossy(rest.strip_prefix(b" ").unwrap_or(rest)),
                    lines: Vec::with_capacity((ol.max(nl) as usize).min(4096)),
                };
                match &mut file.content {
                    DiffContent::Text(h) => h.push(hunk),
                    c => *c = DiffContent::Text(vec![hunk]),
                }
                self.hunk_old = ol;
                self.hunk_new = nl;
                self.old_no = os;
                self.new_no = ns;
            }
        } else if line.starts_with(b"Cannot display: file marked as a binary type")
            || (line.starts_with(b"Binary files ") && line.ends_with(b" differ"))
        {
            file.content = DiffContent::Binary;
        } else if line.starts_with(b"Property changes on: ") {
            self.in_props = true;
        }
        // "=====", "svn:mime-type = …", blank lines and garbage: ignored.
    }

    /// One line inside a text hunk. Returns false if it doesn't fit.
    fn hunk_line(&mut self, line: &[u8]) -> bool {
        let Some(DiffContent::Text(hunks)) = self.file.as_mut().map(|f| &mut f.content) else {
            return false;
        };
        let Some(hunk) = hunks.last_mut() else {
            return false;
        };
        let (marker, text) = match line.split_first() {
            Some((&m, rest)) => (m, rest),
            // An empty line: some tools strip the space of empty context lines.
            None => (b' ', &line[..0]),
        };
        let (kind, old_no, new_no) = match marker {
            b' ' if self.hunk_old > 0 && self.hunk_new > 0 => {
                self.hunk_old -= 1;
                self.hunk_new -= 1;
                (DiffLineKind::Context, Some(self.old_no), Some(self.new_no))
            }
            b'-' if self.hunk_old > 0 => {
                self.hunk_old -= 1;
                (DiffLineKind::Removed, Some(self.old_no), None)
            }
            b'+' if self.hunk_new > 0 => {
                self.hunk_new -= 1;
                (DiffLineKind::Added, None, Some(self.new_no))
            }
            b'\\' => {
                if let Some(l) = hunk.lines.last_mut() {
                    l.no_newline = true;
                }
                return true;
            }
            _ => return false,
        };
        if old_no.is_some() {
            self.old_no += 1;
        }
        if new_no.is_some() {
            self.new_no += 1;
        }
        hunk.lines.push(DiffLine {
            kind,
            old_no,
            new_no,
            text: lossy(text),
            no_newline: false,
        });
        true
    }

    /// One line inside a `## … ##` property hunk. Returns false if it
    /// doesn't fit.
    fn prop_hunk_line(&mut self, line: &[u8]) -> bool {
        let (side, text) = match line.split_first() {
            Some((b' ', t)) if self.prop_old > 0 && self.prop_new > 0 => {
                self.prop_old -= 1;
                self.prop_new -= 1;
                (Side::Both, t)
            }
            // svn:mergeinfo summaries ("## -0,0 +0,1 ##" followed by
            // "   Merged /x:r3") are counted on one side only.
            Some((b' ', t)) if self.prop_new > 0 => {
                self.prop_new -= 1;
                (Side::New, t)
            }
            Some((b' ', t)) if self.prop_old > 0 => {
                self.prop_old -= 1;
                (Side::Old, t)
            }
            Some((b'-', t)) if self.prop_old > 0 => {
                self.prop_old -= 1;
                (Side::Old, t)
            }
            Some((b'+', t)) if self.prop_new > 0 => {
                self.prop_new -= 1;
                (Side::New, t)
            }
            Some((b'\\', _)) => {
                self.no_newline_prop();
                return true;
            }
            _ => return false,
        };
        let text = String::from_utf8_lossy(text);
        self.append_prop(side, &text, false);
        true
    }

    fn current_prop(&mut self) -> Option<&mut PropChange> {
        self.file.as_mut()?.prop_changes.last_mut()
    }

    /// Append a value line (`text` + "\n") to `side`. With `continuation`
    /// (old-style multi-line values) the line is joined with "\n" instead.
    fn append_prop(&mut self, side: Side, text: &str, continuation: bool) {
        self.prop_side = Some(side);
        let Some(p) = self.current_prop() else {
            return;
        };
        let push =|v: &mut Option<String>| {
            let s = v.get_or_insert_with(String::new);
            if continuation {
                s.push('\n');
                s.push_str(text);
            } else {
                s.push_str(text);
                s.push('\n');
            }
        };
        match side {
            Side::Old => push(&mut p.old),
            Side::New => push(&mut p.new),
            Side::Both => {
                push(&mut p.old);
                push(&mut p.new);
            }
        }
    }

    fn no_newline_prop(&mut self) {
        let Some(side) = self.prop_side else {
            return;
        };
        let Some(p) = self.current_prop() else {
            return;
        };
        let strip = |v: &mut Option<String>| {
            if let Some(s) = v
                && s.ends_with('\n')
            {
                s.pop();
            }
        };
        match side {
            Side::Old => strip(&mut p.old),
            Side::New => strip(&mut p.new),
            Side::Both => {
                strip(&mut p.old);
                strip(&mut p.new);
            }
        }
    }

    /// A line in a `Property changes on:` block, outside a `##` hunk.
    fn props_line(&mut self, line: &[u8]) {
        for (prefix, old, new) in [
            (&b"Added: "[..], None, Some(String::new())),
            (b"Modified: ", Some(String::new()), Some(String::new())),
            (b"Deleted: ", Some(String::new()), None),
            (b"Name: ", None, None),
        ] {
            if let Some(name) = line.strip_prefix(prefix) {
                let file = self.file.as_mut().expect("in_props implies a file");
                file.prop_changes.push(PropChange {
                    name: lossy(name),
                    old,
                    new,
                });
                self.prop_side = None;
                self.old_style = false;
                return;
            }
        }
        if line.starts_with(b"## ") {
            if let Some((_, ol, _, nl, _)) = parse_range_header(line, b"##") {
                self.prop_old = ol;
                self.prop_new = nl;
                self.prop_side = None;
                self.old_style = false;
            }
            return;
        }
        // Pre-1.7 format: "   - old" / "   + new", multi-line values
        // continue on unprefixed lines until a blank line.
        if let Some(v) = line.strip_prefix(b"   - ") {
            self.start_old_style(Side::Old, v);
        } else if let Some(v) = line.strip_prefix(b"   + ") {
            self.start_old_style(Side::New, v);
        } else if self.old_style && !line.is_empty() {
            if let Some(side) = self.prop_side {
                let text = lossy(line);
                self.append_prop(side, &text, true);
            }
        } else if line.is_empty() {
            self.old_style = false;
        }
        // "____" separator and anything else: ignored.
    }

    fn start_old_style(&mut self, side: Side, v: &[u8]) {
        if let Some(p) = self.current_prop() {
            let slot = if side == Side::Old { &mut p.old } else { &mut p.new };
            *slot = Some(lossy(v));
            self.prop_side = Some(side);
            self.old_style = true;
        }
    }
}

/// Parse `@@ -a[,b] +c[,d] @@rest` (or the `##` property variant).
/// Returns `(a, b, c, d, rest)`; a missing length means 1.
fn parse_range_header<'a>(line: &'a [u8], delim: &[u8]) -> Option<(u64, u64, u64, u64, &'a [u8])> {
    let rest = line.strip_prefix(delim)?.strip_prefix(b" -")?;
    let (old, rest) = split_at_byte(rest, b' ')?;
    let rest = rest.strip_prefix(b"+")?;
    let (new, rest) = split_at_byte(rest, b' ')?;
    let rest = rest.strip_prefix(delim)?;
    let (os, ol) = parse_range(old)?;
    let (ns, nl) = parse_range(new)?;
    Some((os, ol, ns, nl, rest))
}

fn split_at_byte(s: &[u8], b: u8) -> Option<(&[u8], &[u8])> {
    let i = s.iter().position(|&c| c == b)?;
    Some((&s[..i], &s[i + 1..]))
}

fn parse_range(s: &[u8]) -> Option<(u64, u64)> {
    let s = std::str::from_utf8(s).ok()?;
    match s.split_once(',') {
        Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
        None => Some((s.parse().ok()?, 1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_headers() {
        assert_eq!(
            parse_range_header(b"@@ -1,6 +1,7 @@ fn x()", b"@@"),
            Some((1, 6, 1, 7, &b" fn x()"[..]))
        );
        assert_eq!(
            parse_range_header(b"@@ -0,0 +1 @@", b"@@"),
            Some((0, 0, 1, 1, &b""[..]))
        );
        assert_eq!(
            parse_range_header(b"## -1 +0,0 ##", b"##"),
            Some((1, 1, 0, 0, &b""[..]))
        );
        assert_eq!(parse_range_header(b"@@ -x +1 @@", b"@@"), None);
        assert_eq!(parse_range_header(b"@@ -1 +1", b"@@"), None);
    }
}
