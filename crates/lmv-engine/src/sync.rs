use std::sync::atomic::{AtomicI64, Ordering};

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
    pub fn update(&self, pts: i64, end_time: i64) {
        self.written_pts.store(pts, Ordering::Relaxed);
        self.end_time.store(end_time, Ordering::Relaxed);
    }
    pub fn get_ms(&self) -> i64 {
        let pts = self.written_pts.load(Ordering::Relaxed);
        let end_time = self.end_time.load(Ordering::Relaxed);
        let now_time = get_engine_start().elapsed().as_nanos() as i64;
        (pts - 0.max(end_time - now_time)) / 1_000_000
    }
}
