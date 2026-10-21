//! Diff window (DESIGN.md §7).
//!
//! - [`model`]: pure render models (unified / side-by-side rows, intra-line
//!   ranges, file summaries), unit-tested.
//! - [`view`]: `DiffView` (a `GtkSourceView` painting row backgrounds) and
//!   the line-number gutter renderer.
//! - [`limit`]: the total diff size limit (§7.3).
//! - [`window`]: the window, file sidebar and streaming.

#[cfg(debug_assertions)]
mod debug;
mod intra;
mod limit;
mod model;
mod view;
mod window;

use crate::app::Ctx;
use std::rc::Rc;
use svn_backend::DiffSpec;

/// Open a diff window for a single revision or a revision range.
pub fn open(ctx: &Rc<Ctx>, spec: DiffSpec) {
    window::DiffWindow::open(ctx, spec);
}

/// Debug builds: open the diff given in `GSVN_DEBUG_DIFF` instead of
/// handling the command line. See `debug.rs`.
#[cfg(debug_assertions)]
pub fn debug_open(ctx: &Rc<Ctx>) -> bool {
    debug::open(ctx)
}
