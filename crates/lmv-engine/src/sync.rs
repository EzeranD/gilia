use std::{
    ops::AddAssign,
    sync::atomic::{AtomicI64, Ordering},
};

use crate::engine::get_engine_start;

#[derive(Debug)]
pub struct Clock {
    written_pts: AtomicI64,
    pub end_time: AtomicI64,
}

impl Default for Clock {
    fn default() -> Self {
        let time = get_engine_start().elapsed().as_nanos() as i64;
        Self {
            written_pts: AtomicI64::default(),
            end_time: AtomicI64::new(time),
        }
    }
}

impl Clock {
    pub fn update(&self, pts: i64, now: i64) {
        self.written_pts.store(pts, Ordering::Relaxed);
        self.end_time.store(now, Ordering::Relaxed);
    }
    pub fn get_ms(&self) -> i64 {
        let pts = self.written_pts.load(Ordering::Relaxed);
        let end_time = self.end_time.load(Ordering::Relaxed);
        let now_time = get_engine_start().elapsed().as_nanos() as i64;
        (pts - 0.max(end_time - now_time)) / 1_000_000
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AudioFrames(pub i64);

#[derive(Debug, Clone, Copy)]
pub struct Nanoseconds(pub i64);

impl Nanoseconds {
    #[must_use]
    pub fn from_engine_start() -> Self {
        let ns = get_engine_start().elapsed().as_nanos() as i64;
        Self(ns)
    }

    pub fn from_frames(frames: AudioFrames, rate: u32) -> Self {
        let ns = (frames.0 * 1_000_000_000) / rate as i64;
        Self(ns)
    }
}

impl AddAssign for Nanoseconds {
    fn add_assign(&mut self, rhs: Self) {
        self.0 += rhs.0;
    }
}
