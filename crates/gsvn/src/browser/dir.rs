//! Per-window directory cache. One [`DirModel`] per directory URL (at the
//! window's revision) holds the listing for both the file list (`store`,
//! all entries) and the folder tree (`dirs`, directories plus a
//! placeholder row while loading), so each directory is listed once.

use super::entry::{EntryKind, EntryObject};
use gtk::{gio, glib, prelude::*};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::task::{Poll, Waker};
use std::time::{Duration, Instant};
use svn_backend::{CancelToken, Priority, RepoUrl, RevNum, Svn, SvnError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadState {
    Idle,
    Loading,
    Done,
    Cancelled,
    Failed(SvnError),
}

/// Called after every change of a model's contents or state.
pub type Listener = Rc<dyn Fn(&Rc<DirModel>)>;

pub struct DirModel {
    pub url: Rc<RepoUrl>,
    /// Tree depth of the directory itself (root = 0); children are +1.
    pub depth: u32,
    /// All entries (file list).
    pub store: gio::ListStore,
    /// Subdirectories, plus a trailing placeholder while loading/failed.
    pub dirs: gio::ListStore,
    state: RefCell<LoadState>,
    prio: Cell<Priority>,
    /// Batches received in the current attempt.
    batches: Cell<u32>,
    generation: Cell<u64>,
    token: RefCell<Option<CancelToken>>,
    placeholder: RefCell<Option<EntryObject>>,
    /// The tree node representing this directory (to mark it childless).
    pub owner: RefCell<glib::WeakRef<EntryObject>>,
    /// The tree row of this directory is expanded.
    pub expanded: Cell<bool>,
    wakers: RefCell<Vec<Waker>>,
    pub started: Cell<Option<Instant>>,
    pub first_rows: Cell<Option<Duration>>,
    svn: Svn,
    rev: RevNum,
    listener: Listener,
}

impl DirModel {
    pub fn state(&self) -> LoadState {
        self.state.borrow().clone()
    }

    pub fn is_loading(&self) -> bool {
        matches!(*self.state.borrow(), LoadState::Loading)
    }

    pub fn is_done(&self) -> bool {
        matches!(*self.state.borrow(), LoadState::Done)
    }

    pub fn n_items(&self) -> u32 {
        self.store.n_items()
    }

    /// Start listing unless already loaded or loading. A background load
    /// that has not delivered anything yet is restarted when the user
    /// starts waiting for it (`Interactive`), as it may still be queued
    /// behind other work.
    pub fn ensure(self: &Rc<Self>, prio: Priority) {
        match &*self.state.borrow() {
            LoadState::Done => return,
            LoadState::Loading
                if prio != Priority::Interactive
                    || self.prio.get() == Priority::Interactive
                    || self.batches.get() > 0 =>
            {
                return;
            }
            _ => {}
        }
        self.start(prio);
    }

    /// Discard everything and list again.
    pub fn reload(self: &Rc<Self>, prio: Priority) {
        self.start(prio);
    }

    fn start(self: &Rc<Self>, prio: Priority) {
        if let Some(token) = self.token.take() {
            token.cancel();
        }
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        self.store.remove_all();
        self.dirs.remove_all();
        self.set_placeholder(Some((EntryKind::Loading, "Loading…")));
        self.prio.set(prio);
        self.batches.set(0);
        self.started.set(Some(Instant::now()));
        self.first_rows.set(None);
        *self.state.borrow_mut() = LoadState::Loading;

        let mut stream = self.svn.list(&self.url, self.rev, prio);
        *self.token.borrow_mut() = Some(stream.token());
        (self.listener)(self);

        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            loop {
                let item = stream.next().await;
                let Some(this) = weak.upgrade() else { return };
                if this.generation.get() != generation {
                    return;
                }
                match item {
                    Some(Ok(batch)) => {
                        this.append(batch);
                        drop(this);
                        // Let GTK draw between batches (DESIGN.md §5.2).
                        glib::timeout_future_with_priority(glib::Priority::DEFAULT_IDLE, Duration::ZERO).await;
                    }
                    Some(Err(err)) => return this.finish(Err(err)),
                    None => return this.finish(Ok(())),
                }
            }
        });
    }

    fn append(self: &Rc<Self>, batch: Vec<svn_backend::Entry>) {
        let child_depth = self.depth + 1;
        let objs: Vec<EntryObject> = batch
            .into_iter()
            .map(|e| EntryObject::new(e, &self.url, child_depth))
            .collect();
        let dirs: Vec<EntryObject> = objs.iter().filter(|o| o.is_dir()).cloned().collect();
        self.store.extend_from_slice(&objs);
        if !dirs.is_empty() {
            let pos = self.dirs.n_items() - u32::from(self.placeholder.borrow().is_some());
            self.dirs.splice(pos, 0, &dirs);
        }
        if self.batches.get() == 0
            && let Some(t) = self.started.get()
        {
            self.first_rows.set(Some(t.elapsed()));
        }
        self.batches.set(self.batches.get() + 1);
        (self.listener)(self);
    }

    fn finish(self: &Rc<Self>, result: Result<(), SvnError>) {
        self.token.take();
        let state = match result {
            Ok(()) => {
                self.set_placeholder(None);
                if self.dirs.n_items() == 0
                    && let Some(owner) = self.owner.borrow().upgrade()
                {
                    owner.set_childless(true);
                }
                LoadState::Done
            }
            Err(err) if err.is_cancelled() => {
                self.set_placeholder(None);
                LoadState::Cancelled
            }
            Err(err) => {
                self.set_placeholder(Some((EntryKind::Error, &err.message)));
                LoadState::Failed(err)
            }
        };
        *self.state.borrow_mut() = state;
        self.wake();
        (self.listener)(self);
    }

    /// Stop listing; keeps what arrived so far (state `Cancelled`).
    pub fn cancel(self: &Rc<Self>) {
        if !self.is_loading() {
            return;
        }
        if let Some(token) = self.token.take() {
            token.cancel();
        }
        self.generation.set(self.generation.get() + 1);
        self.finish(Err(SvnError::cancelled()));
    }

    /// Cancel silently without notifying (window closing).
    fn abort(&self) {
        if let Some(token) = self.token.take() {
            token.cancel();
        }
        self.generation.set(self.generation.get() + 1);
    }

    fn set_placeholder(&self, what: Option<(EntryKind, &str)>) {
        if let Some(old) = self.placeholder.take()
            && let Some(pos) = self.dirs.find(&old)
        {
            self.dirs.remove(pos);
        }
        if let Some((kind, text)) = what {
            let p = EntryObject::placeholder(kind, text, &self.url, self.depth + 1);
            self.dirs.append(&p);
            *self.placeholder.borrow_mut() = Some(p);
        }
    }

    fn wake(&self) {
        for w in self.wakers.take() {
            w.wake();
        }
    }

    /// Wait until the model is not loading any more.
    pub async fn wait_idle(self: &Rc<Self>) {
        std::future::poll_fn(|cx| {
            if self.is_loading() {
                self.wakers.borrow_mut().push(cx.waker().clone());
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        })
        .await
    }

    /// Subdirectory called `name`, from `dirs`.
    pub fn find_dir(&self, name: &str) -> Option<EntryObject> {
        self.dirs
            .iter::<EntryObject>()
            .flatten()
            .find(|e| e.is_dir() && e.name() == name)
    }
}

