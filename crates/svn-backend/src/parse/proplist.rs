//! `svn proplist -v --xml` (one target).
//!
//! ```xml
//! <properties>
//! <target path="URL">
//! <property name="svn:ignore">*.o
//! *.tmp
//! </property>
//! <property name="my:binary" encoding="base64">YmluAWFyeQI=
//! </property>
//! </target>
//! </properties>
//! ```
//!
//! Values are kept exactly (multi-line, trailing newline included).
//! svn uses `encoding="base64"` for values that aren't valid XML text
//! (control characters, invalid UTF-8): those are **decoded**, and the bytes
//! are converted to a `String` lossily (invalid UTF-8 → U+FFFD; control
//! characters are kept). If the base64 itself is invalid the raw text is
//! kept. Properties of all targets (and `<revprops>`) are returned in
//! document order.

use super::ParseError;
use super::xml::{Ev, Tag, XmlParser, attr};
use crate::model::*;
use std::io::BufRead;

pub(super) fn parse<R: BufRead>(r: R) -> Result<Vec<Property>, ParseError> {
    let mut p = XmlParser::new(r, "svn proplist");
    let mut out = Vec::new();
    // (name, is_base64) of the <property> being read.
    let mut cur: Option<(String, bool)> = None;
    loop {
        match p.next()? {
            Ev::Start {
                tag: Tag::Property,
                elem,
                ..
            } => {
                let Some(name) = attr(&elem, "name")? else {
                    return Err(p.error("<property> without name"));
                };
                let b64 = attr(&elem, "encoding")?.as_deref() == Some("base64");
                cur = Some((name, b64));
            }
            Ev::End {
                tag: Tag::Property,
                ..
            } => {
                if let Some((name, b64)) = cur.take() {
                    let raw = p.take_text();
                    let value = if b64 {
                        match base64_decode(&raw) {
                            Some(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                            None => raw,
                        }
                    } else {
                        raw
                    };
                    out.push(Property { name, value });
                }
            }
            Ev::Eof => return Ok(out),
            _ => {}
        }
    }
}

/// Standard base64 (RFC 4648) with padding; whitespace is ignored.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0u32;
    let mut padding = false;
    for &c in s.as_bytes() {
        if c.is_ascii_whitespace() {
            continue;
        }
        if c == b'=' {
            padding = true;
            continue;
        }
        if padding {
            return None; // data after padding
        }
        acc = (acc << 6) | val(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::base64_decode;

    #[test]
    fn base64() {
        assert_eq!(base64_decode("YmluAWFyeQI=\n").unwrap(), b"bin\x01ary\x02");
        assert_eq!(base64_decode("").unwrap(), b"");
        assert_eq!(base64_decode("Zm9v").unwrap(), b"foo");
        assert_eq!(base64_decode("Zm9vYg==").unwrap(), b"foob");
        assert_eq!(base64_decode("Zm9v\nYmE=").unwrap(), b"fooba");
        assert!(base64_decode("Zm9v!").is_none());
        assert!(base64_decode("Zg==Zg").is_none());
    }
}
