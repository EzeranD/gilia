// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{
    cell::Cell,
    error::Error,
    fmt,
    marker::PhantomData,
    ptr,
    rc::Rc,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering, fence},
    },
    thread,
    time::Instant,
};

use arc_swap::ArcSwapOption;
use crossbeam_utils::Backoff;
#[cfg(windows)]
use snafu::ResultExt;
use snafu::{Snafu, ensure};

#[cfg(windows)]
use super::event::EventWaiter;

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum WakeError {
    #[snafu(transparent)]
    Failed {
        source: Box<dyn Error + Send + Sync>,
    },
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum WaitError {
    #[snafu(display("no wait sources provided"))]
    NoSources,
    #[snafu(transparent)]
    Failed {
        source: Box<dyn Error + Send + Sync>,
    },
}

#[derive(Debug)]
pub(crate) struct WaiterList {
    empty: AtomicBool,
    entries: Mutex<Vec<WaiterEntry>>,
}

pub(crate) struct WaiterSlot {
    waiter: ArcSwapOption<WaiterEntry>,
    backing: Mutex<Option<Arc<WaiterEntry>>>,
}

pub(crate) trait WaitSource {
    fn is_ready(&self) -> bool;

    fn register(&self, waiter: &Arc<dyn Waiter>, kind: WaiterKind);

    fn unregister(&self, waiter: &Arc<dyn Waiter>);
}

pub(crate) trait Waiter: fmt::Debug + Send + Sync {
    fn wake(&self) -> Result<(), WakeError>;

    fn wait(&self, deadline: Option<Instant>) -> Result<WaitOutcome, WaitError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaiterKind {
    Operation,
    Observer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitOutcome {
    Retry,
    Deadline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WakeMode {
    OneOperation,
    All,
}

#[derive(Debug)]
pub(super) struct ThreadWaiter {
    state: AtomicUsize,
    thread: thread::Thread,
}

pub(super) struct ThreadWaiterGuard {
    pair: Option<ThreadWaiterPair>,
    _not_send: PhantomData<Rc<()>>,
}

type ThreadWaiterPair = (Arc<ThreadWaiter>, Arc<dyn Waiter>);

#[derive(Debug)]
struct WaiterEntry {
    waiter: Weak<dyn Waiter>,
    kind: WaiterKind,
}

struct WaitRegistrations<'a> {
    sources: &'a [&'a dyn WaitSource],
    waiter: &'a Arc<dyn Waiter>,
    registered: usize,
}

const NOTIFIED: usize = 1;
const SLEEPING: usize = 2;

thread_local! {
    static CACHED_THREAD_WAITER: Cell<Option<ThreadWaiterPair>> = const {
        Cell::new(None)
    };
}

pub(super) fn wait_until(
    sources: &[&dyn WaitSource],
    waiter: &Arc<dyn Waiter>,
    kind: WaiterKind,
    deadline: Option<Instant>,
) -> Result<WaitOutcome, WaitError> {
    ensure!(!sources.is_empty(), wait_error::NoSourcesSnafu);

    if sources.iter().any(|source| source.is_ready()) {
        return Ok(WaitOutcome::Retry);
    }

    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Ok(WaitOutcome::Deadline);
    }

    let _registrations = WaitRegistrations::new(sources, waiter, kind);

    if sources.iter().any(|source| source.is_ready()) {
        return Ok(WaitOutcome::Retry);
    }

    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Ok(WaitOutcome::Deadline);
    }

    waiter.wait(deadline)
}

pub(super) fn wait_ready(
    source: &dyn WaitSource,
    deadline: Instant,
) -> Result<WaitOutcome, WaitError> {
    #[cfg(windows)]
    let waiter = EventWaiter::acquire().boxed()?;

    #[cfg(not(windows))]
    let waiter = ThreadWaiter::acquire();

    wait_until(
        &[source],
        waiter.as_waiter(),
        WaiterKind::Operation,
        Some(deadline),
    )
}

impl ThreadWaiter {
    pub(super) fn acquire() -> ThreadWaiterGuard {
        let (concrete, waiter) = CACHED_THREAD_WAITER
            .try_with(Cell::take)
            .unwrap_or_default()
            .unwrap_or_else(Self::new_pair);

        concrete.reset();

        ThreadWaiterGuard {
            pair: Some((concrete, waiter)),
            _not_send: PhantomData,
        }
    }

    fn new() -> Self {
        Self {
            state: AtomicUsize::new(0),
            thread: thread::current(),
        }
    }

    fn new_pair() -> ThreadWaiterPair {
        let concrete = Arc::new(Self::new());
        let waiter: Arc<dyn Waiter> = concrete.clone();

        (concrete, waiter)
    }

    fn reset(&self) {
        self.state.store(0, Ordering::Relaxed);
    }
}

impl Waiter for ThreadWaiter {
    fn wake(&self) -> Result<(), WakeError> {
        if self.state.load(Ordering::SeqCst) & NOTIFIED != 0 {
            return Ok(());
        }

        let previous = self.state.fetch_or(NOTIFIED, Ordering::SeqCst);

        if previous & NOTIFIED != 0 {
            return Ok(());
        }

        if previous & SLEEPING != 0 {
            self.thread.unpark();
        }

        Ok(())
    }

