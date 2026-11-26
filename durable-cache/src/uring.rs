// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! Store writes on an `io_uring` ring (FDS-STORAGE.IOURING-01), on Linux:
//! the body chunks that end past a store's first 64 KiB.
//!
//! One ring per backend, set up when the cache is constructed. A submission
//! takes a lock around the ring's submission queue, and its operation waits
//! in a slot until the kernel completes it. A task on the Tokio runtime that
//! makes the first submission collects the completions: the ring signals an
//! eventfd, which the task watches through that runtime's reactor, and the
//! task wakes each completed operation's future. When that runtime shuts
//! down, the task's end wakes every operation still waiting, and the next one
//! polled starts the task again on its own runtime. The buffer and the file
//! of an operation belong to its slot until the kernel has completed it, so a
//! future dropped before then leaves them to the task, which frees them;
//! dropping the ring waits for the kernel to finish what is still in
//! flight.

use std::any::Any;
use std::fs::File;
use std::future::Future;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};

use bytes::Bytes;
use io_uring::{opcode, squeue, types, IoUring, Probe};
use pingora_error::{ErrorType, OrErr};
use tokio::io::unix::AsyncFd;

use crate::hooks::Hooks;
use crate::FsCacheConfig;

/// The entries of the submission queue. A submission that finds the queue
/// full hands it to the kernel first, so this bounds no more than how many
/// writes one system call submits.
const ENTRIES: u32 = 256;

/// What an operation in flight keeps alive for the kernel: its buffer and
/// its file.
type Kept = Box<dyn Any + Send>;

enum State {
    /// Submitted; the future waits, with its waker once polled.
    Waiting(Option<Waker>),
    /// Completed with the kernel's result, not collected yet.
    Done(i32),
    /// Its future was dropped first: the slot is freed with the completion.
    Abandoned,
}

struct Slot {
    state: State,
    /// What the operation's entry refers to: never read, only kept alive
    /// until the slot is freed, once the kernel has completed it.
    _kept: Kept,
}

/// The operations in flight, indexed by the user data of their entries.
#[derive(Default)]
struct Slots {
    slots: Vec<Option<Slot>>,
    free: Vec<usize>,
}

impl Slots {
    fn insert(&mut self, slot: Slot) -> usize {
        match self.free.pop() {
            Some(i) => {
                self.slots[i] = Some(slot);
                i
            }
            None => {
                self.slots.push(Some(slot));
                self.slots.len() - 1
            }
        }
    }

    fn get_mut(&mut self, i: usize) -> Option<&mut Slot> {
        self.slots.get_mut(i).and_then(Option::as_mut)
    }

    fn remove(&mut self, i: usize) -> Option<Slot> {
        let slot = self.slots.get_mut(i)?.take();
        if slot.is_some() {
            self.free.push(i);
        }
        slot
    }

    /// The kernel has completed the operation in slot `key` with `result`.
    /// The future that waits for it, if one does, is to be woken. If its
    /// future was dropped, the slot is freed now: the kernel is done with
    /// what it kept.
    fn complete(&mut self, key: usize, result: i32) -> Completed {
        let Some(slot) = self.get_mut(key) else {
            return Completed::NotInFlight;
        };
        match std::mem::replace(&mut slot.state, State::Done(result)) {
            State::Waiting(waker) => Completed::Wake(waker),
            State::Abandoned => {
                self.remove(key);
                Completed::Wake(None)
            }
            State::Done(_) => Completed::Twice,
        }
    }

    /// The future of the operation in slot `key` is polled: the kernel's
    /// result once it has completed the operation, the slot freed, as the
    /// kernel is done with what it kept. Until then, `waker` waits in the
    /// slot.
    fn poll(&mut self, key: usize, waker: &Waker) -> Option<i32> {
        let slot = self
            .get_mut(key)
            .expect("an operation's slot lives until its future collects it");
        let result = match &mut slot.state {
            State::Done(result) => Some(*result),
            State::Waiting(waiting) => {
                *waiting = Some(waker.clone());
                None
            }
            State::Abandoned => unreachable!("only a dropped future abandons its operation"),
        };
        if result.is_some() {
            self.remove(key);
        }
        result
    }

