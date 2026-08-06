// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

pub(crate) mod button;
mod fade;
pub(crate) mod notification;
mod timeline;
mod track_selector;

pub(super) use fade::fade;
use time::{OffsetDateTime, macros::format_description};
pub(crate) use timeline::timeline;

pub(crate) fn format_time(milliseconds: i64) -> String {
    let total_seconds = milliseconds.max(0) / 1_000;
    let seconds = total_seconds % 60;
    let minutes = (total_seconds / 60) % 60;
    let hours = total_seconds / 3_600;

    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

pub(super) fn format_ends_at(remaining_ms: i64) -> String {
    let end = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc())
        + time::Duration::milliseconds(remaining_ms.max(0));
    let time = end
        .format(format_description!("[hour]:[minute]"))
        .unwrap_or_else(|_| "--:--".to_owned());

    format!("Ends at {time}")
}
