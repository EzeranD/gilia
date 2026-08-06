// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::time::{Duration, Instant};

use gilia_engine::{ChapterMeta, Session};

const SEEK_INTERVAL: Duration = Duration::from_millis(100);
const CHAPTER_RESTART_MS: i64 = 5_000;

#[derive(Debug, Default)]
pub struct Seek {
    duration_ms: Option<i64>,
    target_ms: Option<i64>,
    active_ms: Option<i64>,
    last_seek: Option<Instant>,
    deadline: Option<Instant>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum SeekMode {
    #[default]
    Throttled,
    Immediate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChapterSeek {
    Index(usize),
    Previous,
    Current,
    Next,
}

impl Seek {
    pub fn new(duration_ms: i64) -> Self {
        Self {
            duration_ms: (duration_ms > 0).then_some(duration_ms),
            ..Self::default()
        }
    }

    pub fn target_ms(&self) -> Option<i64> {
        self.target_ms
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub fn is_ready(&self) -> bool {
        self.deadline
            .is_none_or(|deadline| Instant::now() >= deadline)
    }

    fn clamp_position(&self, position_ms: i64) -> i64 {
        position_ms.clamp(0, self.duration_ms.unwrap_or(i64::MAX))
    }

    pub fn seek_to(
        &mut self,
        session: &mut Session,
        position_ms: i64,
        mode: SeekMode,
    ) -> Option<Instant> {
        self.target_ms = Some(self.clamp_position(position_ms));

        self.deadline = match mode {
            SeekMode::Throttled => self.last_seek.map(|last| last + SEEK_INTERVAL),
            SeekMode::Immediate => None,
        };

        if self.is_ready() {
            self.dispatch(session);
        }

        self.deadline
    }

    pub fn seek_rel(
        &mut self,
        session: &mut Session,
        offset_ms: i64,
        mode: SeekMode,
    ) -> Option<Instant> {
        let position_ms = self
            .target_ms
            .unwrap_or_else(|| session.position_ms())
            .saturating_add(offset_ms);

        self.seek_to(session, position_ms, mode)
    }

    pub fn seek_chapter(
        &mut self,
        session: &mut Session,
        chapters: &[ChapterMeta],
        chapter: ChapterSeek,
        mode: SeekMode,
    ) -> Option<Instant> {
        let position_ms = self.target_ms.unwrap_or_else(|| session.position_ms());
        let next_chapter = chapters.partition_point(|chapter| chapter.start_ms <= position_ms);

        let index = match chapter {
            ChapterSeek::Index(index) => Some(index),
            ChapterSeek::Previous => next_chapter.checked_sub(1).and_then(|index| {
                if position_ms.saturating_sub(chapters[index].start_ms) > CHAPTER_RESTART_MS {
                    Some(index)
                } else {
                    index.checked_sub(1)
                }
            }),
            ChapterSeek::Current => next_chapter.checked_sub(1),
            ChapterSeek::Next => Some(next_chapter),
        };

        if chapter == ChapterSeek::Previous && index.is_none() {
            return self.seek_to(session, 0, mode);
        }

        if chapter == ChapterSeek::Next && !chapters.is_empty() && index == Some(chapters.len()) {
            return match self.duration_ms {
                Some(duration_ms) => self.seek_to(session, duration_ms, mode),
                None => self.deadline,
            };
        }

        let Some(chapter) = index.and_then(|index| chapters.get(index)) else {
            return self.deadline;
        };

        self.seek_to(session, chapter.start_ms, mode)
    }

    pub fn completed(&mut self, session: &mut Session, position_ms: i64) {
        if let Some(active_ms) = self.active_ms {
            if active_ms != position_ms {
                return;
            }

            self.active_ms = None;

            if self.target_ms == Some(position_ms) {
                self.target_ms = None;
                self.deadline = None;
                return;
            }
        }

        if self.is_ready() {
            self.dispatch(session);
        }
    }

    pub(super) fn dispatch(&mut self, session: &mut Session) {
        if self.active_ms.is_some() {
            return;
        }

        let Some(target_ms) = self.target_ms else {
            return;
        };

        let now = Instant::now();

        if session.seek_to(target_ms) {
            self.active_ms = Some(target_ms);
            self.last_seek = Some(now);
            self.deadline = None;
        } else {
            self.deadline = Some(now + SEEK_INTERVAL);
        }
    }
}
