//! Diff window (DESIGN.md §7).

use crate::app::Ctx;
use std::rc::Rc;
use svn_backend::DiffSpec;

pub fn open(ctx: &Rc<Ctx>, spec: DiffSpec) {
    crate::common::placeholder_window(ctx, "Diff", &format!("{spec:?}"));
}
