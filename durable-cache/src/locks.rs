// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! The two locks that order the *changes* this process makes, and the
//! [`Change`] that holds them. Both locks live in process memory.
//!
//! **Changes** are a store's final rename, a metadata update or an expiry
//! (written in place, or withdrawing or removing the record), a purge's
//! unlink, and the restart scan's removals of records, temporaries and
//! leases: the *durable* changes, which other readers of the cache
//! directory see. The restart scan's admission checks count as changes too,
//! though they change only this process's eviction manager. Lookups change
//! nothing. One takes a key's publication lock only to read once more a
//! record whose checksums kept failing ([`ChangeLocks::lock_key`]), and
//! never enters the gate.
//!
//! **The read-only gate** (FDS-COMPAT.READONLY-01) decides whether this
//! process may still make changes. Every change enters it and stays inside,
//! alongside any other changes, until it is done.
//! [`ChangeLocks::set_read_only`], called when a hot upgrade hands the
//! directory over to a new process, takes the gate for itself: it waits
//! until the changes inside have ended, then sets the flag. From then on no
//! change can begin, so once it returns, this process changes nothing more.
//! That stops the restart scan's remaining admission checks too, harmlessly:
//! the eviction manager they would update is discarded when the process
//! exits.
//!
//! **The publication lock** (FDS-STORAGE.METAUPDATE-01) is per key. Some
//! changes check a key's canonical file and then act on what they found, and
//! the file must not change in between:
//!
//! - a metadata update or an expiry opens and reads the record, decides,
//!   and writes the new metadata into it in place, or withdraws or removes
//!   the record;
//! - the reclaim scan confirms that the name still refers to the file it
//!   found reclaimable, then unlinks it;
//! - the restart scan confirms that the name still refers to the file it
//!   read, then admits it to the eviction manager; in its last pass, it
//!   removes the entry of a file that has gone.
//!
//! A store's final rename and a purge's unlink take the lock too, so that
//! neither can land inside one of these.
//!
//! **How the two combine.** A change on a key begins with
//! [`ChangeLocks::begin`], which enters the gate, then takes the key's
//! publication lock, and gives back a [`Change`] holding both. Dropping the
//! change releases the key, then the gate. Once the backend is read-only,
//! `begin` gives back nothing, so no key's lock can be taken either. The
//! restart scan's removals of temporaries and leases touch files no key
//! names, so they take the gate alone ([`ChangeLocks::begin_keyless`]).
//!
//! | Who | Read-only gate | Publication lock |
//! |---|---|---|
//! | A change on a key | shared, for the whole change | the key's, inside the gate |
//! | A removal of temporaries or leases | shared | none |
//! | `set_read_only` | exclusive, to set the flag | none |
//! | A lookup | none | its key's, only for a last read ([`ChangeLocks::lock_key`]) |
//!
//! **Rules.** Each one prevents a deadlock.
//!
//! - *The gate, then the key, never the reverse.* A thread that held a key's
//!   lock and then waited for the gate could queue behind a waiting
//!   `set_read_only`, which waits for a change, which waits for that key's
//!   lock. `begin` enters the gate before it takes the key's lock, and a
//!   lookup, which takes a key's lock alone, never enters the gate.
//! - *One key's lock at a time.* Keys share stripes, so a thread taking a
//!   second key's lock could wait for itself, or for a thread that waits for
//!   it. A change takes one key's lock, as it begins; another key's lock
//!   would take another change, which the next rule forbids.
//! - *One change at a time per thread, and no other entry into the gate
//!   while it lasts,* [`ChangeLocks::is_read_only`] and
//!   [`ChangeLocks::set_read_only`] included. With a `set_read_only`
//!   waiting, a second entry may queue behind it while the first holds it
//!   up; `std`'s `RwLock` documents this deadlock. And a `set_read_only`
//!   during a change would wait for that very change. A debug build, which
//!   is what the tests run, checks this rule: either fails an assertion at
//!   once.
//! - *Never across an `.await`.* A [`Change`] is not `Send`, so the compiler
//!   rejects holding one across an `.await` in any future that must be
//!   `Send`.
//!
//! **Between processes,** neither lock orders anything. Two processes write
//! to one directory only during a hot upgrade, before the old one becomes
//! read-only, and FDS-STORAGE.METAUPDATE-01 accepts the race there.

