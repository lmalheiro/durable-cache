// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! The in-flight registry backing streaming partial hits
//! (FDS-STORAGE.PARTIAL-01).
//!
//! The file system does not signal appends, so a partial hit's reader needs
//! the store's valid length and a notification beside the file. A store
//! enters the registry when `get_miss_handler` opens its temporary and
//! leaves it when the store completes (published, or discarded because the
//! backend is read-only or an invalidating purge ended it) or aborts.
//!
//! An invalidating purge marks the key's stores in flight as ended
//! (FRE-COMPLIANCE.PURGE-01). Untagged lookups pass over them from then on,
//! but they stay registered until they complete or abort, so the request
//! writing one still reads it back by its tag, and readers already streaming
//! one end as they would have.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard};

use tokio::sync::Notify;

/// One in-flight store.
pub struct InFlightWriter {
    /// Opening order, and the streaming write tag `pingora-cache` passes
    /// back when the request that opened the store looks it up.
    pub id: u64,
    pub temp_path: std::path::PathBuf,
    /// The store's metadata, exactly as it was serialised into the record,
    /// so a partial hit answers with the object's metadata without reading
    /// the growing temporary again.
    pub internal_meta: Vec<u8>,
    pub header_meta: Vec<u8>,
    /// Body bytes written to the temporary so far. The store writes every
    /// chunk before its `write_body` returns and only then publishes the new
    /// length, so a reader never reads past what is in the file.
    pub valid_len: AtomicU64,
    /// Notified once per appended chunk, and once more when the store ends,
    /// so readers never poll.
    pub notify: Notify,
    /// Set when an invalidating purge of the key ends the store, and never
    /// cleared: untagged lookups pass over the store, and its `finish`
    /// removes its temporary instead of publishing it.
    purged: AtomicBool,
    /// Set once, when the store completes or aborts; empty while it is open.
    outcome: OnceLock<WriteOutcome>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    /// The store completed: it published its record or, because the backend
    /// was read-only or an invalidating purge had ended it, removed its
    /// temporary. Either way a reader delivers the rest and ends normally
    /// (FRE-COMPAT.STREAMEND-01).
    Completed { body_len: u64 },
    /// The store aborted: a reader ends with an error
    /// (FRE-COMPLIANCE.PARTIALABORT-01).
    Aborted,
}

/// Each key's stores in flight, in the order they reached the map, which is
/// not always id order: `begin` takes a store's id before the lock, so two
/// stores of one key opening at once can reach it in either order. A key
/// with no store left has no entry.
type StoresByKey = HashMap<String, Vec<Arc<InFlightWriter>>>;

#[derive(Default)]
pub struct InFlightRegistry {
    by_key: RwLock<StoresByKey>,
    next_id: AtomicU64,
}

impl InFlightRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a store of `name`.
    pub fn begin(
        &self,
        name: &str,
        temp_path: std::path::PathBuf,
        internal_meta: Vec<u8>,
        header_meta: Vec<u8>,
    ) -> Arc<InFlightWriter> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let writer = Arc::new(InFlightWriter {
            id,
            temp_path,
            internal_meta,
            header_meta,
            valid_len: AtomicU64::new(0),
            notify: Notify::new(),
            purged: AtomicBool::new(false),
            outcome: OnceLock::new(),
        });
        // The key is copied before the lock is taken, so the lock is held
        // only for the insertion.
        let key = name.to_owned();
        self.write_map()
            .entry(key)
            .or_default()
            .push(Arc::clone(&writer));
        writer
    }

    /// The most recently opened store of `name` still in flight that no
    /// invalidating purge has ended (FRE-COMPAT.STREAMING-01): the one with
    /// the highest id, which is not always the last in the list (see
    /// `StoresByKey`).
    pub fn most_recent(&self, name: &str) -> Option<Arc<InFlightWriter>> {
        self.read_map().get(name).and_then(|writers| {
            writers
                .iter()
                .filter(|w| !w.purged())
                .max_by_key(|w| w.id)
                .cloned()
        })
    }

    /// The store `lookup_streaming_write`'s tag names, if it is still in
    /// flight, even once a purge has ended it: the request writing a store
    /// reads its own body back through this lookup. A tagged lookup resolves
    /// to that store or to nothing, never to another writer's body.
    pub fn by_tag(&self, name: &str, tag: u64) -> Option<Arc<InFlightWriter>> {
        self.read_map()
            .get(name)
            .and_then(|writers| writers.iter().find(|w| w.id == tag).cloned())
    }

    /// End every store of `name` in flight, for an invalidating purge of the
    /// key (FRE-COMPLIANCE.PURGE-01): untagged lookups pass over them from
    /// now on, and each one's `finish` removes its temporary instead of
    /// publishing it. The caller holds the key's publication lock, which a
    /// store's `finish` takes to publish, so each store either published
    /// before this call or sees it. Returns whether any store was in flight.
    pub fn purge(&self, name: &str) -> bool {
        match self.read_map().get(name) {
            Some(writers) => {
                for writer in writers {
                    writer.purged.store(true, Ordering::Release);
                }
                !writers.is_empty()
            }
            None => false,
        }
    }

    /// Deregister a store. Entries with no stores left are pruned so the
    /// map does not grow unbounded. Each store ends once, after it began; a
    /// debug build checks it, and a release build ignores a store that is
    /// not there, since this runs from the miss handler's `Drop`, where a
    /// panic during unwinding would abort the process.
    pub fn end(&self, name: &str, id: u64) {
        let mut map = self.write_map();
        let mut removed = None;
        if let Some(writers) = map.get_mut(name) {
            removed = writers
                .iter()
                .position(|w| w.id == id)
                .map(|at| writers.remove(at));
            if writers.is_empty() {
                map.remove(name);
            }
        }
        // Released first, so that the check's panic cannot poison the map.
        drop(map);
        debug_assert!(
            removed.is_some(),
            "store {id} of {name} ended while not in flight"
        );
    }

    /// The map, shared.
    ///
    /// An `RwLock` is poisoned only by a panic while it is held for writing,
    /// and nothing run under this one's write lock panics today: `begin`
    /// and `end` only insert, remove and prune entries, and `HashMap` and
    /// `Vec` panic only when a collection outgrows the address space,
    /// leaving themselves valid. Should a later change make a panic
    /// possible, the registry stays usable, and the first acquisition after
    /// the panic reports it and clears the poison, as the publication locks
    /// do. Several threads can hold the map shared at once, so more than one
    /// of them may report it.
    fn read_map(&self) -> RwLockReadGuard<'_, StoresByKey> {
        self.by_key.read().unwrap_or_else(|poisoned| {
            log::error!("an earlier panic poisoned the in-flight registry; it stays in use");
            self.by_key.clear_poison();
            poisoned.into_inner()
        })
    }

    /// The map, exclusively; poisoning is handled as in
    /// [`InFlightRegistry::read_map`].
    fn write_map(&self) -> RwLockWriteGuard<'_, StoresByKey> {
        self.by_key.write().unwrap_or_else(|poisoned| {
            log::error!("an earlier panic poisoned the in-flight registry; it stays in use");
            self.by_key.clear_poison();
            poisoned.into_inner()
        })
    }
}

