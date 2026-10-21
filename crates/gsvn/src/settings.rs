//! Bookmarks, recent repositories and window state, persisted as TOML
//! under the XDG config/state dirs (DESIGN.md §5.4, §11).
//!
//! - `$XDG_CONFIG_HOME/gsvn/bookmarks.toml`: bookmarks and recent URLs,
//!   saved on every change
//! - `$XDG_STATE_HOME/gsvn/state.toml`: window sizes, pane positions and
//!   view preferences, kept in memory and written by [`Settings::save_state`]
//!   (when a window closes and at shutdown), not on every resize
//!
//! Setting `GSVN_CONFIG_DIR` puts both files in that directory instead
//! (handy for testing without touching the real settings).

use gtk::glib;
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// Maximum number of recent URLs kept.
pub const MAX_RECENT: usize = 20;

const BOOKMARKS_FILE: &str = "bookmarks.toml";
const STATE_FILE: &str = "state.toml";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bookmark {
    pub name: String,
    /// Encoded URL as produced by `RepoUrl::as_str`.
    pub url: String,
    /// Pinned revision, `None` = HEAD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<u64>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct BookmarksFile {
    #[serde(default)]
    recent: Vec<String>,
    #[serde(default, rename = "bookmark")]
    bookmarks: Vec<Bookmark>,
}