    /// The future of the operation in slot `key` is dropped before it took
    /// the result. The slot is freed if the kernel has completed the
    /// operation, and otherwise left for its completion to free.
    fn abandon(&mut self, key: usize) {
        let Some(slot) = self.get_mut(key) else {
            return;
        };
        if matches!(slot.state, State::Done(_)) {
            self.remove(key);
        } else {
            slot.state = State::Abandoned;
        }
    }

    /// Whether an operation the kernel has not completed is in a slot.
    fn any_in_flight(&self) -> bool {
        self.slots
            .iter()
            .flatten()
            .any(|slot| !matches!(slot.state, State::Done(_)))
    }
}

/// What a completion the kernel posted made of its operation.
enum Completed {
    /// Completed: the waker of the future that waits for it, if one does.
    Wake(Option<Waker>),
    /// No operation is in the slot the completion names.
    NotInFlight,
    /// The operation had been completed already.
    Twice,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the ring's users and its completion task share.
struct Shared {
    ring: IoUring,
    /// Held while a submission queue exists, as `submission_shared` asks.
    sq: Mutex<()>,
    /// Held while the completion queue is read, as `completion_shared` asks.
    cq: Mutex<()>,
    slots: Mutex<Slots>,
    /// Signalled by the kernel as it posts completions, and by
    /// [`Ring::drop`] to stop the completion task. Non-blocking, as the
    /// reactor that watches it asks.
    eventfd: OwnedFd,
    /// Whether a completion task runs.
    collecting: AtomicBool,
    /// Set as the ring is dropped: the completion task ends.
    stop: AtomicBool,
}

/// The eventfd as the reactor watches it. The `Shared` it holds keeps the
/// descriptor open for as long as the reactor has it.
struct EventFd(Arc<Shared>);

impl AsRawFd for EventFd {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.0.eventfd.as_raw_fd()
    }
}

/// Held by the completion task while it runs. As the task ends --- its
/// runtime shut down, or the ring dropped --- it wakes every operation still
/// waiting, so that the next one polled starts a task on its own runtime.
struct Collecting(Arc<Shared>);

impl Drop for Collecting {
    fn drop(&mut self) {
        self.0.collecting.store(false, Ordering::Release);
        let waiting: Vec<Waker> = lock(&self.0.slots)
            .slots
            .iter_mut()
            .flatten()
            .filter_map(|slot| match &mut slot.state {
                State::Waiting(waker) => waker.take(),
                _ => None,
            })
            .collect();
        for waker in waiting {
            waker.wake();
        }
    }
}

impl Shared {
    /// Collect every completion the kernel has posted, and wake the futures
    /// that wait for them.
    fn collect(&self) {
        let mut wake = Vec::new();
        {
            let _cq = lock(&self.cq);
            // SAFETY: `cq` is held, so no other completion queue exists.
            let cq = unsafe { self.ring.completion_shared() };
            let mut slots = lock(&self.slots);
            for cqe in cq {
                // The user data is the slot's index, as `Ring::submit` sets it.
                let key = cqe.user_data() as usize;
                match slots.complete(key, cqe.result()) {
                    Completed::Wake(waker) => wake.extend(waker),
                    Completed::NotInFlight => {
                        log::error!("io_uring completed operation {key}, which is not in flight")
                    }
                    Completed::Twice => log::error!("io_uring completed operation {key} twice"),
                }
            }
        }
        for waker in wake {
            waker.wake();
        }
    }

    /// Start the completion task on the runtime that polls the caller,
    /// unless one runs.
    fn ensure_collecting(self: &Arc<Self>) -> io::Result<()> {
        if self.collecting.load(Ordering::Acquire) || self.collecting.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let started = tokio::runtime::Handle::try_current()
            .map_err(io::Error::other)
            .and_then(|runtime| {
                // SAFETY: `EventFd` holds the `Shared` that owns the
                // descriptor, which therefore stays open, and always names
                // it, for as long as the `AsyncFd` lives.
                let watched = unsafe { AsyncFd::register(EventFd(Arc::clone(self))) }
                    .map_err(io::Error::from)?;
                runtime.spawn(collect_completions(watched));
                Ok(())
            });
        if started.is_err() {
            self.collecting.store(false, Ordering::Release);
        }
        started
    }
}

