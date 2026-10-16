//! Repository browser window (DESIGN.md §5).

use crate::app::Ctx;
use std::rc::Rc;
use svn_backend::{RepoUrl, RevNum};

/// Open a browser window at `url` (any path inside a repository; if it is
/// a file, show its directory with the file selected). `rev = None` means
/// resolve HEAD.
pub fn open(ctx: &Rc<Ctx>, url: RepoUrl, rev: Option<RevNum>) {
    let _ = rev;
    crate::common::placeholder_window(ctx, "Browser", url.as_str());
}
