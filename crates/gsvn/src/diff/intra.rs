//! Intra-line ("word diff") highlighting for a pair of changed lines.
//!
//! Lines are split into tokens — runs of word characters, runs of
//! whitespace, and single punctuation characters — and diffed with
//! `similar`. Single-word lines are diffed per character instead, so
//! `foo1` → `foo2` highlights just the digit.
//!
//! The result is a list of changed **character** ranges per side (GTK text
//! iterators count characters, not bytes). `None` means "don't highlight":
//! the lines are identical, too long, or so different that highlighting
//! nearly everything would only add noise. A line kept whole inside the
//! other (`char *s;` → `char *s = f(x);`) is never "too different".

use similar::{Algorithm, DiffTag};
use std::ops::Range;
use std::time::{Duration, Instant};

/// Lines longer than this (in chars) are not diffed.
pub const MAX_LINE_CHARS: usize = 500;
/// A line kept whole inside the other needs at least this many
/// non-whitespace tokens to count (a lone `}` or `;` does not).
const KEPT_LINE_TOKENS: usize = 2;
/// Both lines with at most this many non-whitespace tokens → char-level.
/// (More than one word per side diffed per char gives scattered noise.)
const SHORT_LINE_TOKENS: usize = 1;
/// Per-pair time budget for the token diff.
const DEADLINE: Duration = Duration::from_millis(5);

/// Changed char ranges on the old and the new line.
pub type IntraRanges = (Vec<Range<u32>>, Vec<Range<u32>>);

#[derive(Debug, Clone, Copy)]
struct Token<'a> {
    text: &'a str,
    /// Char offset of the token in the line.
    start: u32,
    /// Length in chars.
    len: u32,
    space: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Word,
    Space,
    Other,
}

fn class(c: char) -> Class {
    if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else if c.is_whitespace() {
        Class::Space
    } else {
        Class::Other
    }
}

/// Split `s` into tokens: words, whitespace runs, single other characters
/// (or every character when `chars` is set).
fn tokenize(s: &str, chars: bool) -> Vec<Token<'_>> {
    let mut out: Vec<Token<'_>> = Vec::new();
    let mut tok_start_byte = 0;
    let mut tok_start_char = 0u32;
    let mut cur: Option<Class> = None;
    let mut char_no = 0u32;
    for (byte, c) in s.char_indices() {
        let k = class(c);
        let joins = !chars && cur == Some(k) && k != Class::Other;
        if !joins {
            if let Some(prev) = cur {
                out.push(Token {
                    text: &s[tok_start_byte..byte],
                    start: tok_start_char,
                    len: char_no - tok_start_char,
                    space: prev == Class::Space,
                });
            }
            tok_start_byte = byte;
            tok_start_char = char_no;
            cur = Some(k);
        }
        char_no += 1;
    }
    if let Some(prev) = cur {
        out.push(Token {
            text: &s[tok_start_byte..],
            start: tok_start_char,
            len: char_no - tok_start_char,
            space: prev == Class::Space,
        });
    }
    out
}

