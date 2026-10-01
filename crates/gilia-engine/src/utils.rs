// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{
    ops::{Deref, DerefMut},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering},
    },
};

use ffmpeg_next::Packet;
use tracing::warn;

use crate::{
    channel::wait::{WaitSource, Waiter, WaiterKind, WaiterList, WakeMode},
    session::get_session_start,
};

#[derive(Debug)]
pub struct Clock {
    pub active: AtomicBool,
    written_pts: AtomicI64,
    pub end_time: AtomicI64,
}

#[derive(Debug, Clone)]
pub struct MemoryBudget {
    inner: Arc<BudgetState>,
}

pub struct TrackedPacket {
    pub packet: Packet,
    pub budget: MemoryBudget,
}

#[derive(Debug)]
struct BudgetState {
    current_bytes: AtomicUsize,
    max_bytes: usize,
    available: WaiterList,
}

impl Clock {
    pub fn update(&self, pts: i64, end_time: i64) {
        self.written_pts.store(pts, Ordering::Relaxed);
        self.end_time.store(end_time, Ordering::Relaxed);
    }

    pub fn get_ms(&self) -> i64 {
        let pts = self.written_pts.load(Ordering::Relaxed);
        let end_time = self.end_time.load(Ordering::Relaxed);
        let now_time = get_session_start().elapsed().as_nanos() as i64;
        (pts - 0.max(end_time - now_time)) / 1_000_000
    }
}

impl Default for Clock {
    fn default() -> Self {
        let time = get_session_start().elapsed().as_nanos() as i64;
        Self {
            active: AtomicBool::new(false),
            written_pts: AtomicI64::default(),
            end_time: AtomicI64::new(time),
        }
    }
}

impl MemoryBudget {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            inner: Arc::new(BudgetState {
                current_bytes: AtomicUsize::new(0),
                max_bytes,
                available: WaiterList::new(),
            }),
        }
    }

    pub fn add(&self, bytes: usize) {
        self.inner.current_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn sub(&self, bytes: usize) {
        let prev = self.inner.current_bytes.fetch_sub(bytes, Ordering::Relaxed);
        let current = prev.saturating_sub(bytes);

        if prev >= self.inner.max_bytes
            && current < self.inner.max_bytes
            && let Err(e) = self.inner.available.wake(WakeMode::All)
        {
            warn!("Failed to wake memory budget waiters: {e}");
        }
    }

    pub fn over_limit(&self) -> bool {
        self.inner.current_bytes.load(Ordering::Relaxed) >= self.inner.max_bytes
    }

    pub fn current_bytes(&self) -> usize {
        self.inner.current_bytes.load(Ordering::Relaxed)
    }
}

impl WaitSource for MemoryBudget {
    fn is_ready(&self) -> bool {
        !self.over_limit()
    }

    fn register(&self, waiter: &Arc<dyn Waiter>, kind: WaiterKind) {
        self.inner.available.register(waiter, kind);
    }

    fn unregister(&self, waiter: &Arc<dyn Waiter>) {
        self.inner.available.unregister(waiter);
    }
}

impl TrackedPacket {
    pub fn new(packet: Packet, budget: MemoryBudget) -> Self {
        budget.add(packet.size());
        Self { packet, budget }
    }
}

impl DerefMut for TrackedPacket {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.packet
    }
}

impl Deref for TrackedPacket {
    type Target = Packet;

    fn deref(&self) -> &Self::Target {
        &self.packet
    }
}

impl Drop for TrackedPacket {
    fn drop(&mut self) {
        self.budget.sub(self.packet.size());
    }
}
