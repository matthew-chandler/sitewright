//! Time helpers. All stored timestamps are Unix milliseconds in UTC.

use std::time::{SystemTime, UNIX_EPOCH};

/// The current time as Unix milliseconds.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub const MS_PER_DAY: i64 = 86_400_000;
