//! File content and blame window (DESIGN.md §8).

use crate::app::Ctx;
use std::rc::Rc;
use svn_backend::{RepoUrl, RevNum};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewerMode {
    Content,
    Blame,
}

/// Open `url@rev` in a viewer window. `size` is the file size if already
/// known (from a listing), used for the large-file confirmation.
pub fn open(ctx: &Rc<Ctx>, url: RepoUrl, rev: RevNum, size: Option<u64>, mode: ViewerMode) {
    let _ = (size, mode);
    crate::common::placeholder_window(ctx, "Viewer", &format!("{url}@{rev}"));
}
