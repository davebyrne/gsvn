//! `EntryStore`: an append-mostly `GListModel` of [`EntryObject`]s backed
//! by a `Vec`.
//!
//! `gio::ListStore` keeps its items in a `GSequence` (a balanced tree), so
//! each `item(i)` is an O(log n) lookup and each item costs a 40-byte tree
//! node. The sort and filter models above the file list call `item(i)` for
//! every entry on every re-sort and re-filter; at 100k entries the vector
//! makes that several times faster and saves the tree nodes (DESIGN.md §13).

use super::entry::EntryObject;
use gtk::{gio, glib, prelude::*, subclass::prelude::*};

mod imp {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    pub struct EntryStore {
        pub items: RefCell<Vec<EntryObject>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for EntryStore {
        const NAME: &'static str = "GsvnBrowserEntryStore";
        type Type = super::EntryStore;
        type Interfaces = (gio::ListModel,);
    }

    impl ObjectImpl for EntryStore {}

    impl ListModelImpl for EntryStore {
        fn item_type(&self) -> glib::Type {
            EntryObject::static_type()
        }

        fn n_items(&self) -> u32 {
            self.items.borrow().len() as u32
        }

        fn item(&self, position: u32) -> Option<glib::Object> {
            self.items.borrow().get(position as usize).map(|o| o.clone().upcast())
        }
    }
}

glib::wrapper! {
    pub struct EntryStore(ObjectSubclass<imp::EntryStore>) @implements gio::ListModel;
}

impl Default for EntryStore {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl EntryStore {
    pub fn len(&self) -> u32 {
        self.imp().items.borrow().len() as u32
    }

    pub fn extend_from_slice(&self, objs: &[EntryObject]) {
        if objs.is_empty() {
            return;
        }
        let pos = {
            let mut items = self.imp().items.borrow_mut();
            let pos = items.len() as u32;
            items.extend_from_slice(objs);
            pos
        };
        // Emitted after the borrow ends: handlers read the model.
        self.items_changed(pos, 0, objs.len() as u32);
    }

    pub fn remove_all(&self) {
        let removed = {
            let mut items = self.imp().items.borrow_mut();
            let n = items.len() as u32;
            // Release the capacity too: a reload may be much smaller.
            *items = Vec::new();
            n
        };
        if removed > 0 {
            self.items_changed(0, removed, 0);
        }
    }

    /// Trim spare capacity once a listing is complete.
    pub fn shrink_to_fit(&self) {
        self.imp().items.borrow_mut().shrink_to_fit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;
    use svn_backend::RepoUrl;

    #[test]
    fn append_and_clear_notify() {
        let store = EntryStore::default();
        let log = Rc::new(RefCell::new(Vec::new()));
        {
            let log = log.clone();
            store.connect_items_changed(move |m, pos, removed, added| {
                // The model is readable from the handler.
                log.borrow_mut().push((pos, removed, added, m.n_items()));
            });
        }
        let dir = Rc::new(RepoUrl::parse("http://h/r").unwrap());
        let a = EntryObject::placeholder(super::super::entry::EntryKind::Loading, "a", &dir, 1);
        let b = EntryObject::placeholder(super::super::entry::EntryKind::Loading, "b", &dir, 1);
        store.extend_from_slice(&[a.clone()]);
        store.extend_from_slice(&[]);
        store.extend_from_slice(&[b.clone()]);
        assert_eq!(store.len(), 2);
        assert_eq!(store.item(1).and_downcast::<EntryObject>().unwrap().name(), "b");
        assert!(store.item(2).is_none());
        store.remove_all();
        assert_eq!(*log.borrow(), vec![(0, 0, 1, 1), (1, 0, 1, 2), (0, 2, 0, 0)]);
    }
}
