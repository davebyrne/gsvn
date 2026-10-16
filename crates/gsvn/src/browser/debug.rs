//! Debug-build hooks for visual testing without a screen:
//!
//! - `GSVN_BROWSER_DARK=1`: force the dark style.
//! - `GSVN_BROWSER_SCRIPT="2:goto=URL;4:action=browser.location;…"`:
//!   at the given second, go to a URL (like the path bar entry), activate
//!   an action (`group.name`), type into the filter (`filter=TEXT`),
//!   open the context menu of list row N (`menu=N`) or select a tree row
//!   (`tree=N`).
//! - `GSVN_BROWSER_SNAPSHOT=/path/out.png` (+ `GSVN_BROWSER_SNAPSHOT_AT`
//!   seconds, default 4): render the window to a PNG, then quit.

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
        _ => eprintln!("GSVN_BROWSER_SCRIPT: unknown command {cmd}"),
    }
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
