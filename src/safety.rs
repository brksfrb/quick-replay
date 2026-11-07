//! Decisions the capture pipeline and the crash supervisor share.
//! No Windows types, so the stress test runs on any platform.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// No progress from the capture thread for this long means it is wedged.
pub const WATCHDOG_LIMIT: Duration = Duration::from_secs(8);
/// How long a restart waits for the capture threads before leaving a stuck encoder to release itself.
pub const JOIN_LIMIT: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WakeSchedule {
    /// First wake event in a burst: arm the delayed restart.
    Arm,
    /// Another wake event before that restart runs: keep a single timer and push its deadline.
    Coalesce,
    /// Stop/start is already running.
    Defer,
}

pub fn wake_schedule(restart_in_progress: bool, pending: bool) -> WakeSchedule {
    if restart_in_progress {
        WakeSchedule::Defer
    } else if pending {
        WakeSchedule::Coalesce
    } else {
        WakeSchedule::Arm
    }
}

/// Milliseconds since the Unix epoch. The capture thread and the watchdog share this clock.
pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub fn watchdog_due(last_beat_ms: u64, now_ms: u64, limit: Duration) -> bool {
    now_ms.saturating_sub(last_beat_ms) > limit.as_millis() as u64
}

/// Codec parameters of the frames currently in the replay buffer.
#[derive(Clone, Copy)]
pub struct Encoding<'a> {
    pub width: u32,
    pub height: u32,
    pub sample_entry: &'a [u8],
}

/// True when new encoder output can be appended to frames already in the buffer.
pub fn same_encoding(old: Encoding<'_>, new: Encoding<'_>) -> bool {
    old.width == new.width && old.height == new.height && old.sample_entry == new.sample_entry
}

/// Backoff after a child that died immediately, so a startup crash doesn't spin.
pub fn restart_backoff(fast_failures: u32) -> Duration {
    Duration::from_secs(1u64.checked_shl(fast_failures.min(6)).unwrap_or(64).min(60))
}

/// The supervisor starts the app again only after an unexpected exit.
pub fn should_restart_process(clean_exit: bool, enabled: bool) -> bool {
    enabled && !clean_exit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    struct Fmt {
        w: u32,
        h: u32,
        tag: u8,
    }

    /// One frame of evidence. `t` is the pipeline timestamp.
    struct Session {
        frames: Vec<i64>,
        recovered: Vec<Vec<i64>>,
        format: Fmt,
        heartbeat: u64,
        now: u64,
        produced: usize,
    }

    impl Session {
        fn new() -> Session {
            Session {
                frames: Vec::new(),
                recovered: Vec::new(),
                format: Fmt { w: 1920, h: 1080, tag: 1 },
                heartbeat: 0,
                now: 0,
                produced: 0,
            }
        }

        fn push_frames(&mut self, count: usize) {
            for _ in 0..count {
                self.now += 100;
                self.heartbeat = self.now;
                self.frames.push(self.now as i64);
                self.produced += 1;
            }
        }

        /// Encoder or capture thread stopped reporting. The watchdog restarts it.
        fn hang(&mut self, frozen_for_ms: u64) {
            self.now += frozen_for_ms;
            assert!(watchdog_due(self.heartbeat, self.now, WATCHDOG_LIMIT), "a hung encoder must trip the watchdog");
            self.restart(self.format);
        }

        fn wake_burst(&mut self, events: usize) {
            let mut pending = false;
            let mut arms = 0;
            for _ in 0..events {
                match wake_schedule(false, pending) {
                    WakeSchedule::Arm => {
                        pending = true;
                        arms += 1;
                    }
                    WakeSchedule::Coalesce => {}
                    WakeSchedule::Defer => {}
                }
            }
            // A burst that arrives while the restart is on the stack must not start a second one.
            assert_eq!(wake_schedule(true, pending), WakeSchedule::Defer);
            assert_eq!(arms, 1, "a wake burst must schedule one restart");
            self.restart(self.format);
        }

        fn change_resolution(&mut self, w: u32, h: u32) {
            self.restart(Fmt { w, h, tag: self.format.tag.wrapping_add(1) });
        }

        fn restart(&mut self, new_format: Fmt) {
            let old_entry = [self.format.tag];
            let new_entry = [new_format.tag];
            let old = Encoding { width: self.format.w, height: self.format.h, sample_entry: &old_entry };
            let new = Encoding { width: new_format.w, height: new_format.h, sample_entry: &new_entry };
            if !self.frames.is_empty() && !same_encoding(old, new) {
                self.recovered.push(std::mem::take(&mut self.frames));
            }
            self.format = new_format;
            self.heartbeat = self.now;
        }

        fn evidence(&self) -> usize {
            self.frames.len() + self.recovered.iter().map(Vec::len).sum::<usize>()
        }
    }

    #[test]
    fn wake_events_collapse_to_one_restart() {
        assert_eq!(wake_schedule(false, false), WakeSchedule::Arm);
        assert_eq!(wake_schedule(false, true), WakeSchedule::Coalesce);
        assert_eq!(wake_schedule(true, false), WakeSchedule::Defer);
        assert_eq!(wake_schedule(true, true), WakeSchedule::Defer);
    }

    #[test]
    fn watchdog_ignores_a_live_encoder() {
        let now = 10_000;
        assert!(!watchdog_due(now - 1000, now, WATCHDOG_LIMIT));
        assert!(watchdog_due(now - 8001, now, WATCHDOG_LIMIT));
        assert!(!watchdog_due(now, now - 5, WATCHDOG_LIMIT), "a clock step backwards is not a hang");
    }

    #[test]
    fn different_parameters_cannot_share_a_buffer() {
        let sps = [1u8, 2, 3];
        let same = Encoding { width: 1920, height: 1080, sample_entry: &sps };
        assert!(same_encoding(same, same));
        assert!(!same_encoding(same, Encoding { width: 1280, ..same }));
        assert!(!same_encoding(same, Encoding { sample_entry: &[9, 9], ..same }));
    }

    #[test]
    fn crash_restart_backoff_grows_and_stays_bounded() {
        assert_eq!(restart_backoff(0), Duration::from_secs(1));
        assert_eq!(restart_backoff(3), Duration::from_secs(8));
        assert_eq!(restart_backoff(100), Duration::from_secs(60));
        assert!(should_restart_process(false, true));
        assert!(!should_restart_process(true, true));
        assert!(!should_restart_process(false, false));
    }

    /// Encoder hangs, monitor wake bursts, and resolution changes, back to back.
    /// Every frame is still either in the buffer or in a recovered segment.
    #[test]
    fn stress_hangs_wakes_and_resolution_changes() {
        let mut session = Session::new();
        for step in 0..60 {
            session.push_frames(12);
            match step % 3 {
                0 => session.hang(9_000),
                1 => session.wake_burst(6),
                _ => session.change_resolution(1280 + (step % 5) * 64, 720 + (step % 4) * 40),
            }
            assert_eq!(
                session.evidence(),
                session.produced,
                "step {step}: a restart dropped replay frames (buffered {}, recovered segments {})",
                session.frames.len(),
                session.recovered.len()
            );
            assert!(session.heartbeat == session.now, "a restart must resume the heartbeat");
        }
        assert!(session.recovered.len() > 10, "resolution changes must recover the previous buffer, not discard it");
        assert!(!session.frames.is_empty() || session.recovered.last().is_some());
        // A same-format restart (the hang path) keeps the live buffer growing across the loop.
        let tail = session.frames.len();
        session.hang(9_000);
        assert_eq!(session.frames.len(), tail, "a hung encoder restart must keep the buffered frames");
    }
}