impl InFlightWriter {
    /// Publish a new valid length and wake every reader. The store calls
    /// this after each chunk is in the file, never before.
    pub(crate) fn publish_valid_len(&self, written: u64) {
        self.valid_len.store(written, Ordering::Release);
        self.notify.notify_waiters();
    }

    /// Record how the store ended, and wake every reader. It is recorded
    /// after the store's last valid length. A store ends once; a debug build
    /// checks it.
    pub fn set_outcome(&self, outcome: WriteOutcome) {
        let first = self.outcome.set(outcome).is_ok();
        debug_assert!(first, "store {} ended twice", self.id);
        self.notify.notify_waiters();
    }

    /// The store's terminal state, or `None` while it is open. A load, not a
    /// lock: a partial-hit reader checks it on each pass of its read loop.
    pub fn outcome(&self) -> Option<WriteOutcome> {
        self.outcome.get().copied()
    }

    /// Whether an invalidating purge of the key has ended the store.
    pub fn purged(&self) -> bool {
        self.purged.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn begin(r: &InFlightRegistry, key: &str) -> Arc<InFlightWriter> {
        r.begin(key, format!("{key}.tmp").into(), vec![], vec![])
    }

    /// A store with a chosen id, for building a list out of id order.
    fn writer(id: u64) -> Arc<InFlightWriter> {
        Arc::new(InFlightWriter {
            id,
            temp_path: format!("{id}.tmp").into(),
            internal_meta: vec![],
            header_meta: vec![],
            valid_len: AtomicU64::new(0),
            notify: Notify::new(),
            purged: AtomicBool::new(false),
            outcome: OnceLock::new(),
        })
    }

    /// Two stores of one key opening at once can reach the map out of id
    /// order, as `begin` takes the id before the lock. The later id is still
    /// the most recent, and each store is found and ended by its own id.
    #[test]
    fn stores_reaching_the_map_out_of_id_order_are_found_by_id() {
        let registry = InFlightRegistry::new();
        registry
            .write_map()
            .entry("abc".to_owned())
            .or_default()
            .extend([writer(6), writer(5)]);
        assert_eq!(registry.most_recent("abc").expect("two stores").id, 6);
        assert_eq!(registry.by_tag("abc", 5).expect("the earlier store").id, 5);
        registry.end("abc", 5);
        assert_eq!(registry.most_recent("abc").expect("one store left").id, 6);
        assert!(
            registry.by_tag("abc", 5).is_none(),
            "the ended store is gone"
        );
    }

    /// A reader registers for the next notification before it checks the
    /// store's state, as the partial-hit read loop does. A new valid length,
    /// and then the store's outcome, each wake it, and each is visible to
    /// it once woken.
    #[tokio::test]
    async fn a_reader_registered_before_a_change_is_woken_by_it() {
        let registry = InFlightRegistry::new();
        let w = begin(&registry, "abc");

        let notified = w.notify.notified();
        w.publish_valid_len(9);
        tokio::time::timeout(Duration::from_secs(1), notified)
            .await
            .expect("a new valid length wakes a reader registered before it");
        assert_eq!(w.valid_len.load(Ordering::Acquire), 9);

        let notified = w.notify.notified();
        w.set_outcome(WriteOutcome::Completed { body_len: 9 });
        tokio::time::timeout(Duration::from_secs(1), notified)
            .await
            .expect("the outcome wakes a reader registered before it");
        assert_eq!(w.outcome(), Some(WriteOutcome::Completed { body_len: 9 }));
    }

    /// Of two stores of one key in flight, the one opened later is the most
    /// recent (FRE-COMPAT.STREAMING-01).
    #[test]
    fn most_recent_prefers_the_later_store() {
        let registry = InFlightRegistry::new();
        let first = begin(&registry, "abc");
        let second = begin(&registry, "abc");
        assert_ne!(second.id, first.id);
        assert_eq!(
            registry
                .most_recent("abc")
                .expect("two stores in flight")
                .id,
            second.id
        );
    }

    /// A tag finds the store it names, even when a later store of the key is
    /// in flight, and nothing once its store has ended; the later store
    /// stays the most recent.
    #[test]
    fn by_tag_finds_only_its_own_store() {
        let registry = InFlightRegistry::new();
        let first = begin(&registry, "abc");
        let second = begin(&registry, "abc");
        assert_eq!(
            registry
                .by_tag("abc", first.id)
                .expect("the first store")
                .id,
            first.id
        );
        registry.end("abc", first.id);
        assert!(registry.by_tag("abc", first.id).is_none());
        assert_eq!(
            registry.most_recent("abc").expect("the second store").id,
            second.id
        );
    }

    /// An invalidating purge ends a key's stores in flight for untagged
    /// lookups only: the most recent store is then one opened after the
    /// purge, or none, while a tag still finds its own store, which stays
    /// registered until it ends. A key with no store has nothing to end.
    #[test]
    fn a_purge_hides_a_keys_stores_from_untagged_lookups_only() {
        let registry = InFlightRegistry::new();
        let ended = begin(&registry, "abc");
        assert!(registry.purge("abc"), "a store was in flight");
        assert!(ended.purged());
        assert!(
            registry.most_recent("abc").is_none(),
            "an untagged lookup passes over the ended store"
        );
        assert_eq!(
            registry.by_tag("abc", ended.id).expect("its own store").id,
            ended.id,
            "the request writing it still finds it by its tag"
        );
        let later = begin(&registry, "abc");
        assert!(
            !later.purged(),
            "a store opened after the purge is not ended"
        );
        assert_eq!(
            registry
                .most_recent("abc")
                .expect("the store opened after the purge")
                .id,
            later.id
        );
        assert!(!registry.purge("xyz"), "no store of that key is in flight");
    }

    /// A key's entry goes with its last store, so the map does not grow.
    #[test]
    fn end_prunes_empty_entries() {
        let registry = InFlightRegistry::new();
        let first = begin(&registry, "abc");
        let second = begin(&registry, "abc");
        registry.end("abc", first.id);
        assert!(registry.read_map().contains_key("abc"), "one store is left");
        registry.end("abc", second.id);
        assert!(registry.read_map().is_empty(), "the entry went with it");
    }

    /// A panic while the map is held for writing poisons it. The registry
    /// stays usable, and the next acquisition, shared or exclusive, clears
    /// the poison.
    #[test]
    fn a_poisoned_registry_stays_usable_and_is_cleared() {
        let registry = InFlightRegistry::new();
        let poison = || {
            let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _map = registry.by_key.write().expect("not poisoned yet");
                panic!("a bug while the map is held");
            }));
            assert!(panicked.is_err(), "the closure panicked");
            assert!(registry.by_key.is_poisoned(), "the panic poisoned the map");
        };

        poison();
        let w = begin(&registry, "abc");
        assert!(
            !registry.by_key.is_poisoned(),
            "an exclusive acquisition clears it"
        );

        poison();
        assert_eq!(
            registry
                .most_recent("abc")
                .expect("the store is registered")
                .id,
            w.id
        );
        assert!(
            !registry.by_key.is_poisoned(),
            "a shared acquisition clears it"
        );
    }

    /// A store that ends twice, or ends without having begun, is a bug that
    /// a debug build catches.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "ended while not in flight")]
    fn a_store_ending_twice_is_caught_in_a_debug_build() {
        let registry = InFlightRegistry::new();
        let w = begin(&registry, "abc");
        registry.end("abc", w.id);
        registry.end("abc", w.id);
    }

    /// So is a store whose outcome is recorded twice.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "ended twice")]
    fn an_outcome_recorded_twice_is_caught_in_a_debug_build() {
        let registry = InFlightRegistry::new();
        let w = begin(&registry, "abc");
        w.set_outcome(WriteOutcome::Aborted);
        w.set_outcome(WriteOutcome::Aborted);
    }
}