fn non_space(tokens: &[Token<'_>]) -> usize {
    tokens.iter().filter(|t| !t.space).count()
}

fn non_space_chars(tokens: &[Token<'_>]) -> u32 {
    tokens.iter().filter(|t| !t.space).map(|t| t.len).sum()
}

/// Char range covered by tokens `r` (non-empty).
fn char_range(tokens: &[Token<'_>], r: &Range<usize>) -> Range<u32> {
    let first = &tokens[r.start];
    let last = &tokens[r.end - 1];
    first.start..last.start + last.len
}

/// Merge ranges that touch or are separated only by whitespace.
fn merge(ranges: Vec<Range<u32>>, line: &[char]) -> Vec<Range<u32>> {
    let mut out: Vec<Range<u32>> = Vec::with_capacity(ranges.len());
    for r in ranges {
        if let Some(prev) = out.last_mut() {
            let gap = &line[prev.end as usize..r.start as usize];
            if gap.iter().all(|c| c.is_whitespace()) {
                prev.end = r.end;
                continue;
            }
        }
        out.push(r);
    }
    out
}

/// Compute intra-line highlight ranges for a removed/added line pair.
pub fn intra_line(old: &str, new: &str) -> Option<IntraRanges> {
    if old == new {
        return None;
    }
    let old_chars: Vec<char> = old.chars().collect();
    let new_chars: Vec<char> = new.chars().collect();
    if old_chars.len() > MAX_LINE_CHARS || new_chars.len() > MAX_LINE_CHARS {
        return None;
    }

    let mut old_toks = tokenize(old, false);
    let mut new_toks = tokenize(new, false);
    if non_space(&old_toks) <= SHORT_LINE_TOKENS && non_space(&new_toks) <= SHORT_LINE_TOKENS {
        old_toks = tokenize(old, true);
        new_toks = tokenize(new, true);
    }
    let a: Vec<&str> = old_toks.iter().map(|t| t.text).collect();
    let b: Vec<&str> = new_toks.iter().map(|t| t.text).collect();
    let ops = similar::capture_diff_slices_deadline(
        Algorithm::Myers,
        &a,
        &b,
        Some(Instant::now() + DEADLINE),
    );

    let mut old_ranges = Vec::new();
    let mut new_ranges = Vec::new();
    let mut equal = 0u32;
    for op in &ops {
        let (tag, o, n) = op.as_tag_tuple();
        match tag {
            DiffTag::Equal => equal += non_space_chars(&old_toks[o]),
            DiffTag::Delete => old_ranges.push(char_range(&old_toks, &o)),
            DiffTag::Insert => new_ranges.push(char_range(&new_toks, &n)),
            DiffTag::Replace => {
                old_ranges.push(char_range(&old_toks, &o));
                new_ranges.push(char_range(&new_toks, &n));
            }
        }
    }
    if old_ranges.is_empty() && new_ranges.is_empty() {
        return None;
    }
    // Less than a third in common: the whole line changed, no highlights,
    // unless one line is all still there and only the other has changes.
    let kept = (old_ranges.is_empty() && non_space(&old_toks) >= KEPT_LINE_TOKENS)
        || (new_ranges.is_empty() && non_space(&new_toks) >= KEPT_LINE_TOKENS);
    let total = non_space_chars(&old_toks).max(non_space_chars(&new_toks));
    if !kept && total > 0 && equal * 3 < total {
        return None;
    }
    Some((merge(old_ranges, &old_chars), merge(new_ranges, &new_chars)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(s: &str, chars: bool) -> Vec<&str> {
        tokenize(s, chars).iter().map(|t| t.text).collect()
    }

    #[test]
    fn tokenizer() {
        assert_eq!(
            texts("  let x_1 = foo(a, b);", false),
            [
                "  ", "let", " ", "x_1", " ", "=", " ", "foo", "(", "a", ",", " ", "b", ")", ";"
            ]
        );
        assert_eq!(texts("a+b", true), ["a", "+", "b"]);
        assert_eq!(texts("ab", true), ["a", "b"]);
        assert_eq!(texts("", false), Vec::<&str>::new());
        assert_eq!(texts("((", false), ["(", "("]);
        // Char offsets, not bytes.
        let t = tokenize("jürgen wrote", false);
        assert_eq!((t[2].start, t[2].len), (7, 5));
        assert_eq!(t[0].len, 6);
    }

    #[test]
    fn word_level() {
        let (o, n) =
            intra_line("let total = compute(a, b);", "let total = compute(a, c);").unwrap();
        assert_eq!(o, vec![23..24]);
        assert_eq!(n, vec![23..24]);

        let (o, n) = intra_line("    return x * x;", "    return x * y + 1;").unwrap();
        // "x" → "y + 1" (adjacent replace + inserts merge across spaces).
        assert_eq!(o, vec![15..16]);
        assert_eq!(n, vec![15..20]);
    }

    #[test]
    fn insert_and_delete_only() {
        let (o, n) = intra_line("foo(a, b, c)", "foo(a, c)").unwrap();
        assert_eq!(o, vec![7..10]);
        assert!(n.is_empty());
        let (o, n) = intra_line("foo(a, c)", "foo(a, b, c)").unwrap();
        assert!(o.is_empty());
        assert_eq!(n, vec![7..10]);
    }

    #[test]
    fn char_level_for_short_lines() {
        let (o, n) = intra_line("value1", "value2").unwrap();
        assert_eq!(o, vec![5..6]);
        assert_eq!(n, vec![5..6]);
        let (o, n) = intra_line("c", "c2").unwrap();
        assert!(o.is_empty());
        assert_eq!(n, vec![1..2]);
        // More than one word: word level.
        let (o, n) = intra_line("x = 10", "x = 12").unwrap();
        assert_eq!(o, vec![4..6]);
        assert_eq!(n, vec![4..6]);
    }

    #[test]
    fn non_ascii_offsets_are_chars() {
        let (o, n) = intra_line("author: jürgen smith here", "author: jürgen jones here").unwrap();
        assert_eq!(o, vec![15..20]);
        assert_eq!(n, vec![15..20]);
    }

    #[test]
    fn line_kept_inside_the_other() {
        // apache r1938822: the initializer moved further down.
        let old = "      char *errstr = apr_palloc(logger->pool, errstr_size);";
        let (o, n) = intra_line(old, "      char *errstr;").unwrap();
        assert_eq!(o, vec![18..58]);
        assert!(n.is_empty());
        let (o, n) = intra_line("      char *errstr;", old).unwrap();
        assert!(o.is_empty());
        assert_eq!(n, vec![18..58]);
        // A lone brace is not enough.
        assert_eq!(intra_line("}", "} else if (retry_count > max_retries) {"), None);
    }

    #[test]
    fn whitespace_only_change() {
        let (o, n) = intra_line("  foo(bar, baz) + qux", "    foo(bar, baz) + qux").unwrap();
        assert_eq!(o, vec![0..2]);
        assert_eq!(n, vec![0..4]);
    }

    #[test]
    fn nothing_to_highlight() {
        assert_eq!(intra_line("same", "same"), None);
        // Completely different lines.
        assert_eq!(intra_line("int main(void) {", "    return result;"), None);
        // Too long.
        let long = "x ".repeat(400);
        assert_eq!(intra_line(&long, &format!("{long}y")), None);
    }

    #[test]
    fn empty_sides() {
        // One side empty: nothing in common.
        assert_eq!(intra_line("", "foo bar"), None);
        assert_eq!(intra_line("foo bar", ""), None);
    }
}