#[cfg(debug_assertions)]
use std::cell::Cell;
use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard};

use crate::sharding::RADIX;

/// How many leading hex digits of a key's name pick its stripe.
const STRIPE_DIGITS: usize = 3;

/// The number of stripes: one for each value of a name's first
/// [`STRIPE_DIGITS`] hex digits. A name is a hash, so two keys share a
/// stripe only by chance, 1 in 4096, and then one waits for the other to
/// finish. Each stripe takes 8 bytes on Linux, 32 KiB in all.
///
/// The stripes are packed, eight to a 64-byte cache line, and deliberately
/// not padded apart. Two keys' stripes share a line only by chance too, 1
/// in 512, and a lock writes its line only as it is taken and released, so
/// the line seldom moves between cores. Measured, padding each stripe to a
/// line of its own gained about 2%, in a synthetic load that took a lock
/// 335,000 times a second, for eight times the memory.
const STRIPES: usize = (RADIX as usize).pow(STRIPE_DIGITS as u32);

// The count the documentation states: a change to the constants above that
// moved it fails the build.
const _: () = assert!(STRIPES == 4096);

/// The locks that order changes: the read-only gate, and the publication
/// locks, one per key, in a fixed table of stripes that keys share
/// according to their names' first digits.
pub struct ChangeLocks {
    /// The read-only gate's flag, `true` once the backend is read-only.
    /// Changes hold the gate shared; `set_read_only` takes it exclusively.
    read_only: RwLock<bool>,
    /// The publication locks' stripes.
    stripes: Vec<Mutex<()>>,
}

impl Default for ChangeLocks {
    fn default() -> Self {
        ChangeLocks {
            read_only: RwLock::new(false),
            stripes: (0..STRIPES).map(|_| Mutex::new(())).collect(),
        }
    }
}

/// A change in progress: the publication lock of its key, if it has one,
/// and the read-only gate, both held until it is dropped, which releases
/// the key first, then the gate. Bind it to a name for as long as the
/// change runs: `let Some(_) = …` would drop it, and its locks, at once.
#[must_use = "a change holds its locks only while it is kept"]
pub struct Change<'a> {
    // Fields are dropped in the order they are declared: the key's lock
    // before the gate.
    _key: Option<MutexGuard<'a, ()>>,
    _gate: RwLockReadGuard<'a, bool>,
}

#[cfg(debug_assertions)]
thread_local! {
    /// Whether this thread holds a [`Change`], in a debug build: the module
    /// doc's third rule.
    static IN_CHANGE: Cell<bool> = const { Cell::new(false) };
}

#[cfg(debug_assertions)]
impl Drop for Change<'_> {
    fn drop(&mut self) {
        IN_CHANGE.with(|in_change| in_change.set(false));
    }
}

