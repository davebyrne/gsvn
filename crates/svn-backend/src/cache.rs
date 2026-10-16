//! In-memory, session-scoped LRU caches keyed on immutable
//! `(url, numeric revision)` (DESIGN.md §10).
//!
//! Only results for numeric revisions are stored, so entries never go stale.
//! All caches are behind a `Mutex` and shared by every clone of
//! [`crate::Svn`].

use crate::model::{Entry, Info, LogEntry, RevNum};
use lru::LruCache;
use std::hash::Hash;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, MutexGuard};

type Key = (String, RevNum);

/// Max number of `info` / `log_entry` results kept (they are small).
const SMALL_CACHE_ITEMS: usize = 20_000;

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

pub(crate) struct Cache {
    list: Mutex<WeightedLru<Key, Arc<Vec<Entry>>>>,
    cat: Mutex<WeightedLru<Key, Arc<Vec<u8>>>>,
    info: Mutex<LruCache<Key, Info>>,
    log_entry: Mutex<LruCache<Key, LogEntry>>,
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
            log_entry: Mutex::new(LruCache::new(n)),
        }
    }

    pub(crate) fn get_list(&self, url: &str, rev: RevNum) -> Option<Arc<Vec<Entry>>> {
        lock(&self.list).get(&key(url, rev))
    }

    /// Store a *complete* listing. An empty directory still weighs 1 so
    /// that the bound holds for many empty directories.
    pub(crate) fn put_list(&self, url: &str, rev: RevNum, entries: Arc<Vec<Entry>>) {
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
        lock(&self.log_entry).get(&key(url, rev)).cloned()
    }

    pub(crate) fn put_log_entry(&self, url: &str, rev: RevNum, entry: LogEntry) {
        lock(&self.log_entry).put(key(url, rev), entry);
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
    use crate::model::{CommitInfo, NodeKind};

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

    #[test]
    fn list_bounded_by_entries() {
        let c = Cache::new(5, 100);
        c.put_list("a", 1, Arc::new(vec![entry("x"), entry("y"), entry("z")]));
        c.put_list("b", 1, Arc::new(vec![entry("x"), entry("y")]));
        assert!(c.get_list("a", 1).is_some());
        assert!(c.get_list("b", 1).is_some());
        // Touch "a" so "b" is the least recently used, then overflow.
        c.get_list("a", 1);
        c.put_list("c", 1, Arc::new(vec![entry("x")]));
        assert!(c.get_list("b", 1).is_none());
        assert_eq!(c.get_list("a", 1).unwrap().len(), 3);
        assert!(c.get_list("c", 1).is_some());
        // Larger than the whole cache: not stored.
        c.put_list("d", 1, Arc::new((0..6).map(|i| entry(&i.to_string())).collect()));
        assert!(c.get_list("d", 1).is_none());
        assert!(c.get_list("a", 2).is_none());
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
