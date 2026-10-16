//! Bookmarks, recent repositories and window state, persisted as TOML
//! under the XDG config/state dirs (DESIGN.md §5.4, §11). Every mutating
//! call saves immediately.
//!
//! - `$XDG_CONFIG_HOME/gsvn/bookmarks.toml`: bookmarks and recent URLs
//! - `$XDG_STATE_HOME/gsvn/state.toml`: window sizes
//!
//! Setting `GSVN_CONFIG_DIR` puts both files in that directory instead
//! (handy for testing without touching the real settings).

use gtk::glib;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
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

#[derive(Debug, Default, Serialize, Deserialize)]
struct StateFile {
    #[serde(default)]
    windows: BTreeMap<String, WindowState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct WindowState {
    width: i32,
    height: i32,
}

pub struct Settings {
    bookmarks_path: PathBuf,
    state_path: PathBuf,
    data: RefCell<BookmarksFile>,
    state: RefCell<StateFile>,
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

    pub fn remove_recent(&self, url: &str) {
        self.data.borrow_mut().recent.retain(|u| u != url);
        self.save_bookmarks();
    }

    pub fn clear_recent(&self) {
        self.data.borrow_mut().recent.clear();
        self.save_bookmarks();
    }

    /// Saved default size for a window kind (`"browser"`, `"log"`, ...).
    pub fn window_size(&self, kind: &str) -> Option<(i32, i32)> {
        self.state
            .borrow()
            .windows
            .get(kind)
            .filter(|s| s.width > 0 && s.height > 0)
            .map(|s| (s.width, s.height))
    }

    pub fn set_window_size(&self, kind: &str, width: i32, height: i32) {
        if width <= 0 || height <= 0 {
            return;
        }
        let new = WindowState { width, height };
        let changed = self.state.borrow_mut().windows.insert(kind.to_owned(), new) != Some(new);
        if changed {
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

        s.remove_recent("https://h/24");
        assert_eq!(load(&dir).recent()[1], "https://h/23");
        s.clear_recent();
        assert!(load(&dir).recent().is_empty());
    }

    #[test]
    fn window_sizes_persist() {
        let dir = tempfile::tempdir().unwrap();
        let s = load(&dir);
        s.set_window_size("log", 800, 600);
        s.set_window_size("bad", 0, 600);
        assert_eq!(load(&dir).window_size("log"), Some((800, 600)));
        assert_eq!(load(&dir).window_size("bad"), None);
        assert!(dir.path().join("state").join(STATE_FILE).exists());
        assert!(!dir.path().join("config").join(BOOKMARKS_FILE).exists());
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
