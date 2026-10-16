//! Commit log window (DESIGN.md §6).
//!
//! - `logic` — GTK-free paging cursor, search terms, selection → diff mapping
//! - `objects` — list-model items
//! - `columns` — column factories
//! - `window` — the window and its controller
//! - `debug` — debug-build scripting hook (`GSVN_LOG_SCRIPT`)

mod columns;
#[cfg(debug_assertions)]
mod debug;
mod logic;
mod objects;
mod window;

use crate::app::Ctx;
use std::rc::Rc;
use svn_backend::{RepoUrl, RevNum};

/// Open the log of `url` (file or directory) starting at `peg`.
pub fn open(ctx: &Rc<Ctx>, url: RepoUrl, peg: RevNum) {
    let win = window::LogWindow::new(ctx, url, peg);
    win.start();
    #[cfg(debug_assertions)]
    debug::maybe_run(&win);
}
