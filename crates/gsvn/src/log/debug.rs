//! Debug-build scripting hook for the log window, used to verify paging,
//! timings and rendering without a human at the screen.
//!
//! `GSVN_LOG_SCRIPT="wait;load:5000;rss;shot:/tmp/a.png;quit"` runs the
//! `;`-separated steps against the first log window opened:
//!
//! | step | effect |
//! |---|---|
//! | `wait` | until no page is loading |
//! | `load:N` | scroll to the end repeatedly until N rows are loaded (or the end) |
//! | `search:TEXT` / `range:NEW,OLD` / `stoponcopy` | set the top bar, then wait |
//! | `select:I` / `select2:I,J` / `selectrev:R` | select rows |
//! | `waitpaths` | until the changed paths are shown |
//! | `filter:TEXT` / `selectpath:I` | filter / select changed paths |
//! | `action:NAME` | activate `log.NAME` |
//! | `shotnewest:PATH.png` | render the most recent other window |
//! | `scroll:I` | scroll row I into view |
//! | `light` / `dark` | colour scheme |
//! | `sleep:MS`, `rss`, `shot:PATH.png`, `quit` | |

use super::objects::RevisionItem;
use super::window::LogWindow;
use adw::prelude::*;
use gtk::glib;
use std::rc::Rc;
use std::time::{Duration, Instant};

pub fn maybe_run(win: &Rc<LogWindow>) {
    thread_local! {
        static STARTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    let Ok(script) = std::env::var("GSVN_LOG_SCRIPT") else { return };
    if STARTED.with(|s| s.replace(true)) {
        return;
    }
    let win = Rc::downgrade(win);
    glib::spawn_future_local(async move {
        let t0 = Instant::now();
        for step in script.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            let Some(win) = win.upgrade() else { return };
            let (cmd, arg) = step.split_once(':').unwrap_or((step, ""));
            let t = Instant::now();
            run_step(&win, cmd, arg).await;
            eprintln!(
                "[gsvn-log] step {step:?} took {} ms (t={} ms, rows={})",
                t.elapsed().as_millis(),
                t0.elapsed().as_millis(),
                win.store.n_items()
            );
        }
    });
}

async fn sleep(ms: u64) {
    glib::timeout_future(Duration::from_millis(ms)).await;
}

fn busy(win: &LogWindow) -> bool {
    let st = win.state.borrow();
    st.page_task.is_some()
        || (st.pages_loaded == 0
            && win.list_stack.visible_child_name().as_deref() == Some("loading")
            && win.content_stack.visible_child_name().as_deref() != Some("error"))
}

async fn wait_idle(win: &LogWindow) {
    sleep(50).await;
    while busy(win) {
        sleep(20).await;
    }
}

