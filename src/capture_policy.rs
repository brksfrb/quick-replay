//! Which screen capture backend to use, and when to replace the capture session, as the app in
//! front of the captured monitor changes. Platform-independent so it is tested on any host.
//!
//! Windows.Graphics.Capture (WGC) has a known limitation with OpenGL apps in old-style exclusive
//! fullscreen (LWJGL2 games such as Minecraft 1.8.9 after F11): while a WGC session covers that
//! monitor, the game's own mouse cursor disappears and the captured picture stops updating (only
//! the cursor overlay keeps moving). DXGI Desktop Duplication keeps receiving the game's frames
//! and leaves the hardware cursor alone, so it is used while such an app is in front.
//!
//! Times are in the capture clock's 100 ns units.

pub const HNS_PER_SEC: i64 = 10_000_000;
/// A new front window must stay the same this long before the capture follows it. Toggling
/// fullscreen destroys and recreates the game window and may switch the display mode.
pub const SETTLE: i64 = HNS_PER_SEC;
/// A second refresh after a change, for display mode switches that finish late.
pub const SECOND_REFRESH: i64 = 4 * HNS_PER_SEC;
/// A fullscreen app that is being played draws all the time. No picture for this long while the
/// user is giving input means the capture stopped receiving them.
pub const STALL: i64 = 3 * HNS_PER_SEC;
/// Input within this many milliseconds counts as somebody actively playing.
pub const ACTIVE_INPUT_MS: u32 = 3_000;
/// Wait between stall refreshes; doubles while refreshes don't bring pictures back.
pub const STALL_BACKOFF_MIN: i64 = 10 * HNS_PER_SEC;
pub const STALL_BACKOFF_MAX: i64 = 5 * 60 * HNS_PER_SEC;
/// After Desktop Duplication fails to start, stay on WGC this long before trying again.
pub const DUPLICATION_RETRY: i64 = 60 * HNS_PER_SEC;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// Windows.Graphics.Capture monitor capture (the normal path, draws the cursor in).
    Wgc,
    /// DXGI Desktop Duplication (exclusive fullscreen OpenGL apps).
    Duplication,
}

/// What is in front of the captured monitor, sampled about once a second.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Front {
    /// Foreground window handle (0 when there is none).
    pub hwnd: isize,
    /// The foreground window covers the whole captured monitor.
    pub fullscreen: bool,
    /// The foreground app renders with OpenGL.
    pub opengl: bool,
}

impl Front {
    fn wanted_backend(self) -> Backend {
        if self.fullscreen && self.opengl {
            Backend::Duplication
        } else {
            Backend::Wgc
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Replace the current capture session with a fresh one of the same backend.
    Refresh(Reason),
    /// Change backends (also a fresh session).
    Switch(Backend, Reason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// A window entered or left fullscreen, or a fullscreen window was replaced.
    FullscreenChanged,
    /// Follow-up refresh after a fullscreen change.
    Settled,
    /// No pictures while a fullscreen app is in front and the user is active.
    Stalled,
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Reason::FullscreenChanged => "fullscreen changed",
            Reason::Settled => "fullscreen change settled",
            Reason::Stalled => "no new pictures from a fullscreen app",
        })
    }
}

pub struct CaptureWatch {
    backend: Backend,
    /// Last front state that was acted on.
    applied: Option<Front>,
    /// A changed front state and when it was first seen.
    candidate: Option<(Front, i64)>,
    second_refresh_at: Option<i64>,
    last_frame: i64,
    next_stall_refresh: i64,
    stall_backoff: i64,
    duplication_blocked_until: i64,
}

impl CaptureWatch {
    pub fn new(now: i64) -> CaptureWatch {
        CaptureWatch {
            backend: Backend::Wgc,
            applied: None,
            candidate: None,
            second_refresh_at: None,
            last_frame: now,
            next_stall_refresh: now,
            stall_backoff: STALL_BACKOFF_MIN,
            duplication_blocked_until: i64::MIN,
        }
    }

    #[cfg(test)]
    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// A picture arrived from the capture.
    pub fn frame_arrived(&mut self, now: i64) {
        self.last_frame = now;
        self.stall_backoff = STALL_BACKOFF_MIN;
    }

    /// The caller carried out `action` (or tried to). `ok == false` for a failed switch to
    /// Desktop Duplication keeps WGC and blocks retries for a while.
    pub fn applied(&mut self, action: Action, ok: bool, now: i64) {
        match action {
            Action::Switch(backend, _) if ok => self.backend = backend,
            Action::Switch(Backend::Duplication, _) => self.duplication_blocked_until = now + DUPLICATION_RETRY,
            _ => {}
        }
        // A fresh session gets the full stall window before it is judged.
        self.last_frame = now;
    }

