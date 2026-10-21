//! Debug-build end-to-end script across windows (debug builds only).
//!
//! ```text
//! GSVN_E2E_SCRIPT='idle;activate=3;waitwin=2;idle;blame;idle;menu=4;shot=/tmp/a.png;quit'
//! ```
//!
//! The `;`-separated steps run once the first window is open. Each acts
//! on the *current window*: the newest window (a window opened by a step
//! becomes current), or the one picked with `win=`.
//!
//! | step | effect |
//! |---|---|
//! | `wait=MS` | sleep |
//! | `idle` | until the current window has nothing loading (30 s max) |
//! | `waitwin=N` | until N windows are open (10 s max), then pick the newest |
//! | `win=N` / `win=TEXT` | pick window N (in opening order, from 0) or the newest whose title contains TEXT |
//! | `windows` | print each window's title, subtitle and whether it is active |
//! | `expect=TEXT` | check that the current window's title or subtitle contains TEXT (prints `E2E PASS`/`E2E FAIL`) |
//! | `shot=PATH` | render the current window (with open popovers) to PNG |
//! | `action=GROUP.NAME[:ARG]` | activate an action on the current window (`ARG`: a number is `u64`, else a string) |
//! | `fill=LABEL:VALUE` | set the entry or switch row titled LABEL in the open dialog (`on`/`off` for switches) |
//! | `waitdialog` | until an alert dialog is open (60 s max) |
//! | `respond=ID` | answer the open dialog with response ID |
//! | `popovers` | print the open popovers, flagging menus that need scrolling (cut off) |
//! | `toplevels` | print all toplevel windows (including dialogs that are not application windows) |
//! | `shottop=TEXT:PATH` | render the toplevel whose title contains TEXT |
//! | `chooser=PATH` | in an open file chooser dialog, choose PATH and accept (`chooser=` cancels) |
//! | `close` | close the current window |
//! | `dark` / `light` | colour scheme |
//! | `quit` | quit the application (also done after the last step) |
//!
//! Any other step goes to the current window's own handler; see
//! `debug.rs` in `browser/`, `log/`, `viewer/` and `diff/`.

use crate::app::Ctx;
use adw::prelude::*;
use gtk::glib;
use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

pub type StepFuture = Pin<Box<dyn Future<Output = bool>>>;

/// A window controller that can run script steps.
pub trait Scriptable {
    /// Something is still loading.
    fn busy(&self) -> bool;
    /// Run one window-specific step; `false` if `cmd` is unknown.
    fn step(self: Rc<Self>, cmd: String, arg: String) -> StepFuture;
}

/// A registered window and its controller.
type Target = (glib::WeakRef<gtk::Window>, Weak<dyn Scriptable>);

thread_local! {
    /// Dialogs answered by `respond` (they stay in the tree while closing).
    static ANSWERED: RefCell<Vec<glib::WeakRef<adw::AlertDialog>>> = const { RefCell::new(Vec::new()) };
    static TARGETS: RefCell<Vec<Target>> = const { RefCell::new(Vec::new()) };
}

fn enabled() -> bool {
    std::env::var_os("GSVN_E2E_SCRIPT").is_some()
}

/// Make a window's controller scriptable (a no-op without a script).
pub fn register(window: &impl IsA<gtk::Window>, target: Weak<dyn Scriptable>) {
    if enabled() {
        TARGETS.with(|t| {
            t.borrow_mut()
                .push((window.upcast_ref::<gtk::Window>().downgrade(), target))
        });
    }
}

fn target_of(window: &gtk::Window) -> Option<Rc<dyn Scriptable>> {
    TARGETS.with(|t| {
        t.borrow()
            .iter()
            .find(|(w, _)| w.upgrade().as_ref() == Some(window))
            .and_then(|(_, s)| s.upgrade())
    })
}

