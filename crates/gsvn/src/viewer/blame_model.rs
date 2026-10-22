//! Blame data for the gutter: per-line commit, grouped into runs.
//!
//! `svn blame --xml` carries no line text, so lines are matched with the
//! `svn cat` text by line number (0-based index = `line_no - 1`). A *run*
//! is a maximal sequence of consecutive lines attributed to the same
//! revision (or all older than the blame start); the gutter labels the
//! first line of each run and bands alternate runs.

use std::collections::HashMap;
use svn_backend::{BlameLine, RevNum, Timestamp};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub rev: RevNum,
    pub author: Option<String>,
    pub date: Option<Timestamp>,
}

/// What the blame says about one line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// Not received (yet).
    Pending,
    /// Last changed before the blame range start.
    Old,
    /// Index into [`BlameModel::commits`].
    Commit(u32),
}

#[derive(Debug, Default)]
pub struct BlameModel {
    commits: Vec<Commit>,
    by_rev: HashMap<RevNum, u32>,
    slots: Vec<Slot>,
    /// Run number per line; equal for consecutive lines of one run.
    runs: Vec<u32>,
    received: usize,
    min_rev: Option<RevNum>,
    max_rev: Option<RevNum>,
    max_author_chars: usize,
}

impl BlameModel {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Number of blame lines received.
    pub fn received(&self) -> usize {
        self.received
    }

    /// Highest line index + 1 covered by received lines.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn revision_count(&self) -> usize {
        self.commits.len()
    }

    /// Length in characters of the longest author seen.
    pub fn max_author_chars(&self) -> usize {
        self.max_author_chars
    }

    pub fn push(&mut self, line: BlameLine) {
        let Some(idx) = (line.line_no as usize).checked_sub(1) else {
            return;
        };
        let slot = match line.commit {
            None => Slot::Old,
            Some(c) => {
                let id = match self.by_rev.get(&c.rev) {
                    Some(&id) => id,
                    None => {
                        let id = self.commits.len() as u32;
                        if let Some(a) = &c.author {
                            self.max_author_chars = self.max_author_chars.max(a.chars().count());
                        }
                        self.min_rev = Some(self.min_rev.map_or(c.rev, |m| m.min(c.rev)));
                        self.max_rev = Some(self.max_rev.map_or(c.rev, |m| m.max(c.rev)));
                        self.commits.push(Commit {
                            rev: c.rev,
                            author: c.author,
                            date: c.date,
                        });
                        self.by_rev.insert(c.rev, id);
                        id
                    }
                };
                Slot::Commit(id)
            }
        };
        if idx == self.slots.len() {
            // The usual case: lines arrive in order.
            self.slots.push(slot);
            self.received += 1;
            let run = match idx {
                0 => 0,
                _ if same_run(self.slots[idx - 1], slot) => self.runs[idx - 1],
                _ => self.runs[idx - 1] + 1,
            };
            self.runs.push(run);
            return;
        }
        if idx >= self.slots.len() {
            self.slots.resize(idx + 1, Slot::Pending);
        }
        if self.slots[idx] == Slot::Pending {
            self.received += 1;
        }
        self.slots[idx] = slot;
        self.recompute_runs(idx);
    }

    fn recompute_runs(&mut self, from: usize) {
        self.runs.resize(self.slots.len(), 0);
        for i in from.max(1)..self.slots.len() {
            self.runs[i] = if same_run(self.slots[i - 1], self.slots[i]) {
                self.runs[i - 1]
            } else {
                self.runs[i - 1] + 1
            };
        }
    }

    pub fn slot(&self, line: usize) -> Slot {
        self.slots.get(line).copied().unwrap_or(Slot::Pending)
    }

    pub fn commit(&self, line: usize) -> Option<&Commit> {
        match self.slot(line) {
            Slot::Commit(id) => self.commits.get(id as usize),
            _ => None,
        }
    }

    /// True if `line` starts a run (the gutter shows its label).
    pub fn is_run_start(&self, line: usize) -> bool {
        match line {
            _ if line >= self.slots.len() => false,
            0 => true,
            _ => self.runs[line] != self.runs[line - 1],
        }
    }

    /// Alternating band per run (true for every other run).
    pub fn band(&self, line: usize) -> bool {
        self.runs.get(line).is_some_and(|r| r % 2 == 1)
    }

    /// 0.0 for the oldest revision seen, 1.0 for the newest.
    pub fn age(&self, rev: RevNum) -> f32 {
        match (self.min_rev, self.max_rev) {
            (Some(lo), Some(hi)) if hi > lo => (rev.saturating_sub(lo)) as f32 / (hi - lo) as f32,
            _ => 1.0,
        }
    }

    /// Compare the blame with the text it is shown against: `None` if they
    /// agree, else `(blame_lines, text_lines)`. Only meaningful once the
    /// blame finished.
    pub fn mismatch(&self, text_lines: usize) -> Option<(usize, usize)> {
        (self.slots.len() != text_lines).then_some((self.slots.len(), text_lines))
    }
}