    /// Desktop Duplication could not be kept running; the caller went back to WGC.
    pub fn duplication_failed(&mut self, now: i64) {
        self.backend = Backend::Wgc;
        self.duplication_blocked_until = now + DUPLICATION_RETRY;
        self.last_frame = now;
    }

    /// The current session reported itself lost (Desktop Duplication after a mode switch, or a
    /// closed WGC item) and the caller reopened it.
    pub fn lost(&mut self, now: i64) {
        self.last_frame = now;
    }

    /// Called about once a second with what is in front and how long the user has been idle.
    pub fn check(&mut self, now: i64, front: Front, input_idle_ms: u32) -> Option<Action> {
        let Some(applied) = self.applied else {
            // First look: just pick the right backend, no refresh of a session that just started.
            self.applied = Some(front);
            return self.backend_action(front, now, Reason::FullscreenChanged);
        };

        if front != applied {
            match self.candidate {
                Some((seen, since)) if seen == front => {
                    if now - since >= SETTLE {
                        self.candidate = None;
                        self.applied = Some(front);
                        if let Some(action) = self.backend_action(front, now, Reason::FullscreenChanged) {
                            self.second_refresh_at = Some(now + SECOND_REFRESH);
                            return Some(action);
                        }
                        if relevant_change(applied, front) {
                            self.second_refresh_at = Some(now + SECOND_REFRESH);
                            return Some(Action::Refresh(Reason::FullscreenChanged));
                        }
                    }
                }
                _ => self.candidate = Some((front, now)),
            }
            return None;
        }
        self.candidate = None;

        if let Some(at) = self.second_refresh_at {
            if now >= at {
                self.second_refresh_at = None;
                return Some(
                    self.backend_action(front, now, Reason::Settled).unwrap_or(Action::Refresh(Reason::Settled)),
                );
            }
            return None;
        }

        // Duplication that was blocked may be retried later.
        if let Some(action) = self.backend_action(front, now, Reason::FullscreenChanged) {
            return Some(action);
        }

        let playing = front.fullscreen && input_idle_ms < ACTIVE_INPUT_MS;
        if playing && now - self.last_frame >= STALL && now >= self.next_stall_refresh {
            self.next_stall_refresh = now + self.stall_backoff;
            self.stall_backoff = (self.stall_backoff * 2).min(STALL_BACKOFF_MAX);
            return Some(Action::Refresh(Reason::Stalled));
        }
        None
    }

    fn backend_action(&self, front: Front, now: i64, reason: Reason) -> Option<Action> {
        let mut wanted = front.wanted_backend();
        if wanted == Backend::Duplication && now < self.duplication_blocked_until {
            wanted = Backend::Wgc;
        }
        (wanted != self.backend).then_some(Action::Switch(wanted, reason))
    }
}

