//! In-memory, session-scoped LRU caches keyed on immutable
//! `(url, numeric revision)` (DESIGN.md §10).
//!
//! Only results for numeric revisions are stored, so entries never go stale.
//! All caches are behind a `Mutex` and shared by every clone of
//! [`crate::Svn`].

use crate::model::{CommitInfo, Entry, Info, LogEntry, NodeKind, RevNum};
use jiff::Timestamp;
use lru::LruCache;
use std::collections::HashSet;
use std::hash::Hash;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, MutexGuard};

type Key = (String, RevNum);

/// Max number of `info` results kept (they are small).
const SMALL_CACHE_ITEMS: usize = 20_000;

/// Bound of the `log_entry` cache, in changed paths (plus
/// [`LOG_ENTRY_BASE_WEIGHT`] per entry). One revision can change 50k
/// paths (~6 MB), so a count bound alone does not bound memory; this is
/// roughly 25 MB at most.
const LOG_ENTRY_CACHE_WEIGHT: usize = 200_000;
const LOG_ENTRY_BASE_WEIGHT: usize = 4;

/// An LRU bounded by the sum of per-item weights instead of item count.
struct WeightedLru<K: Hash + Eq, V> {
    lru: LruCache<K, (V, usize)>,
    total: usize,
    capacity: usize,
}

impl<K: Hash + Eq, V: Clone> WeightedLru<K, V> {
    fn new(capacity: usize) -> Self {
        Self {
            lru: LruCache::unbounded(),
            total: 0,
            capacity,
        }
    }

    fn get(&mut self, k: &K) -> Option<V> {
        self.lru.get(k).map(|(v, _)| v.clone())
    }

    fn put(&mut self, k: K, v: V, weight: usize) {
        if weight > self.capacity {
            return;
        }
        if let Some((_, old)) = self.lru.put(k, (v, weight)) {
            self.total -= old;
        }
        self.total += weight;
        while self.total > self.capacity {
            match self.lru.pop_lru() {
                Some((_, (_, w))) => self.total -= w,
                None => break,
            }
        }
    }

    fn clear(&mut self) {
        self.lru.clear();
        self.total = 0;
    }
}

/// A directory listing as stored in the list cache: about 64 bytes plus
/// the name per entry, against ~140 for an [`Entry`] with its own author
/// string. Listings can hold hundreds of thousands of entries.
pub(crate) type CachedList = Arc<[CachedEntry]>;

pub(crate) struct CachedEntry {
    name: Box<str>,
    /// Shared within the listing.
    author: Option<Arc<str>>,
    rev: RevNum,
    /// Seconds since the epoch; `i64::MIN` = no date.
    date_s: i64,
    date_ns: i32,
    /// `u64::MAX` = no size.
    size: u64,
    kind: NodeKind,
}

impl CachedEntry {
    pub(crate) fn to_entry(&self) -> Entry {
        Entry {
            name: self.name.to_string(),
            kind: self.kind,
            size: (self.size != u64::MAX).then_some(self.size),
            last_changed: CommitInfo {
                rev: self.rev,
                author: self.author.as_deref().map(str::to_owned),
                date: if self.date_s == i64::MIN {
                    None
                } else {
                    Timestamp::new(self.date_s, self.date_ns).ok()
                },
            },
        }
    }
}

/// Builds a [`CachedList`], sharing author strings.
#[derive(Default)]
pub(crate) struct ListBuilder {
    entries: Vec<CachedEntry>,
    authors: HashSet<Arc<str>>,
}

impl ListBuilder {
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn push(&mut self, e: &Entry) {
        let author = e.last_changed.author.as_deref().map(|a| match self.authors.get(a) {
            Some(rc) => rc.clone(),
            None => {
                let rc: Arc<str> = Arc::from(a);
                self.authors.insert(rc.clone());
                rc
            }
        });
        let (date_s, date_ns) = match e.last_changed.date {
            Some(t) => (t.as_second(), t.subsec_nanosecond()),
            None => (i64::MIN, 0),
        };
        self.entries.push(CachedEntry {
            name: e.name.as_str().into(),
            author,
            rev: e.last_changed.rev,
            date_s,
            date_ns,
            // A real size of u64::MAX would read back as "no size"; svn
            // cannot report one that large.
            size: e.size.unwrap_or(u64::MAX),
            kind: e.kind,
        });
    }

    /// Exactly sized.
    pub(crate) fn finish(self) -> CachedList {
        self.entries.into()
    }
}

pub(crate) struct Cache {
    list: Mutex<WeightedLru<Key, CachedList>>,
    cat: Mutex<WeightedLru<Key, Arc<Vec<u8>>>>,
    info: Mutex<LruCache<Key, Info>>,
    log_entry: Mutex<WeightedLru<Key, LogEntry>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic while holding a cache lock cannot leave the LRU in a state
    // worse than "some entry missing", so poisoning is ignored.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn key(url: &str, rev: RevNum) -> Key {
    (url.to_owned(), rev)
}

impl Cache {
    pub(crate) fn new(list_entries: usize, cat_bytes: usize) -> Self {
        let n = NonZeroUsize::new(SMALL_CACHE_ITEMS).expect("nonzero");
        Self {
            list: Mutex::new(WeightedLru::new(list_entries)),
            cat: Mutex::new(WeightedLru::new(cat_bytes)),
            info: Mutex::new(LruCache::new(n)),
            log_entry: Mutex::new(WeightedLru::new(LOG_ENTRY_CACHE_WEIGHT)),
        }
    }

    pub(crate) fn get_list(&self, url: &str, rev: RevNum) -> Option<CachedList> {
        lock(&self.list).get(&key(url, rev))
    }

