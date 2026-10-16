//! Turning `svn cat` bytes into text for the viewer.
//!
//! UTF-8 is tried first. Anything else is decoded as Windows-1252 (a
//! superset of ISO-8859-1 for printable characters), which never fails, so
//! legacy 8-bit files are readable rather than shown as replacement
//! characters. Line endings are left untouched.

/// The encoding a file was decoded with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    /// UTF-8 with a byte-order mark (stripped from the text).
    Utf8Bom,
    /// Not valid UTF-8; uses Windows-1252 characters in 0x80–0x9F.
    Windows1252,
    /// Not valid UTF-8; plain ISO-8859-1 (Latin-1).
    Latin1,
}

impl Encoding {
    pub fn label(self) -> &'static str {
        match self {
            Encoding::Utf8 => "UTF-8",
            Encoding::Utf8Bom => "UTF-8 with BOM",
            Encoding::Windows1252 => "Windows-1252",
            Encoding::Latin1 => "ISO-8859-1",
        }
    }

    /// True for the fallback encodings (the file is not valid UTF-8).
    pub fn is_fallback(self) -> bool {
        matches!(self, Encoding::Windows1252 | Encoding::Latin1)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded {
    pub text: String,
    pub encoding: Encoding,
    /// The text contains CRLF line endings.
    pub crlf: bool,
    /// Number of lines as svn counts them (a trailing newline does not
    /// start another line).
    pub lines: usize,
}

/// Windows-1252 code points for bytes 0x80–0x9F; `None` where the code page
/// leaves a byte undefined (those map to the Latin-1 C1 control instead).
const CP1252_HIGH: [Option<char>; 32] = [
    Some('\u{20AC}'),
    None,
    Some('\u{201A}'),
    Some('\u{0192}'),
    Some('\u{201E}'),
    Some('\u{2026}'),
    Some('\u{2020}'),
    Some('\u{2021}'),
    Some('\u{02C6}'),
    Some('\u{2030}'),
    Some('\u{0160}'),
    Some('\u{2039}'),
    Some('\u{0152}'),
    None,
    Some('\u{017D}'),
    None,
    None,
    Some('\u{2018}'),
    Some('\u{2019}'),
    Some('\u{201C}'),
    Some('\u{201D}'),
    Some('\u{2022}'),
    Some('\u{2013}'),
    Some('\u{2014}'),
    Some('\u{02DC}'),
    Some('\u{2122}'),
    Some('\u{0161}'),
    Some('\u{203A}'),
    Some('\u{0153}'),
    None,
    Some('\u{017E}'),
    Some('\u{0178}'),
];

/// Decode file contents. Returns `None` if the data contains a NUL byte
/// anywhere (GtkTextBuffer cannot hold it; such a file is treated as
/// binary even if the first 8 KB looked like text).
pub fn decode(data: &[u8]) -> Option<Decoded> {
    if data.contains(&0) {
        return None;
    }
    let (text, encoding) = match std::str::from_utf8(data) {
        Ok(s) => match s.strip_prefix('\u{FEFF}') {
            Some(rest) => (rest.to_owned(), Encoding::Utf8Bom),
            None => (s.to_owned(), Encoding::Utf8),
        },
        Err(_) => decode_8bit(data),
    };
    let crlf = text.contains("\r\n");
    let lines = count_lines(&text);
    Some(Decoded {
        text,
        encoding,
        crlf,
        lines,
    })
}

fn decode_8bit(data: &[u8]) -> (String, Encoding) {
    let mut text = String::with_capacity(data.len() + data.len() / 8);
    let mut cp1252 = false;
    for &b in data {
        let c = match b {
            0x80..=0x9F => match CP1252_HIGH[usize::from(b - 0x80)] {
                Some(c) => {
                    cp1252 = true;
                    c
                }
                None => char::from(b),
            },
            _ => char::from(b),
        };
        text.push(c);
    }
    let encoding = if cp1252 {
        Encoding::Windows1252
    } else {
        Encoding::Latin1
    };
    (text, encoding)
}

/// Lines as `svn blame` counts them: `\n`, `\r\n` and a lone `\r` end a
/// line; text after the last line ending is one more line.
pub fn count_lines(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut n = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => n += 1,
            b'\r' => {
                n += 1;
                if bytes.get(i + 1) == Some(&b'\n') {
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    if !bytes.is_empty() && !matches!(bytes[bytes.len() - 1], b'\n' | b'\r') {
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8() {
        let d = decode("grüße\nwelt\n".as_bytes()).unwrap();
        assert_eq!(d.encoding, Encoding::Utf8);
        assert_eq!(d.text, "grüße\nwelt\n");
        assert_eq!(d.lines, 2);
        assert!(!d.crlf);
    }

    #[test]
    fn bom_is_stripped() {
        let d = decode(b"\xEF\xBB\xBFhi").unwrap();
        assert_eq!(d.encoding, Encoding::Utf8Bom);
        assert_eq!(d.text, "hi");
    }

    #[test]
    fn latin1_fallback() {
        let d = decode(b"gr\xFC\xDFe\n").unwrap();
        assert_eq!(d.encoding, Encoding::Latin1);
        assert_eq!(d.text, "grüße\n");
        assert!(d.encoding.is_fallback());
    }

    #[test]
    fn windows1252_fallback() {
        let d = decode(b"\x93quoted\x94 \x80 \x81").unwrap();
        assert_eq!(d.encoding, Encoding::Windows1252);
        assert_eq!(d.text, "\u{201C}quoted\u{201D} \u{20AC} \u{81}");
    }

    #[test]
    fn crlf_kept() {
        let d = decode(b"a\r\nb\r\n").unwrap();
        assert!(d.crlf);
        assert_eq!(d.text, "a\r\nb\r\n");
        assert_eq!(d.lines, 2);
    }

    #[test]
    fn nul_is_binary() {
        assert!(decode(b"text\0more").is_none());
    }

    #[test]
    fn line_counting() {
        assert_eq!(count_lines(""), 0);
        assert_eq!(count_lines("a"), 1);
        assert_eq!(count_lines("a\n"), 1);
        assert_eq!(count_lines("a\nb"), 2);
        assert_eq!(count_lines("\n\n"), 2);
        assert_eq!(count_lines("a\rb\r\nc"), 3);
    }
}
