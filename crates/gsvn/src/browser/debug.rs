//! Debug-build hooks for visual testing without a screen:
//!
//! - `GSVN_BROWSER_DARK=1`: force the dark style.
//! - `GSVN_BROWSER_SCRIPT="2:goto=URL;4:action=browser.location;…"`:
//!   at the given second, go to a URL (like the path bar entry), activate
//!   an action (`group.name`), type into the filter (`filter=TEXT`),
//!   open the context menu of list row N (`menu=N`) or select a tree row
//!   (`tree=N`).
//!   Timing commands: `filtertime=TEXT` sets the file list filter
//!   directly (no search-entry delay) and reports the time until the
//!   filtered list is sorted; `scroll=SECS` scrolls the file list from
//!   top to bottom over SECS seconds and reports frame times; `rss`
//!   prints the resident set size.
//! - `GSVN_BROWSER_SNAPSHOT=/path/out.png` (+ `GSVN_BROWSER_SNAPSHOT_AT`
//!   seconds, default 4): render the window to a PNG, then quit.
//!
//! The same commands work as `GSVN_E2E_SCRIPT` steps (see `e2e.rs`), plus
//! `select=N` (select list row N), `activate=N` (open list row N, as a
//! double-click), `saveas=N:PATH` (Save As of list row N, without the
//! file dialog), `treemenu=N` (context menu of tree row N) and
//! `shotnewest=PATH` (render the newest other window).

use super::Browser;
use adw::prelude::*;
use gtk::glib;
use std::rc::Rc;
use std::time::Duration;

pub fn install(browser: &Rc<Browser>, window: &adw::ApplicationWindow) {
    if std::env::var_os("GSVN_BROWSER_DARK").is_some() {
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
    }
    if let Ok(script) = std::env::var("GSVN_BROWSER_SCRIPT") {
        for step in script.split(';').filter(|s| !s.trim().is_empty()) {
            let Some((at, cmd)) = step.split_once(':') else { continue };
            let Ok(at) = at.trim().parse::<f64>() else { continue };
            let weak = Rc::downgrade(browser);
            let cmd = cmd.trim().to_owned();
            glib::timeout_add_local_once(Duration::from_secs_f64(at), move || {
                if let Some(b) = weak.upgrade() {
                    run(&b, &cmd);
                }
            });
        }
    }
    if let Ok(path) = std::env::var("GSVN_BROWSER_SNAPSHOT") {
        let at = std::env::var("GSVN_BROWSER_SNAPSHOT_AT")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(4.0);
        let window = window.downgrade();
        glib::timeout_add_local_once(Duration::from_secs_f64(at), move || {
            if let Some(window) = window.upgrade() {
                snapshot(&window, &path);
                if let Some(app) = window.application() {
                    app.quit();
                }
            }
        });
    }
}

impl crate::e2e::Scriptable for Browser {
    fn busy(&self) -> bool {
        self.state.borrow().info_token.is_some() || self.current_dir().is_some_and(|d| d.is_loading())
    }

    fn step(self: Rc<Self>, cmd: String, arg: String) -> crate::e2e::StepFuture {
        Box::pin(async move {
            match cmd.as_str() {
                "select" => {
                    let pos: u32 = arg.parse().unwrap_or(0);
                    self.files.selection.set_selected(pos);
                }
                "activate" => match self.files.item(arg.parse().unwrap_or(0)) {
                    Some(entry) => self.open_entry(&entry),
                    None => eprintln!("activate: no row {arg}"),
                },
                "saveas" => {
                    let (pos, path) = arg.split_once(':').unwrap_or(("0", &arg));
                    let entry = self.files.item(pos.parse().unwrap_or(0));
                    let rev = self.state.borrow().loc.as_ref().map(|l| l.rev);
                    if let (Some(entry), Some(rev)) = (entry, rev) {
                        Browser::save_to(Rc::downgrade(&self), entry.url(), rev, gtk::gio::File::for_path(path)).await;
                    }
                }
                "treemenu" => {
                    self.tree.selection.set_selected(arg.parse().unwrap_or(0));
                    match self.tree.selected_entry() {
                        Some(entry) => {
                            let view = self.tree.view.clone().upcast::<gtk::Widget>();
                            self.popup_menu(&entry, true, &view, 100.0, 20.0);
                        }
                        None => eprintln!("treemenu: no row {arg}"),
                    }
                }
                "shotnewest" => {
                    let own = self.window.upgrade().map(|w| w.upcast::<gtk::Window>());
                    let newest = self.ctx.app.windows().into_iter().find(|w| Some(w) != own.as_ref());
                    match newest {
                        Some(w) => crate::e2e::screenshot(&w, &arg),
                        None => eprintln!("shotnewest: no other window"),
                    }
                }
                "goto" | "action" | "filter" | "menu" | "tree" | "revpopover" | "dump" => {
                    run(&self, &format!("{cmd}={arg}"));
                }
                _ => return false,
            }
            true
        })
    }
}