impl ChangeLocks {
    /// Begin a change on the key whose files are named `name`: enter the
    /// read-only gate, then take the key's publication lock. `None` once the
    /// backend is read-only.
    pub fn begin(&self, name: &str) -> Option<Change<'_>> {
        self.begin_with(Some(name))
    }

    /// Begin a change to files no key names, the restart scan's removals of
    /// temporaries and leases: enter the read-only gate only. `None` once
    /// the backend is read-only.
    pub fn begin_keyless(&self) -> Option<Change<'_>> {
        self.begin_with(None)
    }

    /// Make the backend read-only. Returns once no change that began before
    /// this call is still in progress; none can begin after it. Idempotent.
    pub fn set_read_only(&self) {
        #[cfg(debug_assertions)]
        IN_CHANGE.with(|in_change| {
            assert!(
                !in_change.get(),
                "set_read_only called while this thread holds a change"
            );
        });
        // Never poisoned today; if it is, reported and cleared as in
        // `read_gate`.
        let mut read_only = self.read_only.write().unwrap_or_else(|poisoned| {
            log::error!("an earlier panic poisoned the read-only gate; it stays in use");
            self.read_only.clear_poison();
            poisoned.into_inner()
        });
        *read_only = true;
    }

    /// Whether the backend is read-only. This enters the gate for a moment,
    /// so a thread that holds a change must not call it.
    pub fn is_read_only(&self) -> bool {
        *self.read_gate()
    }

    /// Begin a change, on the key named `name` if there is one.
    fn begin_with(&self, name: Option<&str>) -> Option<Change<'_>> {
        let gate = self.read_gate();
        if *gate {
            return None;
        }
        // The gate first, then the key: the module doc's first rule.
        let key = name.map(|name| self.lock_key(name));
        #[cfg(debug_assertions)]
        IN_CHANGE.with(|in_change| in_change.set(true));
        Some(Change {
            _key: key,
            _gate: gate,
        })
    }

    /// The read-only gate, held shared. In a debug build, this first checks
    /// that the thread holds no change, which could deadlock: the module
    /// doc's third rule.
    ///
    /// An `RwLock` is poisoned only by a panic while it is held exclusively,
    /// which `set_read_only`, a single assignment, cannot cause today.
    /// Should a later change make it possible, the gate stays usable, and
    /// the first entry after the panic reports it and clears the poison, as
    /// the stripes do. Several threads can hold the gate shared at once, so
    /// more than one of them may report it.
    fn read_gate(&self) -> RwLockReadGuard<'_, bool> {
        #[cfg(debug_assertions)]
        IN_CHANGE.with(|in_change| {
            assert!(
                !in_change.get(),
                "entered the read-only gate while this thread holds a change"
            );
        });
        self.read_only.read().unwrap_or_else(|poisoned| {
            log::error!("an earlier panic poisoned the read-only gate; it stays in use");
            self.read_only.clear_poison();
            poisoned.into_inner()
        })
    }

    /// Take the publication lock of the key whose files are named `name`.
    /// [`ChangeLocks::begin`] takes it inside the gate, for a change. A
    /// reader takes it alone: while it is held, no change on the key is in
    /// progress, and none can begin. A lookup takes it to read once more a
    /// record whose checksums kept failing: every in-place metadata update
    /// holds this lock from its first write to its last, so under it the
    /// record is whole (SDS-LAYOUT.OBJECTFILE-01). A reader changes nothing,
    /// so it does not enter the read-only gate, and this works once the
    /// backend is read-only too.
    ///
    /// The module doc's rules apply: the thread must hold no change, and
    /// begin none until the lock is released. A debug build checks the
    /// first.
    pub fn lock_key(&self, name: &str) -> MutexGuard<'_, ()> {
        #[cfg(debug_assertions)]
        IN_CHANGE.with(|in_change| {
            assert!(
                !in_change.get(),
                "took a key's lock while this thread holds a change"
            );
        });
        // A panic while a stripe is held poisons it, and the stripe stays
        // usable anyway. What the lock protects is the key's files, not the
        // `()` it guards, and a panic leaves them as a crash at the same
        // point would, which the design already handles. Each change made
        // under the lock is a single rename or unlink, which is atomic, except
        // the in-place metadata update: it writes the metadata region, then
        // the header, and a stop between the two leaves a record whose
        // metadata checksum fails, which reads as a miss
        // (ADR-PERSISTENCE.DURABILITY-01, TC-PERSISTENCE.CRASHSAFE-01). The
        // restart scan's admission checks change no file.
        //
        // The first taker after the panic logs it and clears the poison, so
        // each panic is reported once and the takers after it lock cleanly.
        // Clearing is safe here: this thread holds the lock, and only a
        // holder can poison it.
        let stripe = &self.stripes[stripe_index(name)];
        stripe.lock().unwrap_or_else(|poisoned| {
            log::error!(
                "an earlier panic poisoned the publication lock for {name}; it stays in use"
            );
            stripe.clear_poison();
            poisoned.into_inner()
        })
    }
}