#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
struct StateFile {
    #[serde(default)]
    windows: BTreeMap<String, WindowState>,
    /// Paned positions by key (`"browser.sidebar"`, `"log.split"`, ...).
    #[serde(default)]
    panes: BTreeMap<String, i32>,
    #[serde(default)]
    diff: DiffState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct WindowState {
    width: i32,
    height: i32,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    maximized: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct DiffState {
    #[serde(default)]
    side_by_side: bool,
}

pub struct Settings {
    bookmarks_path: PathBuf,
    state_path: PathBuf,
    data: RefCell<BookmarksFile>,
    state: RefCell<StateFile>,
    /// `state` has changes not yet written.
    state_dirty: Cell<bool>,
    listeners: RefCell<Vec<Rc<dyn Fn()>>>,
}

impl Settings {
    /// Load from disk; missing or corrupt files yield defaults (never fails).
    pub fn load() -> Self {
        if let Some(dir) = std::env::var_os("GSVN_CONFIG_DIR").filter(|d| !d.is_empty()) {
            let dir = PathBuf::from(dir);
            return Self::load_from(&dir, &dir);
        }
        Self::load_from(
            &glib::user_config_dir().join("gsvn"),
            &glib::user_state_dir().join("gsvn"),
        )
    }

    /// Load from explicit directories (`bookmarks.toml` in `config_dir`,
    /// `state.toml` in `state_dir`). Used by tests.
    pub fn load_from(config_dir: &Path, state_dir: &Path) -> Self {
        let bookmarks_path = config_dir.join(BOOKMARKS_FILE);
        let state_path = state_dir.join(STATE_FILE);
        let mut data: BookmarksFile = read_toml(&bookmarks_path);
        data.recent.truncate(MAX_RECENT);
        let state: StateFile = read_toml(&state_path);
        Settings {
            bookmarks_path,
            state_path,
            data: RefCell::new(data),
            state: RefCell::new(state),
            state_dirty: Cell::new(false),
            listeners: RefCell::new(Vec::new()),
        }
    }

    pub fn bookmarks(&self) -> Vec<Bookmark> {
        self.data.borrow().bookmarks.clone()
    }

    /// Whether a bookmark with this `url` and `rev` exists.
    pub fn is_bookmarked(&self, url: &str, rev: Option<u64>) -> bool {
        self.data
            .borrow()
            .bookmarks
            .iter()
            .any(|b| b.url == url && b.rev == rev)
    }

    /// Adds, or replaces a bookmark with the same `url` and `rev`.
    pub fn add_bookmark(&self, bookmark: Bookmark) {
        {
            let mut data = self.data.borrow_mut();
            match data
                .bookmarks
                .iter_mut()
                .find(|b| b.url == bookmark.url && b.rev == bookmark.rev)
            {
                Some(existing) => *existing = bookmark,
                None => data.bookmarks.push(bookmark),
            }
        }
        self.save_bookmarks();
        self.emit_bookmarks_changed();
    }

    pub fn remove_bookmark(&self, url: &str, rev: Option<u64>) {
        let removed = {
            let mut data = self.data.borrow_mut();
            let before = data.bookmarks.len();
            data.bookmarks.retain(|b| !(b.url == url && b.rev == rev));
            data.bookmarks.len() != before
        };
        if removed {
            self.save_bookmarks();
            self.emit_bookmarks_changed();
        }
    }

    /// Called after any bookmark change, so menus can refresh.
    pub fn connect_bookmarks_changed(&self, f: impl Fn() + 'static) {
        self.listeners.borrow_mut().push(Rc::new(f));
    }

    /// Recently opened URLs, most recent first (capped at 20).
    pub fn recent(&self) -> Vec<String> {
        self.data.borrow().recent.clone()
    }

    pub fn add_recent(&self, url: &str) {
        {
            let mut data = self.data.borrow_mut();
            data.recent.retain(|u| u != url);
            data.recent.insert(0, url.to_owned());
            data.recent.truncate(MAX_RECENT);
        }
        self.save_bookmarks();
    }

    pub fn clear_recent(&self) {
        self.data.borrow_mut().recent.clear();
        self.save_bookmarks();
    }

    /// Saved default size for a window kind (`"browser"`, `"log"`, ...).
    pub fn window_size(&self, kind: &str) -> Option<(i32, i32)> {
        self.window(kind).map(|s| (s.width, s.height))
    }

    /// Whether the window kind was last closed maximized.
    pub fn window_maximized(&self, kind: &str) -> bool {
        self.window(kind).is_some_and(|s| s.maximized)
    }

    fn window(&self, kind: &str) -> Option<WindowState> {
        self.state
            .borrow()
            .windows
            .get(kind)
            .filter(|s| s.width > 0 && s.height > 0)
            .copied()
    }

    /// Remember a window's unmaximized size and maximized state (written
    /// by [`Settings::save_state`]).
    pub fn set_window_state(&self, kind: &str, width: i32, height: i32, maximized: bool) {
        if width <= 0 || height <= 0 {
            return;
        }
        let new = WindowState { width, height, maximized };
        let changed = self.state.borrow_mut().windows.insert(kind.to_owned(), new) != Some(new);
        self.mark_state(changed);
    }

    /// Saved position of a paned (`"browser.sidebar"`, ...).
    pub fn pane_position(&self, key: &str) -> Option<i32> {
        self.state.borrow().panes.get(key).copied().filter(|&p| p > 0)
    }

    pub fn set_pane_position(&self, key: &str, position: i32) {
        if position <= 0 {
            return;
        }
        let changed = self.state.borrow_mut().panes.insert(key.to_owned(), position) != Some(position);
        self.mark_state(changed);
    }

    /// Whether new diff windows start side by side.
    pub fn diff_side_by_side(&self) -> bool {
        self.state.borrow().diff.side_by_side
    }

    pub fn set_diff_side_by_side(&self, on: bool) {
        let changed = std::mem::replace(&mut self.state.borrow_mut().diff.side_by_side, on) != on;
        self.mark_state(changed);
    }

    fn mark_state(&self, changed: bool) {
        if changed {
            self.state_dirty.set(true);
        }
    }

    /// Write `state.toml` if anything changed since the last write.
    pub fn save_state(&self) {
        if self.state_dirty.replace(false) {
            write_toml(&self.state_path, &*self.state.borrow());
        }
    }

    fn save_bookmarks(&self) {
        write_toml(&self.bookmarks_path, &*self.data.borrow());
    }

    fn emit_bookmarks_changed(&self) {
        // Clone first so callbacks may read settings or connect more handlers.
        let listeners = self.listeners.borrow().clone();
        for f in listeners {
            f();
        }
    }
}

fn read_toml<T: Default + for<'de> Deserialize<'de>>(path: &Path) -> T {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return T::default(),
        Err(e) => {
            glib::g_warning!("gsvn", "Cannot read {}: {e}", path.display());
            return T::default();
        }
    };
    toml::from_str(&text).unwrap_or_else(|e| {
        glib::g_warning!("gsvn", "Ignoring corrupt {}: {e}", path.display());
        T::default()
    })
}

/// Write atomically: temp file in the same directory, then rename.
fn write_toml<T: Serialize>(path: &Path, value: &T) {
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let text = toml::to_string(value)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if let Err(e) = result {
        glib::g_warning!("gsvn", "Cannot save {}: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn bm(name: &str, url: &str, rev: Option<u64>) -> Bookmark {
        Bookmark {
            name: name.into(),
            url: url.into(),
            rev,
        }
    }

    fn load(dir: &tempfile::TempDir) -> Settings {
        Settings::load_from(&dir.path().join("config"), &dir.path().join("state"))
    }

    #[test]
    fn missing_files_give_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let s = load(&dir);
        assert!(s.bookmarks().is_empty());
        assert!(s.recent().is_empty());
        assert_eq!(s.window_size("browser"), None);
    }

    #[test]
    fn bookmarks_roundtrip_and_replace() {
        let dir = tempfile::tempdir().unwrap();
        let s = load(&dir);
        s.add_bookmark(bm("trunk", "https://h/r/trunk", None));
        s.add_bookmark(bm("old", "https://h/r/trunk", Some(5)));
        s.add_bookmark(bm("Trunk!", "https://h/r/trunk", None));
        assert_eq!(
            s.bookmarks(),
            vec![bm("Trunk!", "https://h/r/trunk", None), bm("old", "https://h/r/trunk", Some(5))]
        );
        assert!(s.is_bookmarked("https://h/r/trunk", Some(5)));

        let s2 = load(&dir);
        assert_eq!(s2.bookmarks(), s.bookmarks());

        s2.remove_bookmark("https://h/r/trunk", None);
        assert_eq!(load(&dir).bookmarks(), vec![bm("old", "https://h/r/trunk", Some(5))]);
    }

    #[test]
    fn recent_dedupes_and_caps() {
        let dir = tempfile::tempdir().unwrap();
        let s = load(&dir);
        for i in 0..25 {
            s.add_recent(&format!("https://h/{i}"));
        }
        s.add_recent("https://h/10");
        let r = s.recent();
        assert_eq!(r.len(), MAX_RECENT);
        assert_eq!(r[0], "https://h/10");
        assert_eq!(r[1], "https://h/24");
        assert_eq!(r.iter().filter(|u| *u == "https://h/10").count(), 1);
        assert_eq!(load(&dir).recent(), r);

        s.clear_recent();
        assert!(load(&dir).recent().is_empty());
    }

    #[test]
    fn window_sizes_persist() {
        let dir = tempfile::tempdir().unwrap();
        let s = load(&dir);
        s.set_window_state("log", 800, 600, false);
        s.set_window_state("bad", 0, 600, false);
        // Nothing is written until save_state.
        assert_eq!(load(&dir).window_size("log"), None);
        s.save_state();
        assert_eq!(load(&dir).window_size("log"), Some((800, 600)));
        assert!(!load(&dir).window_maximized("log"));
        assert_eq!(load(&dir).window_size("bad"), None);
        assert!(dir.path().join("state").join(STATE_FILE).exists());
        assert!(!dir.path().join("config").join(BOOKMARKS_FILE).exists());

        s.set_window_state("log", 800, 600, true);
        s.save_state();
        assert!(load(&dir).window_maximized("log"));
    }

    #[test]
    fn panes_and_diff_mode_persist() {
        let dir = tempfile::tempdir().unwrap();
        let s = load(&dir);
        assert_eq!(s.pane_position("browser.sidebar"), None);
        assert!(!s.diff_side_by_side());
        s.set_pane_position("browser.sidebar", 310);
        s.set_pane_position("log.split", 0); // ignored
        s.set_diff_side_by_side(true);
        s.save_state();
        let s2 = load(&dir);
        assert_eq!(s2.pane_position("browser.sidebar"), Some(310));
        assert_eq!(s2.pane_position("log.split"), None);
        assert!(s2.diff_side_by_side());
    }

    #[test]
    fn save_state_writes_only_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join(STATE_FILE);
        let s = load(&dir);
        s.save_state();
        assert!(!path.exists(), "nothing changed, nothing written");
        s.set_pane_position("diff.sidebar", 250);
        s.save_state();
        std::fs::remove_file(&path).unwrap();
        s.set_pane_position("diff.sidebar", 250); // same value
        s.set_diff_side_by_side(false); // default
        s.save_state();
        assert!(!path.exists());
    }