/// The completion task: collect, then wait for the eventfd, until the ring is
/// dropped or the task's runtime shuts down.
async fn collect_completions(watched: AsyncFd<EventFd>) {
    let shared = Arc::clone(&watched.get_ref().0);
    let _collecting = Collecting(Arc::clone(&shared));
    let mut count = [0u8; 8];
    loop {
        shared.collect();
        if shared.stop.load(Ordering::Acquire) {
            return;
        }
        let mut ready = match watched.readable().await {
            Ok(ready) => ready,
            Err(e) => {
                log::error!("the io_uring completion task lost its eventfd: {e}");
                return;
            }
        };
        match rustix::io::read(&shared.eventfd, &mut count) {
            Ok(_) => {}
            Err(e) if e == rustix::io::Errno::AGAIN || e == rustix::io::Errno::INTR => {}
            Err(e) => {
                log::error!("the io_uring completion task cannot read its eventfd: {e}");
                return;
            }
        }
        ready.clear_ready();
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // Only abandoned operations can be left: every future, and the
        // completion task, held a reference. The kernel may still be using
        // their buffers, so they are freed only once it has completed them.
        while lock(&self.slots).any_in_flight() {
            if let Err(e) = self.ring.submit_and_wait(1) {
                if e.kind() != io::ErrorKind::Interrupted {
                    log::error!("io_uring operations still in flight at the backend's drop: {e}");
                    // The buffers must outlive the kernel's use of them.
                    std::mem::forget(std::mem::take(&mut *lock(&self.slots)));
                    return;
                }
            }
            self.collect();
        }
    }
}

// IMP-STORAGE.IOURING-01: satisfy FDS-STORAGE.IOURING-01
/// The backend's `io_uring` ring, for its store writes. Its completion task
/// starts with the first submission, never at construction, which may
/// precede the fork of a server that daemonises.
pub(crate) struct Ring {
    shared: Arc<Shared>,
}

impl Ring {
    /// The ring `config` asks for, or `None` with it switched off. Fails
    /// with an `InternalError` that names the option if the kernel refuses
    /// to set it up.
    pub(crate) fn new(
        config: &FsCacheConfig,
        hooks: &Hooks,
    ) -> pingora_error::Result<Option<Self>> {
        if !config.io_uring() {
            return Ok(None);
        }
        #[allow(unused_mut)]
        let mut set_up = Self::set_up();
        test_hook! {
            if hooks.fault(crate::hooks::FaultPoint::RingSetup).is_err() {
                set_up = Err(io::Error::other("the injected RingSetup fault"));
            }
        }
        #[cfg(not(test))]
        let _ = hooks;
        set_up.map(Some).or_err(
            ErrorType::InternalError,
            "setting up the io_uring ring that stores write through, which \
             FsCacheConfig::with_io_uring asks for: allow io_uring here, or switch \
             the option off",
        )
    }

