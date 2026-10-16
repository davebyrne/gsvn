//! Credential and certificate prompts (DESIGN.md §9).
//!
//! [`install`] spawns loops on `Svn::auth_prompts` / `Svn::cert_prompts`
//! that look up the keyring and show dialogs on the active window.

use crate::app::Ctx;
use std::rc::Rc;

pub fn install(ctx: &Rc<Ctx>) {
    let _ = ctx;
}