/// Entering or leaving fullscreen, or one fullscreen window replacing another (LWJGL2 recreates
/// its window on every F11). Ordinary window switching on the desktop is not worth a refresh.
fn relevant_change(old: Front, new: Front) -> bool {
    old.fullscreen != new.fullscreen || (new.fullscreen && old.hwnd != new.hwnd)
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i64 = HNS_PER_SEC;
    const DESKTOP: Front = Front { hwnd: 1, fullscreen: false, opengl: false };
    const MC_WINDOWED: Front = Front { hwnd: 10, fullscreen: false, opengl: true };
    const MC_FULLSCREEN: Front = Front { hwnd: 11, fullscreen: true, opengl: true };
    const D3D_FULLSCREEN: Front = Front { hwnd: 20, fullscreen: true, opengl: false };
    const ACTIVE: u32 = 100;
    const IDLE: u32 = 10 * 60 * 1000;

    /// Runs one check per second from `from` to `to` (exclusive), feeding frames, and returns
    /// the actions with their times, applying each one successfully.
    fn run(w: &mut CaptureWatch, front: Front, from: i64, to: i64, frames: bool) -> Vec<(i64, Action)> {
        let mut out = Vec::new();
        let mut t = from;
        while t < to {
            if frames {
                w.frame_arrived(t);
            }
            if let Some(action) = w.check(t, front, ACTIVE) {
                w.applied(action, true, t);
                out.push((t, action));
            }
            t += S;
        }
        out
    }

    #[test]
    fn windowed_desktop_never_refreshes() {
        let mut w = CaptureWatch::new(0);
        assert!(run(&mut w, DESKTOP, 0, 120 * S, true).is_empty());
        // Switching between normal windows is not a fullscreen change.
        assert!(run(&mut w, MC_WINDOWED, 120 * S, 240 * S, true).is_empty());
        assert_eq!(w.backend(), Backend::Wgc);
    }

    #[test]
    fn f11_in_opengl_game_switches_to_duplication_and_back() {
        let mut w = CaptureWatch::new(0);
        assert!(run(&mut w, MC_WINDOWED, 0, 10 * S, true).is_empty());
        // F11: new fullscreen window. Wait SETTLE, then switch.
        let actions = run(&mut w, MC_FULLSCREEN, 10 * S, 30 * S, true);
        assert_eq!(actions[0], (11 * S, Action::Switch(Backend::Duplication, Reason::FullscreenChanged)));
        assert_eq!(actions[1].1, Action::Refresh(Reason::Settled));
        assert_eq!(actions.len(), 2);
        assert_eq!(w.backend(), Backend::Duplication);
        // F11 again: back to WGC.
        let actions = run(&mut w, MC_WINDOWED, 30 * S, 50 * S, true);
        assert_eq!(actions[0], (31 * S, Action::Switch(Backend::Wgc, Reason::FullscreenChanged)));
        assert_eq!(w.backend(), Backend::Wgc);
    }

    #[test]
    fn flicker_shorter_than_settle_is_ignored() {
        let mut w = CaptureWatch::new(0);
        run(&mut w, MC_WINDOWED, 0, 5 * S, true);
        // One sample of a transient window, then back.
        assert_eq!(w.check(5 * S, MC_FULLSCREEN, ACTIVE), None);
        assert!(run(&mut w, MC_WINDOWED, 6 * S, 20 * S, true).is_empty());
    }

    #[test]
    fn recreated_fullscreen_window_refreshes() {
        let mut w = CaptureWatch::new(0);
        run(&mut w, D3D_FULLSCREEN, 0, 10 * S, true);
        let replaced = Front { hwnd: 21, ..D3D_FULLSCREEN };
        let actions = run(&mut w, replaced, 10 * S, 20 * S, true);
        assert_eq!(actions[0], (11 * S, Action::Refresh(Reason::FullscreenChanged)));
        assert_eq!(w.backend(), Backend::Wgc);
    }

    #[test]
    fn stall_while_playing_refreshes_with_backoff() {
        let mut w = CaptureWatch::new(0);
        run(&mut w, D3D_FULLSCREEN, 0, 5 * S, true);
        // Pictures stop at 4 s while the user keeps playing.
        let actions = run(&mut w, D3D_FULLSCREEN, 5 * S, 60 * S, false);
        let times: Vec<i64> = actions.iter().map(|(t, _)| *t / S).collect();
        assert!(actions.iter().all(|(_, a)| *a == Action::Refresh(Reason::Stalled)));
        // First after STALL, then 10 s, 20 s, ... apart.
        assert_eq!(times, vec![7, 17, 37]);
        // Pictures back: backoff resets.
        run(&mut w, D3D_FULLSCREEN, 60 * S, 70 * S, true);
        assert_eq!(w.stall_backoff, STALL_BACKOFF_MIN);
    }

    #[test]
    fn no_stall_refresh_when_idle_or_windowed() {
        let mut w = CaptureWatch::new(0);
        w.check(0, D3D_FULLSCREEN, IDLE);
        for t in 1..100 {
            assert_eq!(w.check(t * S, D3D_FULLSCREEN, IDLE), None);
        }
        let mut w = CaptureWatch::new(0);
        assert!(run(&mut w, DESKTOP, 0, 100 * S, false).is_empty());
    }

    #[test]
    fn duplication_lost_for_good_falls_back_to_wgc() {
        let mut w = CaptureWatch::new(0);
        run(&mut w, MC_FULLSCREEN, 0, 10 * S, true);
        assert_eq!(w.backend(), Backend::Duplication);
        w.duplication_failed(10 * S);
        assert_eq!(w.backend(), Backend::Wgc);
        // Retried once the block expires, not before.
        let actions = run(&mut w, MC_FULLSCREEN, 11 * S, 80 * S, true);
        assert_eq!(actions, vec![(70 * S, Action::Switch(Backend::Duplication, Reason::FullscreenChanged))]);
    }

    #[test]
    fn failed_duplication_stays_on_wgc_then_retries() {
        let mut w = CaptureWatch::new(0);
        let first = w.check(0, MC_FULLSCREEN, ACTIVE).unwrap();
        assert_eq!(first, Action::Switch(Backend::Duplication, Reason::FullscreenChanged));
        w.applied(first, false, 0);
        assert_eq!(w.backend(), Backend::Wgc);
        for t in 1..60 {
            w.frame_arrived(t * S);
            assert_eq!(w.check(t * S, MC_FULLSCREEN, ACTIVE), None, "at {t}s");
        }
        assert_eq!(
            w.check(60 * S, MC_FULLSCREEN, ACTIVE),
            Some(Action::Switch(Backend::Duplication, Reason::FullscreenChanged))
        );
    }
}