async fn run_step(win: &Rc<LogWindow>, cmd: &str, arg: &str) {
    match cmd {
        "wait" => wait_idle(win).await,
        "sleep" => sleep(arg.parse().unwrap_or(500)).await,
        "load" => {
            let want: u32 = arg.parse().unwrap_or(1000);
            let t = Instant::now();
            loop {
                wait_idle(win).await;
                if win.store.n_items() >= want || win.state.borrow().pager.is_done() {
                    break;
                }
                let adj = win.scroller.vadjustment();
                adj.set_value(adj.upper() - adj.page_size());
                sleep(30).await;
            }
            eprintln!(
                "[gsvn-log] loaded {} rows in {} ms",
                win.store.n_items(),
                t.elapsed().as_millis()
            );
        }
        "search" => {
            win.search.set_text(arg);
            sleep(400).await;
            wait_idle(win).await;
        }
        "range" => {
            let (a, b) = arg.split_once(',').unwrap_or((arg, ""));
            set_range(win, a, b);
            sleep(700).await;
            wait_idle(win).await;
        }
        "stoponcopy" => {
            win.stop_on_copy.set_active(!win.stop_on_copy.is_active());
            wait_idle(win).await;
        }
        "select" => {
            win.selection.select_item(arg.parse().unwrap_or(0), true);
        }
        "select2" => {
            let mut it = arg.split(',').filter_map(|s| s.parse::<u32>().ok());
            if let (Some(a), Some(b)) = (it.next(), it.next()) {
                win.selection.select_item(a, true);
                win.selection.select_item(b, false);
            }
        }
        "selectrev" => {
            let rev: u64 = arg.parse().unwrap_or(0);
            let pos = (0..win.store.n_items()).find(|&i| {
                win.store
                    .item(i)
                    .and_downcast::<RevisionItem>()
                    .is_some_and(|item| item.entry().rev == rev)
            });
            match pos {
                Some(pos) => {
                    win.selection.select_item(pos, true);
                    win.revisions.scroll_to(pos, None, gtk::ListScrollFlags::NONE, None);
                }
                None => eprintln!("[gsvn-log] r{rev} is not loaded"),
            }
        }
        "scroll" => {
            win.revisions
                .scroll_to(arg.parse().unwrap_or(0), None, gtk::ListScrollFlags::NONE, None);
        }
        "waitpaths" => {
            let t = Instant::now();
            sleep(20).await;
            while !matches!(win.paths_stack.visible_child_name().as_deref(), Some("list" | "error")) {
                sleep(10).await;
            }
            eprintln!(
                "[gsvn-log] changed paths visible after {} ms ({} rows)",
                t.elapsed().as_millis(),
                win.paths_store.n_items()
            );
        }
        "filter" => {
            win.paths_filter.set_text(arg);
            sleep(300).await;
        }
        "light" | "dark" => {
            adw::StyleManager::default().set_color_scheme(if cmd == "dark" {
                adw::ColorScheme::ForceDark
            } else {
                adw::ColorScheme::ForceLight
            });
            sleep(300).await;
        }
        "rss" => eprintln!("[gsvn-log] {}", rss()),
        "shot" => {
            sleep(250).await;
            screenshot(win.window.upcast_ref(), arg);
        }
        "shotnewest" => {
            // The most recently opened other window (e.g. a diff placeholder).
            sleep(400).await;
            let newest = win.ctx.app.windows().into_iter().find(|w| w != win.window.upcast_ref::<gtk::Window>());
            match newest {
                Some(w) => screenshot(w.upcast_ref(), arg),
                None => eprintln!("[gsvn-log] no other window"),
            }
        }
        "revmenu" | "pathmenu" => {
            // `revmenu:ROW,PATH.png`: open the context menu on ROW and render it.
            let (row, path) = arg.split_once(',').unwrap_or(("0", arg));
            let row = row.parse().unwrap_or(0);
            let popover = if cmd == "revmenu" {
                win.revision_context_menu(row, win.revisions.upcast_ref(), 200.0, 80.0);
                win.rev_menu.clone()
            } else {
                win.path_context_menu(row, win.paths_view.upcast_ref(), 200.0, 40.0);
                win.path_popover.clone()
            };
            sleep(400).await;
            screenshot(popover.upcast_ref(), path);
            popover.popdown();
        }
        "pageerror" => {
            let mut err = svn_backend::SvnError::new(svn_backend::ErrorKind::Network, "Connection reset by peer (simulated)");
            err.code = Some(175012);
            win.show_page_error(err);
        }
        "selectpath" => {
            win.paths_selection.set_selected(arg.parse().unwrap_or(0));
        }
        "action" => {
            let _ = WidgetExt::activate_action(&win.window, &format!("log.{arg}"), None);
        }
        "quit" => {
            win.window.close();
            win.ctx.app.quit();
        }
        other => eprintln!("[gsvn-log] unknown step {other:?}"),
    }
}

fn set_range(win: &LogWindow, newest: &str, oldest: &str) {
    win.range_newest.set_text(newest);
    win.range_oldest.set_text(oldest);
}

fn rss() -> String {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("VmRSS")).map(|l| l.split_whitespace().collect::<Vec<_>>().join(" ")))
        .unwrap_or_else(|| "VmRSS unknown".to_owned())
}

/// Render `widget` to a PNG file.
fn screenshot(widget: &gtk::Widget, path: &str) {
    let paintable = gtk::WidgetPaintable::new(Some(widget));
    let (w, h) = (widget.width(), widget.height());
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, f64::from(w), f64::from(h));
    let Some(node) = snapshot.to_node() else {
        eprintln!("[gsvn-log] nothing to render");
        return;
    };
    let Some(renderer) = widget.native().map(|n| n.renderer()) else { return };
    let Some(renderer) = renderer else { return };
    let texture = renderer.render_texture(&node, Some(&gtk::graphene::Rect::new(0.0, 0.0, w as f32, h as f32)));
    match texture.save_to_png(path) {
        Ok(()) => eprintln!("[gsvn-log] saved {path} ({w}x{h})"),
        Err(e) => eprintln!("[gsvn-log] saving {path}: {e}"),
    }
}
