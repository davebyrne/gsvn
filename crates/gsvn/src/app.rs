//! Application setup and shared context (DESIGN.md §11).
//!
//! Every window gets an `Rc<Ctx>`; open other windows through the
//! `open` functions of their modules:
//!
//! - [`crate::start::open`] — recent repos and bookmarks
//! - [`crate::browser::open`] — repository browser
//! - [`crate::log::open`] — commit log
//! - [`crate::diff::open`] — diff
//! - [`crate::viewer::open`] — file content / blame

use crate::settings::Settings;
use adw::prelude::*;
use gtk::glib;
use std::rc::Rc;
use svn_backend::{Svn, SvnConfig};

pub const APP_ID: &str = "io.github.gsvn.Gsvn";

/// Shared, main-thread-only application state.
pub struct Ctx {
    pub app: adw::Application,
    pub svn: Svn,
    pub settings: Settings,
}

pub fn run() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|app| {
        let ctx = Rc::new(Ctx {
            app: app.clone(),
            svn: Svn::new(SvnConfig::default()),
            settings: Settings::load(),
        });
        crate::auth::install(&ctx);
        app.connect_activate(move |_| crate::start::open(&ctx));
    });
    app.run()
}
