//! Commit log window (DESIGN.md §6).

use crate::app::Ctx;
use std::rc::Rc;
use svn_backend::{RepoUrl, RevNum};

/// Open the log of `url` (file or directory) starting at `peg`.
pub fn open(ctx: &Rc<Ctx>, url: RepoUrl, peg: RevNum) {
    crate::common::placeholder_window(ctx, "Log", &format!("{url}@{peg}"));
}