    fn wait(&self, deadline: Option<Instant>) -> Result<WaitOutcome, WaitError> {
        let previous = self.state.fetch_and(!NOTIFIED, Ordering::SeqCst);

        if previous & NOTIFIED != 0 {
            return Ok(WaitOutcome::Retry);
        }

        self.state.fetch_or(SLEEPING, Ordering::SeqCst);

        loop {
            if self.state.load(Ordering::SeqCst) & NOTIFIED != 0 {
                self.state
                    .fetch_and(!(SLEEPING | NOTIFIED), Ordering::SeqCst);
                return Ok(WaitOutcome::Retry);
            }

            match deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());

                    if remaining.is_zero() {
                        let previous = self
                            .state
                            .fetch_and(!(SLEEPING | NOTIFIED), Ordering::SeqCst);

                        if previous & NOTIFIED != 0 {
                            return Ok(WaitOutcome::Retry);
                        }

                        return Ok(WaitOutcome::Deadline);
                    }

                    thread::park_timeout(remaining);
                }
                None => thread::park(),
            }
        }
    }
}

impl WaiterList {
    pub(crate) fn new() -> Self {
        Self {
            empty: AtomicBool::new(true),
            entries: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn register(&self, waiter: &Arc<dyn Waiter>, kind: WaiterKind) {
        let mut entries = self.entries.lock().unwrap();

        if let Some(entry) = entries
            .iter_mut()
            .find(|entry| ptr::addr_eq(entry.waiter.as_ptr(), Arc::as_ptr(waiter)))
        {
            entry.kind = kind;
        } else {
            entries.push(WaiterEntry {
                waiter: Arc::downgrade(waiter),
                kind,
            });

            self.empty.store(false, Ordering::SeqCst);
        }

        drop(entries);

        fence(Ordering::SeqCst);
    }

    pub(crate) fn unregister(&self, waiter: &Arc<dyn Waiter>) {
        let mut entries = self.entries.lock().unwrap();

        entries.retain(|entry| !ptr::addr_eq(entry.waiter.as_ptr(), Arc::as_ptr(waiter)));

        self.empty.store(entries.is_empty(), Ordering::SeqCst);
    }

    pub(crate) fn wake(&self, mode: WakeMode) -> Result<(), WakeError> {
        fence(Ordering::SeqCst);

        if self.empty.load(Ordering::SeqCst) {
            return Ok(());
        }

        let mut entries = self.entries.lock().unwrap();
        let mut op_selected = false;
        let mut result = Ok(());

        entries.retain(|entry| {
            if mode == WakeMode::OneOperation && entry.kind == WaiterKind::Operation && op_selected
            {
                return true;
            }

            let Some(waiter) = entry.waiter.upgrade() else {
                return false;
            };

            match waiter.wake() {
                Ok(()) => {
                    if entry.kind == WaiterKind::Operation {
                        op_selected = true;
                    }
                    false
                }
                Err(e) => {
                    result = Err(e);
                    true
                }
            }
        });

        self.empty.store(entries.is_empty(), Ordering::SeqCst);

        result
    }
}

impl WaiterSlot {
    pub(crate) fn new() -> Self {
        Self {
            waiter: ArcSwapOption::empty(),
            backing: Mutex::new(None),
        }
    }

    pub(crate) fn register(&self, waiter: &Arc<dyn Waiter>, kind: WaiterKind) {
        let mut backing = self.backing.lock().unwrap();

        if let Some(entry) = backing.as_ref() {
            assert!(
                ptr::addr_eq(entry.waiter.as_ptr(), Arc::as_ptr(waiter)),
                "slot already has a different waiter"
            );
        } else {
            let entry = Arc::new(WaiterEntry {
                waiter: Arc::downgrade(waiter),
                kind,
            });

            self.waiter.store(Some(Arc::clone(&entry)));
            *backing = Some(entry);
        }

        drop(backing);

        fence(Ordering::SeqCst);
    }

    pub(crate) fn unregister(&self, waiter: &Arc<dyn Waiter>) {
        let mut backing = self.backing.lock().unwrap();
        let Some(entry) = backing.as_ref() else {
            return;
        };

        assert!(
            ptr::addr_eq(entry.waiter.as_ptr(), Arc::as_ptr(waiter)),
            "unregistering a different slot waiter"
        );

        self.waiter.store(None);

        let backoff = Backoff::new();
        while Arc::strong_count(entry) != 1 {
            backoff.snooze();
        }

        backing.take();
    }

    pub(crate) fn wake(&self) -> Result<(), WakeError> {
        fence(Ordering::SeqCst);
        let entry = self.waiter.load();

        if let Some(entry) = entry.as_ref()
            && let Some(waiter) = entry.waiter.upgrade()
        {
            waiter.wake()?
        }

        Ok(())
    }
}

impl ThreadWaiterGuard {
    pub(super) fn as_waiter(&self) -> &Arc<dyn Waiter> {
        &self.pair.as_ref().unwrap().1
    }
}

impl Drop for ThreadWaiterGuard {
    fn drop(&mut self) {
        let Some(pair) = self.pair.take() else {
            return;
        };

        let _ = CACHED_THREAD_WAITER.try_with(|cache| {
            let cached = cache.take();
            cache.set(cached.or(Some(pair)));
        });
    }
}

impl<'a> WaitRegistrations<'a> {
    fn new(
        sources: &'a [&'a dyn WaitSource],
        waiter: &'a Arc<dyn Waiter>,
        kind: WaiterKind,
    ) -> Self {
        let mut registrations = Self {
            sources,
            waiter,
            registered: 0,
        };

        for source in sources {
            source.register(waiter, kind);
            registrations.registered += 1;
        }

        registrations
    }
}

impl Drop for WaitRegistrations<'_> {
    fn drop(&mut self) {
        for source in self.sources.iter().take(self.registered) {
            source.unregister(self.waiter);
        }
    }
}