/// The stripe of the key whose files are named `name`: the number that its
/// first [`STRIPE_DIGITS`] hex digits stand for. A name is a hash in hex
/// (SDS-LAYOUT.SHARDING-01), so its digits already spread keys evenly, and
/// need no further hashing.
///
/// # Panics
///
/// If `name` does not start with [`STRIPE_DIGITS`] hex digits. Every caller
/// has first derived the key's path from `name`, which needs its first four
/// characters to be hex digits.
fn stripe_index(name: &str) -> usize {
    name.get(..STRIPE_DIGITS)
        .and_then(|digits| usize::from_str_radix(digits, RADIX).ok())
        .expect("a key's name starts with hex digits")
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::{Arc, Barrier, TryLockError};
    use std::time::Duration;

    use pingora_cache::key::{CacheHashKey, CacheKey};

    use super::*;
    use crate::sharding;

    /// A name of the backend's length, 32 hex digits, starting with
    /// `prefix`.
    fn name_starting(prefix: &str) -> String {
        format!("{prefix:0<32}")
    }

    /// A key's stripe is the number its name's first three hex digits stand
    /// for: names that share them share a stripe, whatever follows, and the
    /// lowest and highest digits pick the first and the last stripe.
    #[test]
    fn the_first_three_digits_pick_the_stripe() {
        assert_eq!(stripe_index(&name_starting("abc")), 0xabc);
        assert_eq!(
            stripe_index(&name_starting("abc0ff")),
            stripe_index(&name_starting("abcf00"))
        );
        assert_ne!(
            stripe_index(&name_starting("abc")),
            stripe_index(&name_starting("abd"))
        );
        assert_eq!(stripe_index(&name_starting("000")), 0);
        assert_eq!(stripe_index(&name_starting("fff")), STRIPES - 1);
    }

    /// The names of distinct keys spread over the stripes as random numbers
    /// would: 4096 keys land on about 63% of the 4096 stripes (1 − 1/e). A
    /// stripe choice that gathered keys on a few stripes fails this.
    #[test]
    fn distinct_keys_spread_over_the_stripes() {
        let used: HashSet<usize> = (0..STRIPES)
            .map(|i| {
                let key = CacheKey::new(format!("/spread/{i}"), "");
                stripe_index(&sharding::object_name(key.combined_bin(), ""))
            })
            .collect();
        assert!(
            used.len() > STRIPES / 2,
            "{} of {STRIPES} stripes used",
            used.len()
        );
    }

    /// A change on a key holds the key's stripe until it is dropped, and a
    /// keyless change holds no stripe: meanwhile, any other change on that
    /// key, or on a key sharing its stripe, waits.
    #[test]
    fn a_change_holds_its_key_until_dropped() {
        let locks = ChangeLocks::default();
        let name = name_starting("abc");
        let stripe = &locks.stripes[stripe_index(&name)];

        let change = locks.begin(&name).expect("a writable backend");
        assert!(
            matches!(stripe.try_lock(), Err(TryLockError::WouldBlock)),
            "held by the change"
        );
        drop(change);
        assert!(stripe.try_lock().is_ok(), "released with the change");

        let keyless = locks.begin_keyless().expect("a writable backend");
        assert!(
            locks.stripes.iter().all(|stripe| stripe.try_lock().is_ok()),
            "a keyless change holds no stripe"
        );
        drop(keyless);
    }

    /// A panic during a change poisons its key's stripe. The next change on
    /// that key still begins, and clears the poison, so the changes after it
    /// lock cleanly: one bug under a key's lock does not stop later changes
    /// on the keys that share its stripe.
    #[test]
    fn a_poisoned_stripe_still_locks_and_is_cleared() {
        let locks = ChangeLocks::default();
        let name = name_starting("abc");
        let stripe = &locks.stripes[stripe_index(&name)];
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _change = locks.begin(&name);
            panic!("a bug during a change");
        }));
        assert!(panicked.is_err(), "the closure panicked");
        assert!(stripe.is_poisoned());
        drop(locks.begin(&name).expect("a writable backend"));
        assert!(
            !stripe.is_poisoned(),
            "the next change on the key clears the poison"
        );
        assert!(
            stripe.lock().is_ok(),
            "so the changes after it lock cleanly"
        );
    }

    /// A panic under the gate's write lock, which today's code cannot cause,
    /// poisons it. The gate stays usable, and the first entry after the
    /// panic clears the poison, whether it begins a change or sets the
    /// backend read-only.
    #[test]
    fn a_poisoned_gate_stays_usable_and_is_cleared() {
        let locks = ChangeLocks::default();
        let poison = |locks: &ChangeLocks| {
            let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _writing = locks.read_only.write();
                panic!("a panic under the gate's write lock");
            }));
            assert!(panicked.is_err(), "the closure panicked");
            assert!(locks.read_only.is_poisoned());
        };

        poison(&locks);
        assert!(locks.begin_keyless().is_some(), "a change still begins");
        assert!(
            !locks.read_only.is_poisoned(),
            "beginning the change cleared the poison"
        );

        poison(&locks);
        locks.set_read_only();
        assert!(
            !locks.read_only.is_poisoned(),
            "set_read_only cleared the poison"
        );
        assert!(locks.is_read_only());
    }

    /// `set_read_only` returns only once the changes in progress have ended.
    /// The barrier makes sure the other thread is about to call it when the
    /// sleep starts. Were that thread kept off the processor for the whole
    /// sleep, the test would pass without testing anything, but it can
    /// never fail falsely.
    #[test]
    fn set_read_only_waits_for_changes_in_progress() {
        let locks = Arc::new(ChangeLocks::default());
        let change = locks
            .begin(&name_starting("abc"))
            .expect("a writable backend");
        let barrier = Arc::new(Barrier::new(2));
        let setter = {
            let (locks, barrier) = (Arc::clone(&locks), Arc::clone(&barrier));
            std::thread::spawn(move || {
                barrier.wait();
                locks.set_read_only();
            })
        };
        barrier.wait();
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !setter.is_finished(),
            "set_read_only returned while a change was in progress"
        );
        drop(change);
        setter.join().expect("the setting thread completes");
        assert!(locks.is_read_only());
    }

    /// Once the backend is read-only, no change can begin, keyed or not, and
    /// setting it read-only again returns at once. The one thread begins and
    /// ends changes again and again, so in a debug build this also shows
    /// that a change's end is recorded.
    #[test]
    fn no_change_begins_once_read_only() {
        let locks = ChangeLocks::default();
        let name = name_starting("abc");
        assert!(!locks.is_read_only());
        assert!(locks.begin(&name).is_some(), "writable");
        assert!(locks.begin_keyless().is_some(), "writable, keyless");
        locks.set_read_only();
        assert!(locks.is_read_only());
        assert!(locks.begin(&name).is_none(), "read-only");
        assert!(locks.begin_keyless().is_none(), "read-only, keyless");
        locks.set_read_only();
        assert!(locks.begin(&name).is_none(), "still read-only");
    }

    /// In a debug build, a thread that holds a change and begins another,
    /// on any key, fails an assertion at once, instead of risking a deadlock
    /// behind a waiting `set_read_only`.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "entered the read-only gate while this thread holds a change")]
    fn a_second_change_fails_in_a_debug_build() {
        let locks = ChangeLocks::default();
        let _first = locks
            .begin(&name_starting("abc"))
            .expect("a writable backend");
        let _second = locks.begin(&name_starting("def"));
    }

    /// In a debug build, asking whether the backend is read-only during a
    /// change enters the gate a second time, and fails an assertion at once.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "entered the read-only gate while this thread holds a change")]
    fn asking_whether_read_only_during_a_change_fails_in_a_debug_build() {
        let locks = ChangeLocks::default();
        let _change = locks.begin_keyless().expect("a writable backend");
        let _ = locks.is_read_only();
    }

    /// In a debug build, `set_read_only` during a change fails an assertion
    /// at once: it would wait for that change, which could then never end.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "set_read_only called while this thread holds a change")]
    fn set_read_only_during_a_change_fails_in_a_debug_build() {
        let locks = ChangeLocks::default();
        let _change = locks.begin_keyless().expect("a writable backend");
        locks.set_read_only();
    }

    /// In a debug build, taking a key's lock during a change fails an
    /// assertion at once: during a change on a key that shares the stripe,
    /// the thread would wait for itself. The change here is keyless, so that
    /// without the assertion the lock is taken and the test fails, rather
    /// than hangs.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "took a key's lock while this thread holds a change")]
    fn a_key_lock_during_a_change_fails_in_a_debug_build() {
        let locks = ChangeLocks::default();
        let _change = locks.begin_keyless().expect("a writable backend");
        let _key = locks.lock_key(&name_starting("abc"));
    }
}
