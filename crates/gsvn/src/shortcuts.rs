//! Keyboard shortcuts of every window, and the Keyboard Shortcuts dialog
//! (Ctrl+?, or *Keyboard Shortcuts* in the main menu).
//!
//! The tables below are the single source of truth: each window installs
//! the entries that have an action with [`add_to`], application-wide ones
//! are registered with [`install`], and the dialog lists them all. Entries
//! without an action are handled by a widget (lists, entries, menus) and
//! are listed for reference only.

use adw::prelude::*;
use gtk::gio;

pub struct Shortcut {
    pub title: &'static str,
    /// In `GtkShortcutTrigger` syntax; `|` separates alternatives.
    pub trigger: &'static str,
    /// Action the window's shortcut controller activates; `None` when a
    /// widget handles the keys itself.
    pub action: Option<&'static str>,
}

pub struct Section {
    pub title: &'static str,
    pub shortcuts: &'static [Shortcut],
}

const fn act(title: &'static str, trigger: &'static str, action: &'static str) -> Shortcut {
    Shortcut {
        title,
        trigger,
        action: Some(action),
    }
}

const fn doc(title: &'static str, trigger: &'static str) -> Shortcut {
    Shortcut {
        title,
        trigger,
        action: None,
    }
}

/// Application-wide: registered with `set_accels_for_action`.
pub const GENERAL: Section = Section {
    title: "General",
    shortcuts: &[
        act("New Window", "<Control>n", "app.new-window"),
        act("Close Window", "<Control>w", "window.close"),
        act("Quit", "<Control>q", "app.quit"),
        doc("Main Menu", "F10"),
        act("Keyboard Shortcuts", "<Control>question", "app.shortcuts"),
    ],
};

pub const BROWSER: Section = Section {
    title: "Repository Browser",
    shortcuts: &[
        act("Back", "<Alt>Left", "browser.back"),
        act("Forward", "<Alt>Right", "browser.forward"),
        act("Parent Folder", "<Alt>Up", "browser.up"),
        act("Refresh", "F5|<Control>r", "browser.refresh"),
        act("Edit Location", "<Control>l", "browser.location"),
        act("Filter Files", "<Control>f", "browser.find"),
        act("Show Log", "<Control><Shift>l", "browser.show-log"),
        act("Bookmark This Location", "<Control>d", "browser.bookmark"),
        act("Context Menu", "Menu|<Shift>F10", "browser.context-menu"),
        doc("Open Folder or File", "Return"),
    ],
};

pub const LOG: Section = Section {
    title: "Log",
    shortcuts: &[
        act("Refresh", "F5|<Control>r", "log.refresh"),
        act("Search", "<Control>f", "log.focus-search"),
        doc("Copy Selected Revisions or Path", "<Control>c"),
        doc("Diff with Previous Revision", "Return"),
    ],
};

pub const DIFF: Section = Section {
    title: "Diff",
    shortcuts: &[
        act("Side by Side", "<Control>t", "win.side-by-side"),
        act("Previous File", "<Alt>Up|<Control>Page_Up", "win.prev-file"),
        act("Next File", "<Alt>Down|<Control>Page_Down", "win.next-file"),
    ],
};

pub const VIEWER: Section = Section {
    title: "File Viewer",
    shortcuts: &[
        act("Find", "<Control>f", "viewer.search"),
        act("Next Match", "<Control>g", "viewer.find-next"),
        act(
            "Previous Match",
            "<Control><Shift>g",
            "viewer.find-previous",
        ),
        act("Go to Line", "<Control>i", "viewer.goto-line"),
        act("Show Log", "<Control><Shift>l", "viewer.history"),
        act("Save As", "<Control>s|<Control><Shift>s", "viewer.save-as"),
    ],
};

pub const SECTIONS: [&Section; 5] = [&GENERAL, &BROWSER, &LOG, &DIFF, &VIEWER];

/// Add the section's action shortcuts to a window's controller.
pub fn add_to(controller: &gtk::ShortcutController, section: &Section) {
    for s in section.shortcuts {
        if let Some(action) = s.action {
            controller.add_shortcut(gtk::Shortcut::new(
                gtk::ShortcutTrigger::parse_string(s.trigger),
                Some(gtk::NamedAction::new(action)),
            ));
        }
    }
}

/// Register the application-wide accelerators and the `app.shortcuts`
/// action that opens the dialog.
pub fn install(app: &adw::Application) {
    let action = gio::ActionEntry::builder("shortcuts")
        .activate(|app: &adw::Application, _, _| show(app))
        .build();
    app.add_action_entries([action]);
    for s in GENERAL.shortcuts {
        if let Some(action) = s.action {
            app.set_accels_for_action(action, &accels(s.trigger));
        }
    }
}

/// `"F5|<Control>r"` → `["F5", "<Control>r"]`.
fn accels(trigger: &str) -> Vec<&str> {
    trigger.split('|').collect()
}

fn show(app: &adw::Application) {
    let dialog = adw::ShortcutsDialog::new();
    for section in SECTIONS {
        let group = adw::ShortcutsSection::new(Some(section.title));
        for s in section.shortcuts {
            // The dialog separates alternatives with spaces.
            group.add(adw::ShortcutsItem::new(
                s.title,
                &accels(s.trigger).join(" "),
            ));
        }
        dialog.add(group);
    }
    dialog.present(app.active_window().as_ref());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `gtk_accelerator_parse` needs no display, unlike the checked
    /// binding (which asserts that GTK is initialised).
    fn parse(accel: &str) -> Option<(u32, u32)> {
        let c = std::ffi::CString::new(accel).unwrap();
        let (mut key, mut mods) = (0, 0);
        let ok = unsafe { gtk::ffi::gtk_accelerator_parse(c.as_ptr(), &mut key, &mut mods) };
        (ok != 0 && key != 0).then_some((key, mods))
    }

    #[test]
    fn triggers_parse() {
        for section in SECTIONS {
            for s in section.shortcuts {
                for accel in accels(s.trigger) {
                    assert!(parse(accel).is_some(), "{}: {accel}", s.title);
                }
            }
        }
    }

    #[test]
    fn no_conflicts_within_a_window() {
        // General shortcuts apply in every window, so they must not clash
        // with any window's own.
        for section in &SECTIONS[1..] {
            let mut seen: Vec<((u32, u32), &str)> = Vec::new();
            for s in GENERAL.shortcuts.iter().chain(section.shortcuts) {
                for accel in accels(s.trigger) {
                    let key = parse(accel).unwrap();
                    if let Some((_, other)) = seen.iter().find(|(k, _)| *k == key) {
                        panic!(
                            "{}: {accel} is used by both {other} and {}",
                            section.title, s.title
                        );
                    }
                    seen.push((key, s.title));
                }
            }
        }
    }
}
