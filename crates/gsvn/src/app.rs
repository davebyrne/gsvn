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
//!
//! Command line (a second launch forwards to the running instance):
//!
//! ```text
//! gsvn                      start window
//! gsvn URL…                 browser window(s)
//! gsvn --rev N URL          browser at revision N
//! gsvn --log [--rev N] URL  log window
//! ```
//!
//! Application actions: `app.new-window` (Ctrl+N), `app.quit` (Ctrl+Q),
//! `app.shortcuts` (Ctrl+?), `app.about`; `window.close` is bound to
//! Ctrl+W. All accelerators are listed in [`crate::shortcuts`].

use crate::common;
use crate::settings::Settings;
use adw::prelude::*;
use gtk::{gio, glib};
use std::rc::Rc;
use svn_backend::{Priority, RepoUrl, Rev, RevNum, Svn, SvnConfig};

pub const APP_ID: &str = "io.github.gsvn.Gsvn";

/// Shared, main-thread-only application state.
pub struct Ctx {
    pub app: adw::Application,
    pub svn: Svn,
    pub settings: Settings,
}

pub fn run() -> glib::ExitCode {
    #[cfg(debug_assertions)]
    let started = std::time::Instant::now();
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    app.set_option_context_parameter_string(Some("[URL…]"));
    app.set_option_context_summary(Some(
        "Browse Subversion repositories. Without a URL, shows the start window.",
    ));
    app.add_main_option(
        "rev",
        glib::Char::from(b'r'),
        glib::OptionFlags::NONE,
        glib::OptionArg::String,
        "Open at revision REV (a number or HEAD)",
        Some("REV"),
    );
    app.add_main_option(
        "log",
        glib::Char::from(b'l'),
        glib::OptionFlags::NONE,
        glib::OptionArg::None,
        "Open the commit log of URL instead of browsing it",
        None,
    );
    app.add_main_option(
        "version",
        glib::Char::from(0),
        glib::OptionFlags::NONE,
        glib::OptionArg::None,
        "Print the version and exit",
        None,
    );
    app.connect_handle_local_options(|_, options| {
        if options.contains("version") {
            println!("gsvn {}", env!("CARGO_PKG_VERSION"));
            return std::ops::ControlFlow::Break(glib::ExitCode::SUCCESS);
        }
        std::ops::ControlFlow::Continue(())
    });

    app.connect_startup(move |app| {
        // Uninstalled debug builds: find the app icon in the source tree.
        #[cfg(debug_assertions)]
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::IconTheme::for_display(&display)
                .add_search_path(concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/icons"));
        }
        gtk::Window::set_default_icon_name(APP_ID);
        let ctx = Rc::new(Ctx {
            app: app.clone(),
            svn: Svn::new(SvnConfig::default()),
            settings: Settings::load(),
        });
        crate::auth::install(&ctx);
        install_actions(&ctx);
        #[cfg(debug_assertions)]
        crate::viewer::debug::hook(&ctx);
        #[cfg(debug_assertions)]
        startup_timing_hook(app, started);
        #[cfg(debug_assertions)]
        crate::e2e::hook(&ctx);
        {
            let ctx = ctx.clone();
            app.connect_activate(move |_| crate::start::open(&ctx));
        }
        {
            // Windows closed by `app.quit` queue the write on an idle that
            // may never run.
            let ctx = ctx.clone();
            app.connect_shutdown(move |_| ctx.settings.save_state());
        }
        app.connect_command_line(move |_, cmd| handle_command_line(&ctx, cmd));
    });
    app.run()
}

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    Start,
    Browse(Vec<RepoUrl>, Option<RevNum>),
    Log(Vec<RepoUrl>, Option<RevNum>),
}

fn parse_command(args: &[String], rev: Option<&str>, log: bool) -> Result<Command, String> {
    let rev = match rev {
        Some(r) => common::parse_rev_input(r)?,
        None => None,
    };
    let urls = args
        .iter()
        .map(|a| common::parse_url_input(a).map_err(|e| format!("{a}: {e}")))
        .collect::<Result<Vec<_>, _>>()?;
    if urls.is_empty() {
        if log {
            return Err("--log needs a URL".to_owned());
        }
        return Ok(Command::Start);
    }
    Ok(if log {
        Command::Log(urls, rev)
    } else {
        Command::Browse(urls, rev)
    })
}

