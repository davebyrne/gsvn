//! Debug-build hooks for developing the viewer without the other windows:
//! open a viewer from the environment, script a few interactions and
//! render the window to PNG.
//!
//! ```text
//! GSVN_DEBUG_VIEWER='URL@REV'            open a viewer at startup
//! GSVN_DEBUG_VIEWER_MODE=blame           start in blame mode
//! GSVN_DEBUG_VIEWER_STEPS='idle;shot=/tmp/a.png;dark;shot=/tmp/b.png'
//! ```
//!
//! Steps (`;`-separated): `wait=MS`, `idle` (until nothing is loading),
//! `shot=PATH`, `shotnewest=PATH` (the newest other window), `dark`, `light`, `blame`, `content`, `load-anyway`,
//! `cancel`, `cancel-blame`, `since=REV`, `search=TEXT`, `goto=LINE`,
//! `menu=LINE` (gutter menu), `prev=LINE` (blame previous revision of
//! LINE's commit), `changes=LINE` (show changes in LINE's commit), `save=PATH`, `dump` (gutter allocations), `size=W,H`. The app quits after the last step.

use super::{BlameState, ContentState, Viewer, ViewerMode};
use crate::app::Ctx;
use adw::prelude::*;
use gtk::glib;
use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};
use svn_backend::RepoUrl;

/// Call once at application startup.
pub fn hook(ctx: &Rc<Ctx>) {
    let Ok(target) = std::env::var("GSVN_DEBUG_VIEWER") else { return };
    let Some((url, rev)) = target.rsplit_once('@') else {
        eprintln!("GSVN_DEBUG_VIEWER: expected URL@REV");
        return;
    };
    let (Ok(url), Ok(rev)) = (RepoUrl::parse(url), rev.parse()) else {
        eprintln!("GSVN_DEBUG_VIEWER: bad URL or revision");
        return;
    };
    let mode = match std::env::var("GSVN_DEBUG_VIEWER_MODE").as_deref() {
        Ok("blame") => ViewerMode::Blame,
        _ => ViewerMode::Content,
    };
    let steps = std::env::var("GSVN_DEBUG_VIEWER_STEPS").unwrap_or_default();
    let hold = ctx.app.hold();
    let ctx = ctx.clone();
    glib::spawn_future_local(async move {
        let _hold = hold;
        // Track the longest main-loop stall while the script runs.
        let stall = Rc::new(Cell::new(Duration::ZERO));
        {
            let stall = stall.clone();
            let mut last = Instant::now();
            glib::timeout_add_local(Duration::from_millis(5), move || {
                let gap = last.elapsed();
                last = Instant::now();
                if gap > stall.get() {
                    stall.set(gap);
                }
                glib::ControlFlow::Continue
            });
        }
        let viewer = Viewer::new(&ctx, url, rev, None, mode);
        for step in steps.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            let (cmd, arg) = step.split_once('=').unwrap_or((step, ""));
            eprintln!("step {step}");
            if !run_step(&viewer, cmd, arg, &stall).await {
                eprintln!("unknown step {step}");
            }
        }
        eprintln!("longest main-loop stall: {:?}", stall.get());
        if !steps.is_empty() {
            viewer.window.close();
            ctx.app.quit();
        }
    });
}