fn same_run(a: Slot, b: Slot) -> bool {
    a == b && a != Slot::Pending
}

#[cfg(test)]
mod tests {
    use super::*;
    use svn_backend::CommitInfo;

    fn line(no: u64, rev: Option<RevNum>) -> BlameLine {
        BlameLine {
            line_no: no,
            commit: rev.map(|rev| CommitInfo {
                rev,
                author: Some(format!("user{rev}")),
                date: None,
            }),
        }
    }

    fn model(revs: &[Option<RevNum>]) -> BlameModel {
        let mut m = BlameModel::default();
        for (i, r) in revs.iter().enumerate() {
            m.push(line(i as u64 + 1, *r));
        }
        m
    }

    #[test]
    fn runs_and_bands() {
        let m = model(&[Some(5), Some(5), Some(3), None, None, Some(5)]);
        let starts: Vec<bool> = (0..6).map(|i| m.is_run_start(i)).collect();
        assert_eq!(starts, [true, false, true, true, false, true]);
        let bands: Vec<bool> = (0..6).map(|i| m.band(i)).collect();
        assert_eq!(bands, [false, false, true, false, false, true]);
        assert_eq!(m.revision_count(), 2);
        assert_eq!(m.commit(5).unwrap().rev, 5);
        assert_eq!(m.slot(3), Slot::Old);
        assert!(m.commit(3).is_none());
        assert_eq!(m.max_author_chars(), 5);
    }

    #[test]
    fn out_of_order_and_gaps() {
        let mut m = BlameModel::default();
        m.push(line(3, Some(7)));
        assert_eq!(m.slot(0), Slot::Pending);
        assert_eq!(m.received(), 1);
        m.push(line(1, Some(7)));
        m.push(line(2, Some(7)));
        assert_eq!(m.received(), 3);
        assert!(m.is_run_start(0));
        assert!(!m.is_run_start(1));
        assert!(!m.is_run_start(2));
        // Re-sending a line does not count twice.
        m.push(line(2, Some(7)));
        assert_eq!(m.received(), 3);
        // line_no 0 is invalid and ignored.
        m.push(line(0, Some(1)));
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn pending_lines_never_join_runs() {
        let mut m = BlameModel::default();
        m.push(line(1, Some(2)));
        m.push(line(4, Some(2)));
        assert!(m.is_run_start(3));
        assert_eq!(m.slot(1), Slot::Pending);
    }

    #[test]
    fn zipping_with_text() {
        let m = model(&[Some(1), Some(2)]);
        assert_eq!(m.mismatch(2), None);
        assert_eq!(m.mismatch(3), Some((2, 3)));
        // Lines past the blame are simply pending.
        assert_eq!(m.slot(10), Slot::Pending);
        assert!(!m.is_run_start(10));
        assert!(!m.band(10));
    }

    #[test]
    fn age() {
        let m = model(&[Some(10), Some(20), Some(15)]);
        assert_eq!(m.age(10), 0.0);
        assert_eq!(m.age(20), 1.0);
        assert_eq!(m.age(15), 0.5);
        assert_eq!(model(&[Some(4)]).age(4), 1.0);
    }
}