    /// A ring that signals an eventfd as it completes operations, once the
    /// kernel has been found to support the writes this backend submits.
    fn set_up() -> io::Result<Self> {
        let ring = IoUring::new(ENTRIES)?;
        let mut probe = Probe::new();
        ring.submitter().register_probe(&mut probe)?;
        if !probe.is_supported(opcode::Write::CODE) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "this kernel's io_uring lacks IORING_OP_WRITE",
            ));
        }
        let eventfd = rustix::event::eventfd(
            0,
            rustix::event::EventfdFlags::CLOEXEC | rustix::event::EventfdFlags::NONBLOCK,
        )?;
        ring.submitter().register_eventfd(eventfd.as_raw_fd())?;
        Ok(Self {
            shared: Arc::new(Shared {
                ring,
                sq: Mutex::new(()),
                cq: Mutex::new(()),
                slots: Mutex::new(Slots::default()),
                eventfd,
                collecting: AtomicBool::new(false),
                stop: AtomicBool::new(false),
            }),
        })
    }

    /// Submit `entry`, whose operation completes as the returned future.
    ///
    /// # Safety
    ///
    /// The memory and the file descriptors `entry` refers to must be owned
    /// by `kept`, and stay valid while `kept` lives: a `Bytes`'s heap
    /// buffer, an `Arc<File>`. The slot keeps `kept` until the kernel has
    /// completed the operation.
    unsafe fn submit(&self, entry: squeue::Entry, kept: Kept) -> io::Result<Op> {
        self.shared.ensure_collecting()?;
        let shared = &self.shared;
        let key = lock(&shared.slots).insert(Slot {
            state: State::Waiting(None),
            _kept: kept,
        });
        let entry = entry.user_data(key as u64);
        let submitted = {
            let _sq = lock(&shared.sq);
            // SAFETY: `sq` is held, so no other submission queue exists.
            let mut sq = unsafe { shared.ring.submission_shared() };
            loop {
                // SAFETY: the caller's: what `entry` refers to is in `kept`,
                // which the slot holds until the kernel completes it.
                if unsafe { sq.push(&entry) }.is_ok() {
                    break;
                }
                // Full: hand what the queue holds to the kernel first.
                sq.sync();
                if let Err(e) = shared.ring.submit() {
                    // The entry never reached the queue.
                    drop(sq);
                    lock(&shared.slots).remove(key);
                    return Err(e);
                }
                sq.sync();
            }
            drop(sq);
            shared.ring.submit()
        };
        let op = Op {
            shared: Arc::clone(shared),
            key: Some(key),
        };
        // A failed submission may still have handed the entry to the kernel:
        // dropping the future leaves its slot to the completion, if any.
        submitted.map(|_| op)
    }

    /// Write all of `data` at `offset` of `file`.
    pub(crate) async fn write_all_at(
        &self,
        file: Arc<File>,
        data: Bytes,
        offset: u64,
    ) -> io::Result<()> {
        let mut done = 0;
        while done < data.len() {
            let part = data.slice(done..);
            let len = u32::try_from(part.len()).unwrap_or(u32::MAX);
            let entry = opcode::Write::new(types::Fd(file.as_raw_fd()), part.as_ptr(), len)
                .offset(offset + done as u64)
                .build();
            // SAFETY: the entry refers to `part`'s bytes and to `file`'s
            // descriptor, and the operation keeps both.
            let op = unsafe { self.submit(entry, Box::new((Arc::clone(&file), part)))? };
            let written = op.await;
            let written = usize::try_from(written)
                .map_err(|_| io::Error::from_raw_os_error(written.saturating_neg()))?;
            if written == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            done += written;
        }
        Ok(())
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        // Wake the completion task, if one runs: it sees `stop` once it has
        // collected, and ends.
        if let Err(e) = rustix::io::write(&self.shared.eventfd, &1u64.to_ne_bytes()) {
            log::error!("cannot stop the io_uring completion task: {e}");
        }
    }
}

/// An operation in flight: the kernel's result, once it has completed it.
pub(crate) struct Op {
    shared: Arc<Shared>,
    key: Option<usize>,
}

impl Future for Op {
    type Output = i32;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<i32> {
        let key = self
            .key
            .expect("an operation is not polled after it has completed");
        let completed = lock(&self.shared.slots).poll(key, cx.waker());
        if let Some(result) = completed {
            self.key = None;
            return Poll::Ready(result);
        }
        // Only the completion task calls the waker: if the runtime that ran
        // it has shut down, start it again on this one.
        match self.shared.ensure_collecting() {
            Ok(()) => Poll::Pending,
            // Not on a Tokio runtime: nothing would ever wake this future.
            // The operation is abandoned as the future is dropped.
            Err(e) => Poll::Ready(-e.raw_os_error().unwrap_or(libc::EIO)),
        }
    }
}

impl Drop for Op {
    fn drop(&mut self) {
        if let Some(key) = self.key {
            lock(&self.shared.slots).abandon(key);
        }
    }
}

/// The table of operations in flight, for every order in which two
/// operations are submitted, completed by the kernel, polled and dropped
/// (TC-STORAGE.UNSAFE-01). An ordinary test that goes through every such
/// order, as Kani runs out of memory on the table.
#[cfg(test)]
mod exhaustive_checks {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::Waker;

    use super::*;

    // IMP-TC.STORAGE.UNSAFE-02: satisfy TC-STORAGE.UNSAFE-01

    /// The operations each order involves.
    const OPS: usize = 2;

    /// How many times a future is polled at most.
    const POLLS: usize = 2;

    /// What happens to an operation.
    #[derive(Clone, Copy, Debug)]
    enum Event {
        Submit,
        /// The kernel completes it: once, as the kernel does.
        Complete,
        Poll,
        /// Its future is dropped before it took the result.
        Abandon,
    }