/// Call once at application startup.
pub fn hook(ctx: &Rc<Ctx>) {
    let Ok(script) = std::env::var("GSVN_E2E_SCRIPT") else {
        return;
    };
    // Windows in opening order; the newest becomes current.
    let opened: Rc<RefCell<Vec<glib::WeakRef<gtk::Window>>>> = Rc::default();
    let current: Rc<RefCell<Option<gtk::Window>>> = Rc::default();
    {
        let (opened, current) = (opened.clone(), current.clone());
        ctx.app.connect_window_added(move |_, w| {
            opened.borrow_mut().push(w.downgrade());
            current.replace(Some(w.clone()));
        });
    }
    let ctx = ctx.clone();
    glib::spawn_future_local(async move {
        let hold = ctx.app.hold();
        // Also start for a dialog shown with no window (`--log` resolving HEAD).
        while opened.borrow().is_empty() && !toplevels().iter().any(|w| w.is_visible()) {
            sleep(50).await;
        }
        if current.borrow().is_none() {
            current.replace(toplevels().into_iter().find(|w| w.is_visible()));
        }
        sleep(300).await;
        let t0 = Instant::now();
        let (mut pass, mut fail) = (0, 0);
        for step in script.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            let (cmd, arg) = step.split_once('=').unwrap_or((step, ""));
            eprintln!("[e2e {:>6} ms] {step}", t0.elapsed().as_millis());
            let win = current.borrow().clone();
            match cmd {
                "wait" => sleep(arg.parse().unwrap_or(500)).await,
                "idle" => {
                    sleep(150).await;
                    if let Some(target) = win.as_ref().and_then(target_of) {
                        let t = Instant::now();
                        while target.busy() && t.elapsed() < Duration::from_secs(30) {
                            sleep(50).await;
                        }
                        if target.busy() {
                            eprintln!("[e2e] idle: timed out");
                        }
                    }
                    sleep(300).await;
                }
                "waitwin" => {
                    let n: usize = arg.parse().unwrap_or(2);
                    let t = Instant::now();
                    while live(&opened).len() < n && t.elapsed() < Duration::from_secs(10) {
                        sleep(50).await;
                    }
                    let live = live(&opened);
                    if live.len() < n {
                        eprintln!("[e2e] waitwin: only {} windows", live.len());
                    }
                    current.replace(live.last().cloned());
                    sleep(200).await;
                }
                "win" => {
                    let live = live(&opened);
                    let pick = match arg.parse::<usize>() {
                        Ok(i) => live.get(i).cloned(),
                        Err(_) => live
                            .iter()
                            .rev()
                            .find(|w| title_of(w).contains(arg))
                            .cloned(),
                    };
                    match pick {
                        Some(w) => {
                            w.present();
                            current.replace(Some(w));
                        }
                        None => eprintln!("[e2e] win: no window {arg:?}"),
                    }
                }
                "windows" => {
                    for (i, w) in live(&opened).iter().enumerate() {
                        eprintln!(
                            "[e2e] window {i}: title {:?} subtitle {:?} active {} {}",
                            w.title().unwrap_or_default(),
                            subtitle_of(w).unwrap_or_default(),
                            w.is_active(),
                            if current.borrow().as_ref() == Some(w) {
                                "(current)"
                            } else {
                                ""
                            }
                        );
                    }
                }
                "expect" => {
                    let text = win.as_ref().map(|w| {
                        format!("{} | {}", title_of(w), subtitle_of(w).unwrap_or_default())
                    });
                    let text = text.unwrap_or_default();
                    if text.contains(arg) {
                        pass += 1;
                        eprintln!("[e2e] E2E PASS {arg:?} in {text:?}");
                    } else {
                        fail += 1;
                        eprintln!("[e2e] E2E FAIL {arg:?} not in {text:?}");
                    }
                }
                "shot" => {
                    sleep(300).await;
                    if let Some(w) = &win {
                        screenshot(w, arg);
                    }
                }
                "action" => {
                    let (name, param) = match arg.split_once(':') {
                        Some((n, p)) => (n, Some(p)),
                        None => (arg, None),
                    };
                    let param = param.map(|p| match p.parse::<u64>() {
                        Ok(n) => n.to_variant(),
                        Err(_) => p.to_variant(),
                    });
                    if let Some(w) = &win
                        && let Err(e) = WidgetExt::activate_action(w, name, param.as_ref())
                    {
                        eprintln!("[e2e] action {name}: {e}");
                    }
                }
                "fill" => {
                    let (label, value) = arg.split_once(':').unwrap_or((arg, ""));
                    if !win.as_ref().is_some_and(|w| fill(w, label, value)) {
                        eprintln!("[e2e] fill: no row {label:?} in an open dialog");
                    }
                }
                "waitdialog" => {
                    let t = Instant::now();
                    while win.as_ref().and_then(open_alert).is_none()
                        && t.elapsed() < Duration::from_secs(60)
                    {
                        sleep(50).await;
                    }
                    match win.as_ref().and_then(open_alert) {
                        Some(d) => eprintln!(
                            "[e2e] dialog {:?} after {} ms",
                            d.heading(),
                            t.elapsed().as_millis()
                        ),
                        None => eprintln!("[e2e] waitdialog: timed out"),
                    }
                    sleep(300).await;
                }
                "respond" => match win.as_ref().and_then(open_alert) {
                    Some(dialog) => {
                        ANSWERED.with(|a| a.borrow_mut().push(dialog.downgrade()));
                        // Click the button, as a user would (emitting
                        // `response` alone leaves the dialog open).
                        let label = dialog.response_label(arg);
                        let button = descendants(dialog.upcast_ref())
                            .into_iter()
                            .filter_map(|c| c.downcast::<gtk::Button>().ok())
                            .find(|b| b.label().as_deref() == Some(label.as_str()));
                        match button {
                            Some(b) => b.emit_clicked(),
                            None => eprintln!("[e2e] respond: no button {arg:?}"),
                        }
                    }
                    None => eprintln!("[e2e] respond: no open dialog"),
                },
                "popovers" => {
                    sleep(300).await;
                    for p in win
                        .iter()
                        .flat_map(|w| descendants(w.upcast_ref()))
                        .filter(|c| c.is::<gtk::Popover>() && c.is_mapped())
                    {
                        for sw in descendants(&p)
                            .into_iter()
                            .filter_map(|c| c.downcast::<gtk::ScrolledWindow>().ok())
                        {
                            let adj = sw.vadjustment();
                            let cut = adj.upper() > adj.page_size() + 0.5;
                            eprintln!(
                                "[e2e] popover {}x{}: content {} of {} px{}",
                                p.width(),
                                p.height(),
                                adj.page_size(),
                                adj.upper(),
                                if cut { " CUT OFF" } else { "" }
                            );
                        }
                    }
                }
                "toplevels" => {
                    for w in toplevels() {
                        eprintln!(
                            "[e2e] toplevel {} {:?} visible {} modal {} transient-for {:?}",
                            w.type_().name(),
                            title_of(&w),
                            w.is_visible(),
                            w.is_modal(),
                            w.transient_for().map(|t| title_of(&t))
                        );
                    }
                }
                "shottop" => {
                    let (text, path) = arg.split_once(':').unwrap_or((arg, ""));
                    sleep(300).await;
                    match toplevels()
                        .into_iter()
                        .find(|w| w.is_visible() && title_of(w).contains(text))
                    {
                        Some(w) => screenshot(&w, path),
                        None => eprintln!("[e2e] shottop: no toplevel {text:?}"),
                    }
                }
                "chooser" => {
                    if !choose_file(arg) {
                        eprintln!("[e2e] chooser: no open file chooser");
                    }
                    sleep(300).await;
                }
                "close" => {
                    if let Some(w) = &win {
                        w.close();
                    }
                    current.replace(live(&opened).last().cloned());
                }
                "dark" => {
                    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark)
                }
                "light" => {
                    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceLight)
                }
                "quit" => break,
                _ => {
                    let target = win.as_ref().and_then(target_of);
                    let known = match target {
                        Some(t) => t.step(cmd.to_owned(), arg.to_owned()).await,
                        None => false,
                    };
                    if !known {
                        eprintln!(
                            "[e2e] unknown step {step:?} for window {:?}",
                            win.as_ref().map(title_of).unwrap_or_default()
                        );
                    }
                }
            }
        }
        eprintln!(
            "[e2e] done after {} ms: {pass} passed, {fail} failed",
            t0.elapsed().as_millis()
        );
        drop(hold);
        for w in ctx.app.windows() {
            w.close();
        }
        ctx.app.quit();
    });
}

