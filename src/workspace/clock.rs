//! The workspace's notion of now: the system clock, or a pinned one a test or replay supplies.

use crate::wallclock::{self, Clock as WallClock};

#[derive(Clone)]
pub struct Clock {
    now: WallClock,
    pinned: bool,
}

impl Clock {
    pub fn system() -> Self {
        Self {
            now: wallclock::system_clock(),
            pinned: false,
        }
    }

    /// A clock the caller drives. Git commits made through it carry its instant as author and committer date.
    pub fn pinned(now: WallClock) -> Self {
        Self { now, pinned: true }
    }

    pub fn now_ms(&self) -> u64 {
        (self.now)().max(0) as u64
    }

    pub fn iso(&self) -> String {
        wallclock::iso8601(self.now_ms() as i64)
    }

    pub fn is_pinned(&self) -> bool {
        self.pinned
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::system()
    }
}