fn handle_command_line(ctx: &Rc<Ctx>, cmd: &gio::ApplicationCommandLine) -> glib::ExitCode {
    #[cfg(debug_assertions)]
    if crate::diff::debug_open(ctx) {
        return glib::ExitCode::SUCCESS;
    }
    let options = cmd.options_dict();
    let rev = options.lookup::<String>("rev").ok().flatten();
    let log = options.contains("log");
    let args: Vec<String> = cmd
        .arguments()
        .into_iter()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();

    match parse_command(&args, rev.as_deref(), log) {
        Ok(Command::Start) => crate::start::open(ctx),
        Ok(Command::Browse(urls, rev)) => {
            for url in urls {
                ctx.settings.add_recent(url.as_str());
                crate::browser::open(ctx, url, rev);
            }
        }
        Ok(Command::Log(urls, rev)) => {
            for url in urls {
                open_log(ctx, url, rev);
            }
        }
        Err(e) => {
            cmd.printerr_literal(&format!("gsvn: {e}\n"));
            return glib::ExitCode::FAILURE;
        }
    }
    glib::ExitCode::SUCCESS
}

/// Open the log of `url` at peg `rev`, resolving HEAD first when `None`.
fn open_log(ctx: &Rc<Ctx>, url: RepoUrl, rev: Option<RevNum>) {
    if let Some(peg) = rev {
        crate::log::open(ctx, url, peg);
        return;
    }
    // Keep the application alive while HEAD is resolved with no window open.
    let hold = ctx.app.hold();
    let ctx = ctx.clone();
    common::spawn(async move {
        let _hold = hold;
        let result = ctx.svn.info(&url, &Rev::Head, Priority::Interactive).result().await;
        match result {
            Ok(info) => crate::log::open(&ctx, url, info.rev),
            Err(err) if err.is_cancelled() => {}
            Err(err) => {
                let parent = match ctx.app.active_window() {
                    Some(window) => window,
                    None => crate::start::open_window(&ctx).upcast(),
                };
                common::show_message(
                    Some(parent.upcast_ref()),
                    "Cannot Open Log",
                    &format!("{}\n\n{err}", svn_backend::url::decode(url.as_str())),
                );
            }
        }
    });
}

/// Debug builds: with `GSVN_DEBUG_STARTUP` set, print when the first
/// window has painted its first frame, then quit. The line carries the
/// wall clock (`epoch_ms`) so a script can measure from exec, and the
/// time since `main` (`since_main_ms`).
#[cfg(debug_assertions)]
fn startup_timing_hook(app: &adw::Application, started: std::time::Instant) {
    if std::env::var_os("GSVN_DEBUG_STARTUP").is_none() {
        return;
    }
    app.connect_window_added(move |app, window| {
        if app.windows().len() != 1 {
            return;
        }
        let app = app.clone();
        window.connect_map(move |window| {
            let Some(clock) = window.frame_clock() else { return };
            let app = app.clone();
            let done = std::cell::Cell::new(false);
            clock.connect_after_paint(move |_| {
                if done.replace(true) {
                    return;
                }
                let epoch = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default();
                eprintln!(
                    "[gsvn-startup] first frame: epoch_ms={} since_main_ms={:.1}",
                    epoch.as_millis(),
                    started.elapsed().as_secs_f64() * 1000.0
                );
                let app = app.clone();
                glib::idle_add_local_once(move || app.quit());
            });
        });
    });
}

fn install_actions(ctx: &Rc<Ctx>) {
    let app = &ctx.app;

    let new_window = {
        let ctx = ctx.clone();
        gio::ActionEntry::builder("new-window")
            .activate(move |_: &adw::Application, _, _| crate::start::open(&ctx))
            .build()
    };
    let quit = gio::ActionEntry::builder("quit")
        .activate(|app: &adw::Application, _, _| {
            // Close windows first so they save their state.
            for window in app.windows() {
                window.close();
            }
            app.quit();
        })
        .build();
    let about = gio::ActionEntry::builder("about")
        .activate(|app: &adw::Application, _, _| show_about(app))
        .build();
    app.add_action_entries([new_window, quit, about]);
    crate::shortcuts::install(app);
}

fn show_about(app: &adw::Application) {
    let about = adw::AboutDialog::builder()
        .application_name("gsvn")
        .application_icon(APP_ID)
        .version(env!("CARGO_PKG_VERSION"))
        .comments("A lightweight, read-only Subversion repository browser")
        .developer_name("The gsvn developers")
        .license_type(gtk::License::Gpl30)
        .build();
    about.present(app.active_window().as_ref());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn command_parsing() {
        assert_eq!(parse_command(&[], None, false), Ok(Command::Start));
        assert!(parse_command(&[], None, true).is_err());
        let url = RepoUrl::parse("https://h/r").unwrap();
        assert_eq!(
            parse_command(&args(&["https://h/r/"]), None, false),
            Ok(Command::Browse(vec![url.clone()], None))
        );
        assert_eq!(
            parse_command(&args(&["https://h/r"]), Some("r12"), true),
            Ok(Command::Log(vec![url.clone()], Some(12)))
        );
        assert_eq!(
            parse_command(&args(&["https://h/r"]), Some("HEAD"), false),
            Ok(Command::Browse(vec![url], None))
        );
        assert!(parse_command(&args(&["https://h/r"]), Some("x"), false).is_err());
        assert!(parse_command(&args(&["not-a-url"]), None, false).is_err());
    }
}
