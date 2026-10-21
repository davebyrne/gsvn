//! Small helpers shared by all windows.

use crate::app::Ctx;
use adw::prelude::*;
use sourceview5::prelude::*;
use gtk::{gio, glib};
use std::rc::Rc;
use svn_backend::{ErrorKind, RepoUrl, RevNum, SvnError, Timestamp};

/// Files larger than this need confirmation before preview/blame (DESIGN.md §8).
pub const PREVIEW_CONFIRM_BYTES: u64 = 2 * 1024 * 1024;

/// Run a future on the GTK main loop.
pub fn spawn<F: std::future::Future<Output = ()> + 'static>(f: F) {
    glib::spawn_future_local(f);
}

/// Create an application window whose size (and maximized state) is
/// restored from and saved to settings under `kind` (`"browser"`, `"log"`,
/// `"diff"`, `"viewer"`, `"start"`). The state file is written when the
/// window closes.
pub fn new_window(ctx: &Rc<Ctx>, kind: &'static str, title: &str, default_size: (i32, i32)) -> adw::ApplicationWindow {
    let (w, h) = ctx.settings.window_size(kind).unwrap_or(default_size);
    let window = adw::ApplicationWindow::builder()
        .application(&ctx.app)
        .title(title)
        .default_width(w)
        .default_height(h)
        .maximized(ctx.settings.window_maximized(kind))
        .build();
    let ctx = ctx.clone();
    window.connect_close_request(move |win| {
        // `default_size` tracks the unmaximized size.
        let (w, h) = win.default_size();
        ctx.settings.set_window_state(kind, w, h, win.is_maximized());
        // After the other close handlers (pane positions) have run.
        let ctx = ctx.clone();
        glib::idle_add_local_once(move || ctx.settings.save_state());
        glib::Propagation::Proceed
    });
    window
}

/// Restore `paned`'s position from settings under `key` and save it when
/// `window` closes.
pub fn persist_pane(ctx: &Rc<Ctx>, window: &impl IsA<gtk::Window>, paned: &gtk::Paned, key: &'static str) {
    if let Some(position) = ctx.settings.pane_position(key) {
        paned.set_position(position);
    }
    let ctx = ctx.clone();
    let paned = paned.downgrade();
    window.connect_close_request(move |_| {
        if let Some(paned) = paned.upgrade() {
            ctx.settings.set_pane_position(key, paned.position());
        }
        glib::Propagation::Proceed
    });
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
    if let Some(mime) = mime_type
        && !mime.starts_with("text/")
        && !mime.is_empty()
    {
        return true;
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
    // Without a content type, names with no extension (`Makefile`) are not
    // recognised.
    let (content_type, _) = gio::content_type_guess(Some(file_name), None);
    sourceview5::LanguageManager::default()
        .guess_language(Some(file_name), Some(content_type.as_str()))
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

/// The shared primary menu (☰) model: New Window, Keyboard Shortcuts,
/// About gsvn. Windows may prepend their own sections with
/// `menu.prepend_section`.
pub fn main_menu() -> gtk::gio::Menu {
    let menu = gtk::gio::Menu::new();
    let section = gtk::gio::Menu::new();
    section.append(Some("_New Window"), Some("app.new-window"));
    menu.append_section(None, &section);
    let section = gtk::gio::Menu::new();
    section.append(Some("_Keyboard Shortcuts"), Some("app.shortcuts"));
    section.append(Some("_About gsvn"), Some("app.about"));
    menu.append_section(None, &section);
    menu
}

/// A primary menu button (opens with F10) showing `menu`, usually
/// [`main_menu`].
pub fn main_menu_button(menu: &gtk::gio::Menu) -> gtk::MenuButton {
    gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .tooltip_text("Main Menu")
        .menu_model(menu)
        .primary(true)
        .build()
}

/// Parse a repository URL typed by the user, with a user-facing error.
pub fn parse_url_input(input: &str) -> Result<RepoUrl, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("Enter a repository URL".to_owned());
    }
    RepoUrl::parse(input)
        .map_err(|_| "Not a Subversion URL — it should start with https:// or http://".to_owned())
}

/// Parse an optional revision typed by the user: empty or `HEAD` → `None`,
/// `1234` or `r1234` → `Some(1234)`.
pub fn parse_rev_input(input: &str) -> Result<Option<RevNum>, String> {
    let input = input.trim();
    if input.is_empty() || input.eq_ignore_ascii_case("head") {
        return Ok(None);
    }
    let digits = input.strip_prefix(['r', 'R']).unwrap_or(input);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("“{input}” is not a revision number"));
    }
    digits
        .parse::<RevNum>()
        .map(Some)
        .map_err(|_| format!("“{input}” is not a revision number"))
}

/// Show a simple message dialog on `parent` (standalone when `None`).
pub fn show_message(parent: Option<&gtk::Widget>, heading: &str, body: &str) {
    let dialog = adw::AlertDialog::new(Some(heading), Some(body));
    dialog.add_response("close", "_Close");
    dialog.set_default_response(Some("close"));
    dialog.set_close_response("close");
    dialog.present(parent);
}

/// `1234567` → `1 234 567` with narrow no-break spaces.
pub fn group_thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 * 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push('\u{202F}');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rev_input() {
        assert_eq!(parse_rev_input(""), Ok(None));
        assert_eq!(parse_rev_input(" HEAD "), Ok(None));
        assert_eq!(parse_rev_input("head"), Ok(None));
        assert_eq!(parse_rev_input("42"), Ok(Some(42)));
        assert_eq!(parse_rev_input("r42"), Ok(Some(42)));
        assert!(parse_rev_input("r").is_err());
        assert!(parse_rev_input("+1").is_err());
        assert!(parse_rev_input("-1").is_err());
        assert!(parse_rev_input("abc").is_err());
    }

    #[test]
    fn url_input() {
        assert_eq!(
            parse_url_input("  https://svn.example.com/repo/ ").unwrap().as_str(),
            "https://svn.example.com/repo"
        );
        assert!(parse_url_input("").is_err());
        assert!(parse_url_input("svn.example.com/repo").is_err());
        assert!(parse_url_input("ftp://x/y").is_err());
    }

    #[test]
    fn thousands() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1234), "1\u{202F}234");
        assert_eq!(group_thousands(1234567), "1\u{202F}234\u{202F}567");
    }
}
