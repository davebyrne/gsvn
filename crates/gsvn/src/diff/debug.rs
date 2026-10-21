//! Debug-build hooks for developing the diff window without the other
//! windows, measuring it, and rendering it to PNG.
//!
//! ```text
//! GSVN_DEBUG_DIFF="URL REV"            open `-c REV` (instead of the start window)
//! GSVN_DEBUG_DIFF="URL FROM:TO[@PEG]"  open `-r FROM:TO URL@PEG` (PEG defaults to TO)
//! GSVN_DIFF_SELECT=N        select file N once the diff has loaded
//! GSVN_DIFF_SBS=1           switch to side-by-side
//! GSVN_DIFF_SHOW_LARGE=1    press "Show Anyway" on the large-file prompt
//! GSVN_DIFF_DARK=1          force the dark style
//! GSVN_DIFF_SIZE=WxH        window size
//! GSVN_DIFF_SHOT=out.png    render the window to a PNG, then quit
//! GSVN_DIFF_QUIT=1          quit when loaded (after the shot, if any)
//! GSVN_DIFF_TIMING=1        print timings and RSS to stderr
//! GSVN_DIFF_LARGE_ROWS=N    large-file threshold override
//! ```

use super::window::{DiffWindow, FileItem};
use crate::app::Ctx;
use adw::prelude::*;
use gtk::glib;
use std::rc::Rc;
use std::time::Duration;
use svn_backend::{DiffSpec, RepoUrl};

pub struct Hooks {
    select: Option<u32>,
    side_by_side: bool,
    show_large: bool,
    shot: Option<String>,
    quit: bool,
    pub timing: bool,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn parse_spec(s: &str) -> Option<DiffSpec> {
    let (url, revs) = s.trim().rsplit_once(' ')?;
    let url = RepoUrl::parse(url.trim()).ok()?;
    let revs = revs.trim_start_matches('r');
    if let Some((from, rest)) = revs.split_once(':') {
        let (to, peg) = match rest.split_once('@') {
            Some((to, peg)) => (to, Some(peg)),
            None => (rest, None),
        };
        let from = from.trim_start_matches('r').parse().ok()?;
        let to: u64 = to.trim_start_matches('r').parse().ok()?;
        let peg = match peg {
            Some(p) => p.parse().ok()?,
            None => to,
        };
        Some(DiffSpec::Range { url, peg, from, to })
    } else {
        Some(DiffSpec::Change {
            url,
            rev: revs.parse().ok()?,
        })
    }
}

/// Open a diff window from `GSVN_DEBUG_DIFF`; `false` if it isn't set.
pub fn open(ctx: &Rc<Ctx>) -> bool {
    let Some(spec) = env("GSVN_DEBUG_DIFF") else {
        return false;
    };
    let Some(spec) = parse_spec(&spec) else {
        eprintln!("GSVN_DEBUG_DIFF: expected \"URL REV\" or \"URL FROM:TO[@PEG]\"");
        return false;
    };
    if env("GSVN_DIFF_DARK").is_some() {
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
    }
    let hooks = Hooks {
        select: env("GSVN_DIFF_SELECT").and_then(|v| v.parse().ok()),
        side_by_side: env("GSVN_DIFF_SBS").is_some(),
        show_large: env("GSVN_DIFF_SHOW_LARGE").is_some(),
        shot: env("GSVN_DIFF_SHOT"),
        quit: env("GSVN_DIFF_QUIT").is_some(),
        timing: env("GSVN_DIFF_TIMING").is_some(),
    };
    let window = DiffWindow::open(ctx, spec);
    if let Some((w, h)) = env("GSVN_DIFF_SIZE").and_then(|s| {
        let (w, h) = s.split_once('x')?;
        Some((w.parse().ok()?, h.parse().ok()?))
    }) {
        window.gtk_window().set_default_size(w, h);
    }
    window.debug.replace(Some(hooks));
    true
}

fn rss_mb() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("VmRSS:"))
                .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<f64>().ok())
        })
        .map_or(0.0, |kb| kb / 1024.0)
}

/// The stream finished (or failed).
pub fn finished(window: &Rc<DiffWindow>) {
    let Some(hooks) = window.debug.borrow_mut().take() else {
        return;
    };
    if hooks.timing {
        let (first, total, n) = window.timings();
        eprintln!(
            "diff: {n} files, first file after {first:?}, complete after {total:?}, RSS {:.1} MB",
            rss_mb()
        );
    }
    if let Some(i) = hooks.select {
        window.select(i);
    }
    if hooks.side_by_side {
        window.set_side_by_side(true);
    }
    if hooks.show_large
        && let Some(obj) = window.selected_item()
    {
        obj.borrow::<FileItem>().show_large.set(true);
        window.show_selected();
    }
    let window = window.clone();
    let timing = hooks.timing;
    window.debug.replace(Some(Hooks {
        select: None,
        side_by_side: false,
        show_large: false,
        ..hooks
    }));
    glib::timeout_add_local_once(Duration::from_millis(1500), move || {
        if timing {
            eprintln!("diff: RSS after render {:.1} MB", rss_mb());
        }
        finish_shot(window, 10);
    });
}

/// Take the screenshot (retrying while the window is still busy laying
/// out), then quit if asked.
fn finish_shot(window: Rc<DiffWindow>, tries: u32) {
    let shot = window.debug.borrow().as_ref().and_then(|h| h.shot.clone());
    if let Some(path) = &shot {
        match screenshot(window.gtk_window().upcast_ref(), path) {
            Ok(()) => eprintln!("diff: wrote {path}"),
            Err(e) if tries > 0 => {
                eprintln!("diff: screenshot not ready ({e}), retrying");
                glib::timeout_add_local_once(Duration::from_millis(500), move || {
                    finish_shot(window, tries - 1)
                });
                return;
            }
            Err(e) => eprintln!("diff: screenshot failed: {e}"),
        }
    }
    let Some(hooks) = window.debug.take() else {
        return;
    };
    if hooks.quit || hooks.shot.is_some() {
        window.gtk_window().application().inspect(|app| app.quit());
    }
}

/// A file was put into the views.
pub fn rendered(window: &DiffWindow, rows: usize, elapsed: Duration) {
    if window.debug.borrow().as_ref().is_some_and(|h| h.timing)
        || std::env::var_os("GSVN_DIFF_TIMING").is_some()
    {
        eprintln!("diff: rendered {rows} rows in {elapsed:?}");
    }
}

fn screenshot(widget: &gtk::Widget, path: &str) -> Result<(), String> {
    let (w, h) = (widget.width(), widget.height());
    let paintable = gtk::WidgetPaintable::new(Some(widget));
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, f64::from(w), f64::from(h));
    let node = snapshot.to_node().ok_or("nothing rendered")?;
    let renderer = widget
        .native()
        .ok_or("not realized")?
        .renderer()
        .ok_or("no renderer")?;
    let texture = renderer.render_texture(&node, None);
    texture.save_to_png(path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs() {
        assert!(matches!(
            parse_spec("http://h/r 12"),
            Some(DiffSpec::Change { rev: 12, .. })
        ));
        assert!(matches!(
            parse_spec("http://h/r/f.c r3:r9"),
            Some(DiffSpec::Range {
                from: 3,
                to: 9,
                peg: 9,
                ..
            })
        ));
        assert!(matches!(
            parse_spec("http://h/r/f.c 3:9@20"),
            Some(DiffSpec::Range {
                from: 3,
                to: 9,
                peg: 20,
                ..
            })
        ));
        assert!(parse_spec("http://h/r").is_none());
    }
}