/// Run one step on `viewer`; `false` if `cmd` is unknown.
async fn run_step(viewer: &Rc<Viewer>, cmd: &str, arg: &str, stall: &Cell<Duration>) -> bool {
    match cmd {
        "wait" => glib::timeout_future(Duration::from_millis(arg.parse().unwrap_or(500))).await,
        "idle" => {
            let t0 = Instant::now();
            while viewer.content.get() == ContentState::Loading
                || viewer.blame_state.get() == BlameState::Running
            {
                if t0.elapsed() > Duration::from_secs(300) {
                    eprintln!("idle: timed out");
                    break;
                }
                glib::timeout_future(Duration::from_millis(50)).await;
            }
            eprintln!("idle after {:?}, longest main-loop stall so far {:?}", t0.elapsed(), stall.get());
            glib::timeout_future(Duration::from_millis(400)).await;
        }
        "shot" => {
            glib::timeout_future(Duration::from_millis(300)).await;
            crate::e2e::screenshot(&viewer.window, arg);
        }
        "shotnewest" => {
            // The most recently opened other window (e.g. a diff).
            glib::timeout_future(Duration::from_millis(300)).await;
            let newest = viewer
                .ctx
                .app
                .windows()
                .into_iter()
                .filter(|w| w != viewer.window.upcast_ref::<gtk::Window>())
                .find_map(|w| w.downcast::<adw::ApplicationWindow>().ok());
            match newest {
                Some(w) => crate::e2e::screenshot(&w, arg),
                None => eprintln!("shotnewest: no other window"),
            }
        }
        "dark" => adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark),
        "light" => adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceLight),
        "blame" => viewer.set_mode(ViewerMode::Blame),
        "content" => viewer.set_mode(ViewerMode::Content),
        "load-anyway" => viewer.load(true),
        "cancel" => viewer.cancel_load(),
        "cancel-blame" => viewer.cancel_blame(),
        "since" => viewer.set_blame_start(arg.parse().ok()),
        "search" => viewer.search.set_text(arg),
        "goto" => viewer.goto_line(arg.parse().unwrap_or(1)),
        "menu" => viewer.popup_line_menu(arg.parse().unwrap_or(0)),
        "prev" | "changes" => {
            // Go through the gutter menu's actions; both resolve
            // asynchronously, so follow with `wait=MS;idle`.
            let rev = viewer.blame.borrow().commit(arg.parse().unwrap_or(0)).map(|c| c.rev);
            let (action, target) = match (cmd, rev) {
                ("prev", Some(r)) if r > 1 => ("viewer.blame-previous", r - 1),
                ("changes", Some(r)) => ("viewer.show-changes", r),
                _ => {
                    eprintln!("{cmd}: no commit on that line");
                    return true;
                }
            };
            if let Err(e) = WidgetExt::activate_action(&viewer.window, action, Some(&target.to_variant())) {
                eprintln!("{cmd}: {e}");
            }
        }
        "size" => {
            if let Some((w, h)) = arg.split_once(',')
                && let (Ok(w), Ok(h)) = (w.parse(), h.parse())
            {
                viewer.window.set_default_size(w, h);
            }
        }
        "save" => Viewer::write_file(Rc::downgrade(viewer), gtk::gio::File::for_path(arg)).await,
        "dump" => {
            let gutter = sourceview5::prelude::ViewExt::gutter(&viewer.view, gtk::TextWindowType::Left);
            eprintln!("gutter width {}", gutter.width());
            let mut child = gutter.first_child();
            while let Some(c) = child {
                eprintln!(
                    "  {} bounds {:?} visible {} measure {:?}",
                    c.type_().name(),
                    c.compute_bounds(&gutter),
                    c.is_visible(),
                    c.measure(gtk::Orientation::Horizontal, -1)
                );
                child = c.next_sibling();
            }
        }
        _ => return false,
    }
    true
}

/// The steps also work in `GSVN_E2E_SCRIPT` (see `e2e.rs`), on any viewer.
impl crate::e2e::Scriptable for Viewer {
    fn busy(&self) -> bool {
        self.content.get() == ContentState::Loading || self.blame_state.get() == BlameState::Running
    }

    fn step(self: Rc<Self>, cmd: String, arg: String) -> crate::e2e::StepFuture {
        Box::pin(async move { run_step(&self, &cmd, &arg, &Cell::new(Duration::ZERO)).await })
    }
}

/// Log how long putting a file into the view took, and when the next
/// frame after it was drawn.
pub fn log_text_timing(fetched: Instant, set_start: Instant, bytes: usize) {
    eprintln!(
        "gsvn viewer: {bytes} bytes: decode {:?}, set_text {:?}",
        set_start - fetched,
        set_start.elapsed()
    );
    glib::idle_add_local_full(glib::Priority::DEFAULT_IDLE, move || {
        eprintln!("gsvn viewer: first idle after set_text {:?} after fetch", fetched.elapsed());
        glib::ControlFlow::Break
    });
}