    /// One operation, as the check follows it.
    #[derive(Clone, Copy, Default)]
    struct Operation {
        submitted: bool,
        completed: bool,
        polls: usize,
        future_gone: bool,
    }

    impl Operation {
        fn enabled(&self, event: Event) -> bool {
            match event {
                Event::Submit => !self.submitted,
                Event::Complete => self.submitted && !self.completed,
                Event::Poll => self.submitted && !self.future_gone && self.polls < POLLS,
                Event::Abandon => self.submitted && !self.future_gone,
            }
        }
    }

    /// What a slot keeps for the kernel, in place of a buffer and a file:
    /// it records when it is dropped.
    struct Kept(Arc<AtomicBool>);

    impl Drop for Kept {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// The kernel's result for operation `i`, one of its own.
    fn result_of(i: usize) -> i32 {
        100 + i as i32
    }

    /// Replay `order` on a new table, checking after each event that a
    /// slot drops what it keeps only once the kernel has completed its
    /// operation, and then as soon as its future is gone too, and that a
    /// future takes its own operation's result, once.
    fn replay(order: &[(usize, Event)]) {
        let mut slots = Slots::default();
        let freed: [Arc<AtomicBool>; OPS] =
            std::array::from_fn(|_| Arc::new(AtomicBool::new(false)));
        let mut keys = [None; OPS];
        let mut ops = [Operation::default(); OPS];
        for &(i, event) in order {
            let op = &mut ops[i];
            match event {
                Event::Submit => {
                    keys[i] = Some(slots.insert(Slot {
                        state: State::Waiting(None),
                        _kept: Box::new(Kept(Arc::clone(&freed[i]))),
                    }));
                    op.submitted = true;
                }
                Event::Complete => {
                    let key = keys[i].expect("a completed operation was submitted");
                    let completed = slots.complete(key, result_of(i));
                    assert!(matches!(completed, Completed::Wake(_)), "{order:?}");
                    op.completed = true;
                }
                Event::Poll => {
                    let key = keys[i].expect("a polled operation was submitted");
                    match slots.poll(key, Waker::noop()) {
                        Some(result) => {
                            assert!(op.completed, "a result before the kernel's: {order:?}");
                            assert_eq!(result, result_of(i), "another's result: {order:?}");
                            op.future_gone = true;
                        }
                        None => assert!(!op.completed, "a result not taken: {order:?}"),
                    }
                    op.polls += 1;
                }
                Event::Abandon => {
                    slots.abandon(keys[i].expect("an abandoned operation was submitted"));
                    op.future_gone = true;
                }
            }
            for (i, op) in ops.iter().enumerate() {
                let freed = freed[i].load(Ordering::SeqCst);
                if op.submitted && !op.completed {
                    assert!(!freed, "a slot dropped what the kernel may use: {order:?}");
                }
                if op.completed && op.future_gone {
                    assert!(freed, "a slot kept what nothing needs: {order:?}");
                }
            }
        }
    }

    /// Replay every order that extends `order`, and `order` itself.
    fn every_order(order: &mut Vec<(usize, Event)>, ops: [Operation; OPS], count: &mut usize) {
        replay(order);
        *count += 1;
        for i in 0..OPS {
            for event in [Event::Submit, Event::Complete, Event::Poll, Event::Abandon] {
                if !ops[i].enabled(event) {
                    continue;
                }
                let mut next = ops;
                match event {
                    Event::Submit => next[i].submitted = true,
                    Event::Complete => next[i].completed = true,
                    Event::Poll => {
                        next[i].polls += 1;
                        next[i].future_gone |= ops[i].completed;
                    }
                    Event::Abandon => next[i].future_gone = true,
                }
                order.push((i, event));
                every_order(order, next, count);
                order.pop();
            }
        }
    }

    /// For every order in which two operations are submitted, completed by
    /// the kernel, polled up to twice and dropped, a slot drops what it
    /// keeps only once the kernel has completed its operation, and then as
    /// soon as its future is gone too; and each future takes its own
    /// operation's result, once. A slot freed is reused by the next
    /// submission, so the orders cover reuse too.
    #[test]
    fn tc_storage_unsafe_01_every_order_keeps_buffers_until_completed() {
        let mut count = 0;
        every_order(&mut Vec::new(), [Operation::default(); OPS], &mut count);
        assert!(count > 1_000, "only {count} orders");
    }
}
