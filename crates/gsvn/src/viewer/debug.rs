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
//! `shot=PATH`, `dark`, `light`, `blame`, `content`, `load-anyway`,
//! `cancel`, `cancel-blame`, `since=REV`, `search=TEXT`, `goto=LINE`,
//! `menu=LINE` (gutter menu), `prev=LINE` (blame previous revision of
//! LINE's commit), `size=W,H`. The app quits after the last step.

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
                    screenshot(&viewer.window, arg);
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
                "prev" => {
                    let rev = viewer.blame.borrow().commit(arg.parse().unwrap_or(0)).map(|c| c.rev);
                    match rev {
                        Some(r) if r > 1 => viewer.switch_revision(r - 1),
                        _ => eprintln!("prev: no commit on that line"),
                    }
                }
                "size" => {
                    if let Some((w, h)) = arg.split_once(',')
                        && let (Ok(w), Ok(h)) = (w.parse(), h.parse())
                    {
                        viewer.window.set_default_size(w, h);
                    }
                }
                "dump" => {
                    let gutter = sourceview5::prelude::ViewExt::gutter(&viewer.view, gtk::TextWindowType::Left);
                    eprintln!("gutter {:?} width {}", gutter.allocation(), gutter.width());
                    let mut child = gutter.first_child();
                    while let Some(c) = child {
                        eprintln!(
                            "  {} {:?} visible {} measure {:?}",
                            c.type_().name(),
                            c.allocation(),
                            c.is_visible(),
                            c.measure(gtk::Orientation::Horizontal, -1)
                        );
                        child = c.next_sibling();
                    }
                }
                _ => eprintln!("unknown step {step}"),
            }
        }
        eprintln!("longest main-loop stall: {:?}", stall.get());
        if !steps.is_empty() {
            viewer.window.close();
            ctx.app.quit();
        }
    });
}

fn screenshot(window: &adw::ApplicationWindow, path: &str) {
    let paintable = gtk::WidgetPaintable::new(Some(window));
    let (w, h) = (window.width(), window.height());
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, f64::from(w), f64::from(h));
    let Some(node) = snapshot.to_node() else {
        eprintln!("screenshot: nothing to render");
        return;
    };
    let Some(renderer) = window.renderer() else { return };
    let texture = renderer.render_texture(&node, None);
    match texture.save_to_png(path) {
        Ok(()) => eprintln!("screenshot: {path}"),
        Err(e) => eprintln!("screenshot: {e}"),
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