    /// Store a *complete* listing. An empty directory still weighs 1 so
    /// that the bound holds for many empty directories.
    pub(crate) fn put_list(&self, url: &str, rev: RevNum, entries: CachedList) {
        let w = entries.len().max(1);
        lock(&self.list).put(key(url, rev), entries, w);
    }

    pub(crate) fn get_cat(&self, url: &str, rev: RevNum) -> Option<Arc<Vec<u8>>> {
        lock(&self.cat).get(&key(url, rev))
    }

    /// Store complete (untruncated) file content.
    pub(crate) fn put_cat(&self, url: &str, rev: RevNum, data: Arc<Vec<u8>>) {
        let w = data.len().max(1);
        lock(&self.cat).put(key(url, rev), data, w);
    }

    pub(crate) fn get_info(&self, url: &str, rev: RevNum) -> Option<Info> {
        lock(&self.info).get(&key(url, rev)).cloned()
    }

    pub(crate) fn put_info(&self, url: &str, rev: RevNum, info: Info) {
        lock(&self.info).put(key(url, rev), info);
    }

    pub(crate) fn get_log_entry(&self, url: &str, rev: RevNum) -> Option<LogEntry> {
        lock(&self.log_entry).get(&key(url, rev))
    }

    pub(crate) fn put_log_entry(&self, url: &str, rev: RevNum, entry: LogEntry) {
        let w = LOG_ENTRY_BASE_WEIGHT + entry.changed_paths.len();
        lock(&self.log_entry).put(key(url, rev), entry, w);
    }

    pub(crate) fn clear(&self) {
        lock(&self.list).clear();
        lock(&self.cat).clear();
        lock(&self.info).clear();
        lock(&self.log_entry).clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> Entry {
        Entry {
            name: name.to_owned(),
            kind: NodeKind::File,
            size: Some(1),
            last_changed: CommitInfo {
                rev: 1,
                author: None,
                date: None,
            },
        }
    }

    fn list(entries: &[Entry]) -> CachedList {
        let mut b = ListBuilder::default();
        for e in entries {
            b.push(e);
        }
        b.finish()
    }

    #[test]
    fn cached_entries_round_trip() {
        let full = Entry {
            name: "näme".to_owned(),
            kind: NodeKind::Dir,
            size: Some(0),
            last_changed: CommitInfo {
                rev: 7,
                author: Some("alice".to_owned()),
                date: Some("2024-01-31T12:34:56.789012Z".parse().unwrap()),
            },
        };
        let empty = Entry {
            size: None,
            last_changed: CommitInfo {
                rev: 0,
                author: None,
                date: None,
            },
            ..entry("x")
        };
        let l = list(&[full.clone(), empty.clone(), full.clone()]);
        assert_eq!(l[0].to_entry(), full);
        assert_eq!(l[1].to_entry(), empty);
        // The author string is shared.
        assert!(Arc::ptr_eq(l[0].author.as_ref().unwrap(), l[2].author.as_ref().unwrap()));
    }

    #[test]
    fn list_bounded_by_entries() {
        let c = Cache::new(5, 100);
        c.put_list("a", 1, list(&[entry("x"), entry("y"), entry("z")]));
        c.put_list("b", 1, list(&[entry("x"), entry("y")]));
        assert!(c.get_list("a", 1).is_some());
        assert!(c.get_list("b", 1).is_some());
        // Touch "a" so "b" is the least recently used, then overflow.
        c.get_list("a", 1);
        c.put_list("c", 1, list(&[entry("x")]));
        assert!(c.get_list("b", 1).is_none());
        assert_eq!(c.get_list("a", 1).unwrap().len(), 3);
        assert!(c.get_list("c", 1).is_some());
        // Larger than the whole cache: not stored.
        c.put_list("d", 1, list(&(0..6).map(|i| entry(&i.to_string())).collect::<Vec<_>>()));
        assert!(c.get_list("d", 1).is_none());
        assert!(c.get_list("a", 2).is_none());
    }

    #[test]
    fn log_entries_bounded_by_changed_paths() {
        use crate::model::{ChangeAction, ChangedPath};
        let entry = |rev: RevNum, paths: usize| LogEntry {
            rev,
            author: None,
            date: None,
            message: String::new(),
            changed_paths: (0..paths)
                .map(|i| ChangedPath {
                    action: ChangeAction::Added,
                    path: format!("/p{i}"),
                    kind: None,
                    copy_from: None,
                    text_mods: false,
                    prop_mods: false,
                })
                .collect(),
        };
        let c = Cache::new(5, 10);
        c.put_log_entry("r", 1, entry(1, 2));
        assert_eq!(c.get_log_entry("r", 1).unwrap().changed_paths.len(), 2);
        // Two huge revisions do not fit together; the older one goes.
        let big = LOG_ENTRY_CACHE_WEIGHT / 2;
        c.put_log_entry("r", 2, entry(2, big));
        c.put_log_entry("r", 3, entry(3, big));
        assert!(c.get_log_entry("r", 2).is_none());
        assert!(c.get_log_entry("r", 3).is_some());
    }

    #[test]
    fn cat_bounded_by_bytes_and_clear() {
        let c = Cache::new(5, 10);
        c.put_cat("a", 1, Arc::new(vec![0; 6]));
        c.put_cat("b", 1, Arc::new(vec![0; 6]));
        assert!(c.get_cat("a", 1).is_none());
        assert!(c.get_cat("b", 1).is_some());
        // Replacing an entry must not double count.
        c.put_cat("b", 1, Arc::new(vec![0; 4]));
        c.put_cat("c", 1, Arc::new(vec![0; 6]));
        assert!(c.get_cat("b", 1).is_some());
        assert!(c.get_cat("c", 1).is_some());
        c.clear();
        assert!(c.get_cat("b", 1).is_none());
        assert!(c.get_cat("c", 1).is_none());
    }
}