pub async fn sleep(ms: u64) {
    glib::timeout_future(Duration::from_millis(ms)).await;
}

fn live(opened: &RefCell<Vec<glib::WeakRef<gtk::Window>>>) -> Vec<gtk::Window> {
    opened
        .borrow()
        .iter()
        .filter_map(|w| w.upgrade())
        .filter(|w| w.application().is_some())
        .collect()
}

fn toplevels() -> Vec<gtk::Window> {
    let list = gtk::Window::toplevels();
    (0..list.n_items())
        .filter_map(|i| list.item(i).and_downcast::<gtk::Window>())
        .collect()
}

/// Drive an in-process file chooser dialog (what `gtk::FileDialog` shows
/// outside a sandbox): select `path` and accept, or cancel if empty.
#[allow(deprecated)]
fn choose_file(path: &str) -> bool {
    let Some(dialog) = toplevels()
        .into_iter()
        .filter(|w| w.is_visible())
        .find_map(|w| w.downcast::<gtk::FileChooserDialog>().ok())
    else {
        return false;
    };
    if path.is_empty() {
        dialog.response(gtk::ResponseType::Cancel);
        return true;
    }
    let file = gtk::gio::File::for_path(path);
    if let Some(dir) = file.parent() {
        let _ = dialog.set_current_folder(Some(&dir));
    }
    if let Some(name) = file.basename() {
        dialog.set_current_name(&name.to_string_lossy());
    }
    dialog.response(gtk::ResponseType::Accept);
    true
}

