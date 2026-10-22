//! `EntryObject`: one directory entry (or a tree placeholder row) as a
//! GObject, shared between the file list and the folder tree.

use super::nav;
use gtk::glib;
use gtk::subclass::prelude::*;
use std::cmp::Ordering;
use std::rc::Rc;
use svn_backend::{Entry, NodeKind, RepoUrl, RevNum, Timestamp};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Dir,
    File,
    /// Tree placeholder while a directory is being listed.
    Loading,
    /// Tree placeholder for a failed listing; the name is the message.
    Error,
}

/// One entry's data. Kept compact: a 100k-entry directory holds 100k of
/// these (DESIGN.md §13).
#[derive(Debug)]
pub struct EntryData {
    pub name: Box<str>,
    pub kind: EntryKind,
    /// The directory containing this entry (shared by all its siblings).
    pub dir: Rc<RepoUrl>,
    pub size: Option<u64>,
    pub rev: RevNum,
    /// Interned: a directory has few distinct authors.
    pub author: Option<Rc<str>>,
    pub date: Option<Timestamp>,
    /// [`nav::natural_key`] of the name; `None` when it equals the name.
    key: Option<Box<str>>,
    /// Tree depth below the repository root (root = 0).
    pub depth: u32,
    /// Set for the root node only (its URL is not `dir` + `name`).
    pub self_url: Option<Box<RepoUrl>>,
}

impl EntryData {
    /// [`nav::natural_key`] of the name.
    pub fn key(&self) -> &str {
        self.key.as_deref().unwrap_or(&self.name)
    }
}

/// The natural key of `name`, stored only if it differs from the name.
fn key_of(name: &str) -> Option<Box<str>> {
    let key = nav::natural_key(name);
    (key != name).then(|| key.into_boxed_str())
}

/// Share one allocation per distinct author string (per thread; the
/// browser only builds entries on the main thread).
fn intern_author(author: Option<String>) -> Option<Rc<str>> {
    thread_local! {
        static AUTHORS: std::cell::RefCell<std::collections::HashSet<Rc<str>>> = Default::default();
    }
    let author = author?;
    Some(AUTHORS.with_borrow_mut(|set| match set.get(author.as_str()) {
        Some(rc) => rc.clone(),
        None => {
            let rc: Rc<str> = Rc::from(author);
            set.insert(rc.clone());
            rc
        }
    }))
}

mod imp {
    use super::*;
    use glib::{ParamSpec, ParamSpecBoolean, Value};
    use gtk::prelude::*;
    use std::cell::{Cell, OnceCell};
    use std::sync::OnceLock;

    #[derive(Default)]
    pub struct EntryObject {
        pub data: OnceCell<EntryData>,
        /// The directory is known to have no subdirectories (hides the
        /// tree expander).
        pub childless: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for EntryObject {
        const NAME: &'static str = "GsvnBrowserEntry";
        type Type = super::EntryObject;
    }

    impl ObjectImpl for EntryObject {
        fn properties() -> &'static [ParamSpec] {
            static PROPS: OnceLock<Vec<ParamSpec>> = OnceLock::new();
            PROPS.get_or_init(|| {
                vec![ParamSpecBoolean::builder("childless").explicit_notify().build()]
            })
        }

        fn property(&self, _id: usize, pspec: &ParamSpec) -> Value {
            match pspec.name() {
                "childless" => self.childless.get().to_value(),
                _ => unreachable!(),
            }
        }

        fn set_property(&self, _id: usize, value: &Value, pspec: &ParamSpec) {
            if pspec.name() == "childless" {
                let v = value.get().unwrap_or(false);
                if self.childless.replace(v) != v {
                    self.obj().notify("childless");
                }
            }
        }
    }
}

glib::wrapper! {
    pub struct EntryObject(ObjectSubclass<imp::EntryObject>);
}

impl EntryObject {
    fn with_data(data: EntryData) -> Self {
        let obj: Self = glib::Object::new();
        let _ = obj.imp().data.set(data);
        obj
    }

    pub fn new(entry: Entry, dir: &Rc<RepoUrl>, depth: u32) -> Self {
        Self::with_data(EntryData {
            key: key_of(&entry.name),
            name: entry.name.into_boxed_str(),
            kind: match entry.kind {
                NodeKind::Dir => EntryKind::Dir,
                NodeKind::File => EntryKind::File,
            },
            dir: dir.clone(),
            size: entry.size,
            rev: entry.last_changed.rev,
            author: intern_author(entry.last_changed.author),
            date: entry.last_changed.date,
            depth,
            self_url: None,
        })
    }

    /// The repository root node of the tree.
    pub fn root(root: &RepoUrl) -> Self {
        let name = root.name();
        let dir = Rc::new(root.clone());
        Self::with_data(EntryData {
            key: key_of(&name),
            name: name.into_boxed_str(),
            kind: EntryKind::Dir,
            dir,
            size: None,
            rev: 0,
            author: None,
            date: None,
            depth: 0,
            self_url: Some(Box::new(root.clone())),
        })
    }

    pub fn placeholder(kind: EntryKind, text: &str, dir: &Rc<RepoUrl>, depth: u32) -> Self {
        Self::with_data(EntryData {
            name: text.into(),
            kind,
            dir: dir.clone(),
            size: None,
            rev: 0,
            author: None,
            date: None,
            key: Some(Box::from("")),
            depth,
            self_url: None,
        })
    }

    pub fn data(&self) -> &EntryData {
        self.imp().data.get().expect("EntryObject without data")
    }

    pub fn name(&self) -> &str {
        &self.data().name
    }

    pub fn kind(&self) -> EntryKind {
        self.data().kind
    }

    pub fn is_dir(&self) -> bool {
        self.kind() == EntryKind::Dir
    }

    pub fn is_placeholder(&self) -> bool {
        matches!(self.kind(), EntryKind::Loading | EntryKind::Error)
    }

    /// Full URL of this entry. For the root node, the root URL.
    pub fn url(&self) -> RepoUrl {
        let d = self.data();
        match &d.self_url {
            Some(url) => (**url).clone(),
            None => d.dir.join(&d.name),
        }
    }

    pub fn is_childless(&self) -> bool {
        self.imp().childless.get()
    }

    pub fn set_childless(&self, v: bool) {
        use gtk::prelude::ObjectExt;
        self.set_property("childless", v);
    }
}

/// Comparison helpers for sorters. `a`/`b` are entries; non-entries
/// compare equal.
pub fn cmp_by(a: &glib::Object, b: &glib::Object, f: impl Fn(&EntryData, &EntryData) -> Ordering) -> Ordering {
    match (a.downcast_ref::<EntryObject>(), b.downcast_ref::<EntryObject>()) {
        (Some(a), Some(b)) => f(a.data(), b.data()),
        _ => Ordering::Equal,
    }
}

use gtk::prelude::Cast;

/// Natural name order.
pub fn cmp_name(a: &EntryData, b: &EntryData) -> Ordering {
    nav::natural_cmp(a.key(), &a.name, b.key(), &b.name)
}

/// Directories before files (and placeholders last).
pub fn cmp_dirs_first(a: &EntryData, b: &EntryData) -> Ordering {
    fn rank(k: EntryKind) -> u8 {
        match k {
            EntryKind::Dir => 0,
            EntryKind::File => 1,
            EntryKind::Loading | EntryKind::Error => 2,
        }
    }
    rank(a.kind).cmp(&rank(b.kind))
}
