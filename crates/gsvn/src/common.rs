//! Small helpers shared by all windows.

use crate::app::Ctx;
use adw::prelude::*;
use sourceview5::prelude::*;
use gtk::glib;
use std::rc::Rc;
use svn_backend::{ErrorKind, SvnError, Timestamp};

/// Files larger than this need confirmation before preview/blame (DESIGN.md §8).
pub const PREVIEW_CONFIRM_BYTES: u64 = 2 * 1024 * 1024;

/// Run a future on the GTK main loop.
pub fn spawn<F: std::future::Future<Output = ()> + 'static>(f: F) {
    glib::spawn_future_local(f);
}

/// Create an application window whose size is restored from and saved to
/// settings under `kind` (`"browser"`, `"log"`, `"diff"`, `"viewer"`, `"start"`).
pub fn new_window(ctx: &Rc<Ctx>, kind: &'static str, title: &str, default_size: (i32, i32)) -> adw::ApplicationWindow {
    let (w, h) = ctx.settings.window_size(kind).unwrap_or(default_size);
    let window = adw::ApplicationWindow::builder()
        .application(&ctx.app)
        .title(title)
        .default_width(w)
        .default_height(h)
        .build();
    let ctx = ctx.clone();
    window.connect_close_request(move |win| {
        if !win.is_maximized() {
            let (w, h) = win.default_size();
            ctx.settings.set_window_size(kind, w, h);
        }
        glib::Propagation::Proceed
    });
    window
}

pub fn format_size(bytes: u64) -> String {
    glib::format_size(bytes).to_string()
}

/// "just now", "5 minutes ago", "3 days ago", ...
pub fn format_relative(ts: Timestamp) -> String {
    let secs = Timestamp::now().as_second() - ts.as_second();
    if secs < 0 {
        return format_datetime(ts);
    }
    let plural = |n: i64, unit: &str| {
        if n == 1 {
            format!("1 {unit} ago")
        } else {
            format!("{n} {unit}s ago")
        }
    };
    match secs {
        0..60 => "just now".to_owned(),
        60..3600 => plural(secs / 60, "minute"),
        3600..86_400 => plural(secs / 3600, "hour"),
        86_400..604_800 => plural(secs / 86_400, "day"),
        604_800..2_629_746 => plural(secs / 604_800, "week"),
        2_629_746..31_556_952 => plural(secs / 2_629_746, "month"),
        _ => plural(secs / 31_556_952, "year"),
    }
}

/// Local date and time, `2024-01-31 14:05`.
pub fn format_datetime(ts: Timestamp) -> String {
    glib::DateTime::from_unix_local(ts.as_second())
        .and_then(|dt| dt.format("%Y-%m-%d %H:%M"))
        .map(|s| s.to_string())
        .unwrap_or_else(|_| ts.to_string())
}

/// Heuristic binary check: non-text `svn:mime-type`, or NUL in the first 8 KB.
pub fn is_binary(data: &[u8], mime_type: Option<&str>) -> bool {
    if let Some(mime) = mime_type {
        if !mime.starts_with("text/") && !mime.is_empty() {
            return true;
        }
    }
    data[..data.len().min(8192)].contains(&0)
}

/// A status page describing an error, with an optional Retry button and a
/// Details expander showing the svn command and stderr (DESIGN.md §12).
pub fn error_page(err: &SvnError, retry: Option<Box<dyn Fn()>>) -> adw::StatusPage {
    let (icon, title) = match err.kind {
        ErrorKind::NotFound => ("edit-find-symbolic", "Not Found"),
        ErrorKind::Auth => ("dialog-password-symbolic", "Authentication Failed"),
        ErrorKind::Cert => ("channel-insecure-symbolic", "Untrusted Certificate"),
        ErrorKind::Network => ("network-offline-symbolic", "Connection Failed"),
        ErrorKind::SvnNotFound => ("dialog-error-symbolic", "Subversion Not Installed"),
        ErrorKind::Cancelled => ("process-stop-symbolic", "Cancelled"),
        ErrorKind::Parse | ErrorKind::Other => ("dialog-error-symbolic", "Error"),
    };
    let page = adw::StatusPage::builder()
        .icon_name(icon)
        .title(title)
        .description(glib::markup_escape_text(&err.to_string()))
        .build();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_halign(gtk::Align::Center);
    if let Some(retry) = retry {
        let button = gtk::Button::builder()
            .label("Retry")
            .halign(gtk::Align::Center)
            .css_classes(["pill", "suggested-action"])
            .build();
        button.connect_clicked(move |_| retry());
        content.append(&button);
    }
    if !err.command.is_empty() || !err.stderr.is_empty() {
        let details = format!("$ {}\n{}", err.command, err.stderr.trim_end());
        let label = gtk::Label::builder()
            .label(details)
            .selectable(true)
            .wrap(true)
            .xalign(0.0)
            .css_classes(["monospace", "dim-label"])
            .build();
        let expander = gtk::Expander::builder().label("Details").child(&label).build();
        content.append(&expander);
    }
    page.set_child(Some(&content));
    page
}

/// Guess a GtkSourceView language from a file name.
pub fn language_for(file_name: &str) -> Option<sourceview5::Language> {
    sourceview5::LanguageManager::default().guess_language(Some(file_name), None::<&str>)
}

/// Keep a source buffer's style scheme in sync with the light/dark setting.
pub fn follow_style_scheme(buffer: &sourceview5::Buffer) {
    let manager = adw::StyleManager::default();
    let apply = {
        let buffer = buffer.downgrade();
        move |manager: &adw::StyleManager| {
            let Some(buffer) = buffer.upgrade() else { return };
            let id = if manager.is_dark() { "Adwaita-dark" } else { "Adwaita" };
            let scheme = sourceview5::StyleSchemeManager::default().scheme(id);
            buffer.set_style_scheme(scheme.as_ref());
        }
    };
    apply(&manager);
    manager.connect_dark_notify(apply);
}

/// Temporary window for modules that are not implemented yet.
pub fn placeholder_window(ctx: &Rc<Ctx>, title: &str, text: &str) {
    let window = new_window(ctx, "placeholder", title, (600, 400));
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(
        &adw::StatusPage::builder().title(title).description(text).build(),
    ));
    window.set_content(Some(&view));
    window.present();
}
