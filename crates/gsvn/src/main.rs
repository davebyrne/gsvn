mod app;
mod auth;
mod browser;
mod common;
mod diff;
#[cfg(debug_assertions)]
mod e2e;
mod log;
mod settings;
mod start;
mod viewer;

fn main() -> gtk::glib::ExitCode {
    app::run()
}