    #[test]
    fn state_file_format() {
        let mut state = StateFile::default();
        state.windows.insert("browser".into(), WindowState { width: 1100, height: 720, maximized: false });
        state.windows.insert("diff".into(), WindowState { width: 900, height: 700, maximized: true });
        state.panes.insert("browser.sidebar".into(), 280);
        state.diff.side_by_side = true;
        let text = toml::to_string(&state).unwrap();
        assert!(text.contains("[windows.browser]\nwidth = 1100\nheight = 720\n"), "{text}");
        assert_eq!(text.matches("maximized").count(), 1, "{text}");
        assert!(text.contains("[panes]\n\"browser.sidebar\" = 280"), "{text}");
        assert!(text.contains("[diff]\nside_by_side = true"), "{text}");
        assert_eq!(toml::from_str::<StateFile>(&text).unwrap(), state);

        // Files from older versions (sizes only) still load.
        let old: StateFile = toml::from_str("[windows.log]\nwidth = 1\nheight = 2\n").unwrap();
        assert_eq!(old.windows["log"], WindowState { width: 1, height: 2, maximized: false });
        assert!(old.panes.is_empty());
        assert!(!old.diff.side_by_side);
    }

    #[test]
    fn corrupt_files_give_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::create_dir_all(dir.path().join("state")).unwrap();
        std::fs::write(dir.path().join("config").join(BOOKMARKS_FILE), "this is [not toml").unwrap();
        std::fs::write(dir.path().join("state").join(STATE_FILE), "windows = 3").unwrap();
        let s = load(&dir);
        assert!(s.bookmarks().is_empty());
        assert_eq!(s.window_size("log"), None);
        assert_eq!(s.pane_position("log.split"), None);
        // Saving overwrites the corrupt file.
        s.add_recent("https://h/r");
        assert_eq!(load(&dir).recent(), vec!["https://h/r".to_owned()]);
    }

    #[test]
    fn file_format() {
        let dir = tempfile::tempdir().unwrap();
        let s = load(&dir);
        s.add_recent("https://h/r");
        s.add_bookmark(bm("a", "https://h/r/a", Some(3)));
        s.add_bookmark(bm("b", "https://h/r/b", None));
        let text = std::fs::read_to_string(dir.path().join("config").join(BOOKMARKS_FILE)).unwrap();
        assert!(text.contains("[[bookmark]]"), "{text}");
        assert!(text.contains("rev = 3"), "{text}");
        assert_eq!(text.matches("rev =").count(), 1, "{text}");
    }

    #[test]
    fn bookmarks_changed_callbacks() {
        let dir = tempfile::tempdir().unwrap();
        let s = Rc::new(load(&dir));
        let count = Rc::new(Cell::new(0));
        {
            let count = count.clone();
            let weak = Rc::downgrade(&s);
            s.connect_bookmarks_changed(move || {
                // Re-entrant reads must work.
                let s = weak.upgrade().unwrap();
                count.set(count.get() + s.bookmarks().len());
            });
        }
        s.add_bookmark(bm("a", "https://h/a", None));
        s.add_bookmark(bm("b", "https://h/b", None));
        s.remove_bookmark("https://h/nope", None); // no change, no signal
        s.remove_bookmark("https://h/a", None);
        s.add_recent("https://h/c"); // recents don't signal
        assert_eq!(count.get(), 1 + 2 + 1);
    }
}