/// All directory models of one window at one revision.
pub struct DirCache {
    pub root: RepoUrl,
    pub rev: RevNum,
    svn: Svn,
    models: RefCell<HashMap<RepoUrl, Rc<DirModel>>>,
    listener: Listener,
}

impl DirCache {
    pub fn new(svn: Svn, root: RepoUrl, rev: RevNum, listener: Listener) -> Self {
        Self {
            root,
            rev,
            svn,
            models: RefCell::default(),
            listener,
        }
    }

    /// The model for `url`, created (not loaded) if needed.
    pub fn get(&self, url: &RepoUrl) -> Rc<DirModel> {
        if let Some(m) = self.models.borrow().get(url) {
            return m.clone();
        }
        let depth = super::nav::segments(&self.root, url).len() as u32;
        let model = Rc::new(DirModel {
            url: Rc::new(url.clone()),
            depth,
            store: gio::ListStore::new::<EntryObject>(),
            dirs: gio::ListStore::new::<EntryObject>(),
            state: RefCell::new(LoadState::Idle),
            prio: Cell::new(Priority::Background),
            batches: Cell::new(0),
            generation: Cell::new(0),
            token: RefCell::default(),
            placeholder: RefCell::default(),
            owner: RefCell::default(),
            expanded: Cell::new(false),
            wakers: RefCell::default(),
            started: Cell::new(None),
            first_rows: Cell::new(None),
            svn: self.svn.clone(),
            rev: self.rev,
            listener: self.listener.clone(),
        });
        self.models.borrow_mut().insert(url.clone(), model.clone());
        model
    }

    pub fn peek(&self, url: &RepoUrl) -> Option<Rc<DirModel>> {
        self.models.borrow().get(url).cloned()
    }

    /// Forget a model (e.g. a URL that turned out to be a file).
    pub fn remove(&self, url: &RepoUrl) {
        if let Some(m) = self.models.borrow_mut().remove(url) {
            m.abort();
        }
    }

    /// Cancel all loads (window closing or revision change).
    pub fn abort_all(&self) {
        for m in self.models.borrow().values() {
            m.abort();
        }
    }

    /// Whether `url` is known to be a directory: the root, or listed as a
    /// directory in its parent.
    pub fn known_dir(&self, url: &RepoUrl) -> bool {
        if *url == self.root {
            return true;
        }
        let (Some(parent), name) = (url.parent(), url.name()) else {
            return false;
        };
        self.peek(&parent).is_some_and(|m| m.find_dir(&name).is_some())
    }
}

/// Weak handle helper for listener closures.
pub fn weak_listener<T: 'static>(target: &Rc<T>, f: impl Fn(&Rc<T>, &Rc<DirModel>) + 'static) -> Listener {
    let weak: Weak<T> = Rc::downgrade(target);
    Rc::new(move |m| {
        if let Some(t) = weak.upgrade() {
            f(&t, m);
        }
    })
}