fn run(b: &Rc<Browser>, cmd: &str) {
    let (verb, arg) = cmd.split_once('=').unwrap_or((cmd, ""));
    match verb {
        "goto" => b.go_to_input(arg),
        "action" => {
            let (group, name) = arg.split_once('.').unwrap_or(("browser", arg));
            let g = if group == "row" { &b.row_actions } else { &b.actions };
            g.activate_action(name, None);
        }
        "filter" => b.filter_entry.set_text(arg),
        "menu" => {
            let pos: u32 = arg.parse().unwrap_or(0);
            b.files.selection.set_selected(pos);
            if let Some(entry) = b.files.item(pos) {
                let view = b.files.view.clone().upcast::<gtk::Widget>();
                b.popup_menu(&entry, false, &view, 120.0, 60.0 + 34.0 * pos as f64);
            }
        }
        "tree" => {
            let pos: u32 = arg.parse().unwrap_or(0);
            b.tree.selection.set_selected(pos);
        }
        "revpopover" => b.rev_button.popup(),
        "dump" => {
            let n = b.files.n_shown();
            let pending = b.files.sort_model.pending();
            let first: Vec<String> = (0..n.min(arg.parse().unwrap_or(5)))
                .filter_map(|i| b.files.item(i))
                .map(|e| format!("{}{}", e.name(), if e.is_dir() { "/" } else { "" }))
                .collect();
            eprintln!("dump: {n} items, {pending} pending sort: {first:?}");
        }
        "filtertime" => filter_time(b, arg),
        "scroll" => scroll_time(b, arg.parse().unwrap_or(3.0)),
        "rss" => eprintln!("rss: {}", super::rss()),
        _ => eprintln!("GSVN_BROWSER_SCRIPT: unknown command {cmd}"),
    }
}

fn filter_time(b: &Rc<Browser>, text: &str) {
    let t = std::time::Instant::now();
    b.files.set_filter_text(text);
    let sync = t.elapsed();
    let text = text.to_owned();
    let shown = b.files.filter_model.clone();
    let report = move |_: &gtk::SortListModel| {
        eprintln!(
            "filtertime {text:?}: {} matches, filter {:.1} ms, sorted after {:.1} ms",
            shown.n_items(),
            sync.as_secs_f64() * 1000.0,
            t.elapsed().as_secs_f64() * 1000.0,
        );
    };
    let model = b.files.sort_model.clone();
    if model.pending() == 0 {
        report(&model);
        return;
    }
    b.files.view.add_tick_callback(move |_, _| {
        if model.pending() > 0 {
            return glib::ControlFlow::Continue;
        }
        report(&model);
        glib::ControlFlow::Break
    });
}

/// Scroll the file list top to bottom over `secs`, one step per frame,
/// and report frame intervals.
fn scroll_time(b: &Rc<Browser>, secs: f64) {
    let adj = b.files.scrolled.vadjustment();
    adj.set_value(0.0);
    let start = std::cell::Cell::new(None::<i64>);
    let last = std::cell::Cell::new(0i64);
    let frames = std::cell::RefCell::new(Vec::<f64>::new());
    b.files.view.add_tick_callback(move |_, clock| {
        let now = clock.frame_time();
        let t0 = start.get().unwrap_or(now);
        start.set(Some(t0));
        if last.get() != 0 {
            frames.borrow_mut().push((now - last.get()) as f64 / 1000.0);
        }
        last.set(now);
        let f = ((now - t0) as f64 / 1e6 / secs).min(1.0);
        adj.set_value(f * (adj.upper() - adj.page_size()));
        if f < 1.0 {
            return glib::ControlFlow::Continue;
        }
        let v = frames.borrow();
        let n = v.len().max(1) as f64;
        let avg = v.iter().sum::<f64>() / n;
        let worst = v.iter().cloned().fold(0.0, f64::max);
        let slow = v.iter().filter(|&&d| d > 20.0).count();
        eprintln!(
            "scroll: {} frames, avg {avg:.1} ms ({:.0} fps), worst {worst:.1} ms, {slow} frames > 20 ms",
            v.len(),
            1000.0 / avg
        );
        glib::ControlFlow::Break
    });
}

fn snapshot(window: &adw::ApplicationWindow, path: &str) {
    let widget = window.upcast_ref::<gtk::Widget>();
    let (w, h) = (widget.width(), widget.height());
    let paintable = gtk::WidgetPaintable::new(Some(widget));
    let snap = gtk::Snapshot::new();
    paintable.snapshot(&snap, w as f64, h as f64);
    // Popovers are separate surfaces; draw visible ones on top.
    for popover in visible_popovers(widget) {
        let Some(native) = popover.native() else { continue };
        let _ = native;
        let Some(bounds) = popover.compute_bounds(widget) else { continue };
        let p = gtk::WidgetPaintable::new(Some(&popover));
        snap.save();
        snap.translate(&gtk::graphene::Point::new(bounds.x(), bounds.y()));
        p.snapshot(&snap, bounds.width() as f64, bounds.height() as f64);
        snap.restore();
    }
    let Some(node) = snap.to_node() else {
        eprintln!("snapshot: nothing to render");
        return;
    };
    let Some(renderer) = widget.native().and_then(|n| n.renderer()) else { return };
    let texture = renderer.render_texture(&node, Some(&gtk::graphene::Rect::new(0.0, 0.0, w as f32, h as f32)));
    match texture.save_to_png(path) {
        Ok(()) => eprintln!("snapshot: wrote {path}"),
        Err(e) => eprintln!("snapshot: {e}"),
    }
}

fn visible_popovers(root: &gtk::Widget) -> Vec<gtk::Widget> {
    let mut out = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(w) = stack.pop() {
        let mut child = w.first_child();
        while let Some(c) = child {
            if c.is::<gtk::Popover>() {
                if c.is_visible() && c.is_mapped() {
                    out.push(c.clone());
                }
            } else {
                stack.push(c.clone());
            }
            // Popovers can host submenus etc.; their own children are not needed.
            child = c.next_sibling();
        }
    }
    out
}