fn title_of(w: &gtk::Window) -> String {
    w.title().map(|t| t.to_string()).unwrap_or_default()
}

/// The subtitle of the window's `AdwWindowTitle`, if it has one.
fn subtitle_of(w: &gtk::Window) -> Option<String> {
    descendants(w.upcast_ref())
        .into_iter()
        .find_map(|c| c.downcast::<adw::WindowTitle>().ok())
        .map(|t| t.subtitle().to_string())
}

fn descendants(root: &gtk::Widget) -> Vec<gtk::Widget> {
    let mut out = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(w) = stack.pop() {
        let mut child = w.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            out.push(c.clone());
            stack.push(c);
        }
    }
    out
}

/// The alert dialog open on `w`, else on any toplevel (a dialog presented
/// with no parent window gets its own).
fn open_alert(w: &gtk::Window) -> Option<adw::AlertDialog> {
    let answered = |d: &adw::AlertDialog| {
        ANSWERED.with(|a| a.borrow().iter().any(|w| w.upgrade().as_ref() == Some(d)))
    };
    let find = |w: &gtk::Window| {
        descendants(w.upcast_ref())
            .into_iter()
            .filter_map(|c| c.downcast::<adw::AlertDialog>().ok())
            .find(|d| !answered(d))
    };
    find(w).or_else(|| toplevels().iter().filter(|t| t.is_visible()).find_map(find))
}

fn fill(w: &gtk::Window, label: &str, value: &str) -> bool {
    let Some(dialog) = open_alert(w) else {
        return false;
    };
    for c in descendants(dialog.upcast_ref()) {
        if let Some(row) = c.downcast_ref::<adw::SwitchRow>() {
            if row.title() == label {
                row.set_active(value == "on");
                return true;
            }
        } else if let Some(row) = c.downcast_ref::<adw::EntryRow>()
            && row.title() == label
        {
            row.set_text(value);
            return true;
        }
    }
    false
}

/// Render `window` and the popovers open in it to a PNG file.
pub fn screenshot(window: &impl IsA<gtk::Widget>, path: &str) {
    let widget = window.upcast_ref::<gtk::Widget>();
    let (w, h) = (widget.width(), widget.height());
    let snap = gtk::Snapshot::new();
    gtk::WidgetPaintable::new(Some(widget)).snapshot(&snap, f64::from(w), f64::from(h));
    // Popovers are separate surfaces; draw the visible ones on top.
    for popover in descendants(widget)
        .into_iter()
        .filter(|c| c.is::<gtk::Popover>() && c.is_mapped())
    {
        let Some(bounds) = popover.compute_bounds(widget) else {
            continue;
        };
        // Cross-surface bounds include the window's CSD shadow margin.
        let (sx, sy) = widget
            .native()
            .map_or((0.0, 0.0), |n| n.surface_transform());
        snap.save();
        snap.translate(&gtk::graphene::Point::new(
            bounds.x() - sx as f32,
            bounds.y() - sy as f32,
        ));
        gtk::WidgetPaintable::new(Some(&popover)).snapshot(
            &snap,
            f64::from(bounds.width()),
            f64::from(bounds.height()),
        );
        snap.restore();
    }
    let Some(node) = snap.to_node() else {
        eprintln!("[e2e] shot: nothing to render");
        return;
    };
    let Some(renderer) = widget.native().and_then(|n| n.renderer()) else {
        return;
    };
    let texture = renderer.render_texture(
        &node,
        Some(&gtk::graphene::Rect::new(0.0, 0.0, w as f32, h as f32)),
    );
    match texture.save_to_png(path) {
        Ok(()) => eprintln!("[e2e] wrote {path}"),
        Err(e) => eprintln!("[e2e] shot: {e}"),
    }
}
