//! Bookmarks, recent repositories and window state, persisted as TOML
//! under the XDG config/state dirs (DESIGN.md §5.4, §11). Every mutating
//! call saves immediately.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bookmark {
    pub name: String,
    /// Encoded URL as produced by `RepoUrl::as_str`.
    pub url: String,
    /// Pinned revision, `None` = HEAD.
    pub rev: Option<u64>,
}

pub struct Settings {}

impl Settings {
    /// Load from disk; missing or corrupt files yield defaults (never fails).
    pub fn load() -> Self {
        Settings {}
    }

    pub fn bookmarks(&self) -> Vec<Bookmark> {
        Vec::new()
    }

    /// Adds, or replaces a bookmark with the same `url` and `rev`.
    pub fn add_bookmark(&self, bookmark: Bookmark) {
        let _ = bookmark;
    }

    pub fn remove_bookmark(&self, url: &str, rev: Option<u64>) {
        let _ = (url, rev);
    }

    /// Called after any bookmark change, so menus can refresh.
    pub fn connect_bookmarks_changed(&self, f: impl Fn() + 'static) {
        let _ = f;
    }

    /// Recently opened URLs, most recent first (capped at 20).
    pub fn recent(&self) -> Vec<String> {
        Vec::new()
    }

    pub fn add_recent(&self, url: &str) {
        let _ = url;
    }

    /// Saved default size for a window kind (`"browser"`, `"log"`, ...).
    pub fn window_size(&self, kind: &str) -> Option<(i32, i32)> {
        let _ = kind;
        None
    }

    pub fn set_window_size(&self, kind: &str, width: i32, height: i32) {
        let _ = (kind, width, height);
    }
}
