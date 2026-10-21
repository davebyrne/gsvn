//! List-model items: one GObject per log entry and per changed path.

use gtk::glib;
use gtk::subclass::prelude::*;
use svn_backend::{ChangedPath, LogEntry};

mod imp {
    use super::*;
    use std::cell::OnceCell;

    #[derive(Default)]
    pub struct RevisionItem {
        pub entry: OnceCell<LogEntry>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for RevisionItem {
        const NAME: &'static str = "GsvnLogRevisionItem";
        type Type = super::RevisionItem;
    }

    impl ObjectImpl for RevisionItem {}

    #[derive(Default)]
    pub struct PathItem {
        pub path: OnceCell<ChangedPath>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PathItem {
        const NAME: &'static str = "GsvnLogPathItem";
        type Type = super::PathItem;
    }

    impl ObjectImpl for PathItem {}
}

glib::wrapper! {
    /// A row of the revision list.
    pub struct RevisionItem(ObjectSubclass<imp::RevisionItem>);
}

impl RevisionItem {
    pub fn new(entry: LogEntry) -> Self {
        let item: Self = glib::Object::new();
        let _ = item.imp().entry.set(entry);
        item
    }

    pub fn entry(&self) -> &LogEntry {
        self.imp().entry.get().expect("RevisionItem without entry")
    }
}

glib::wrapper! {
    /// A row of the changed-paths list.
    pub struct PathItem(ObjectSubclass<imp::PathItem>);
}

impl PathItem {
    pub fn new(path: ChangedPath) -> Self {
        let item: Self = glib::Object::new();
        let _ = item.imp().path.set(path);
        item
    }

    pub fn path(&self) -> &ChangedPath {
        self.imp().path.get().expect("PathItem without path")
    }
}
