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
/// After Desktop Duplication fails to start, stay on WGC this long before trying again; doubles
/// with every failure in a row (the start often fails only while the game is still switching
/// display modes after F11).
pub const DUPLICATION_RETRY_MIN: i64 = 2 * HNS_PER_SEC;
pub const DUPLICATION_RETRY_MAX: i64 = 60 * HNS_PER_SEC;
/// A lost Desktop Duplication is reopened right away the first time, then after this long,
/// doubling up to `REOPEN_MAX` while reopening fails or the new session is lost again.
pub const REOPEN_MIN: i64 = HNS_PER_SEC / 4;
pub const REOPEN_MAX: i64 = 5 * HNS_PER_SEC;
/// A reopened duplication that lasts this long ends the loss episode (and its backoff).
pub const REOPEN_STABLE: i64 = 10 * HNS_PER_SEC;
/// While a duplication keeps failing or getting lost, a reminder is logged at most this often.
pub const REOPEN_LOG_EVERY: i64 = 60 * HNS_PER_SEC;

/// `DXGI_ERROR_WAIT_TIMEOUT`: no new picture within the wait. Normal, not a loss.
pub const HR_WAIT_TIMEOUT: i32 = 0x887A_0027_u32 as i32;

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
    duplication_retry: i64,
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
            duplication_retry: DUPLICATION_RETRY_MIN,
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
    /// Desktop Duplication keeps WGC and blocks retries for a while (growing with each failure).
    /// A failed refresh of a duplication leaves it released: the caller's `DuplicationRecovery`
    /// reopens it, the backend stays Duplication (WGC would hide the game's cursor again).
    pub fn applied(&mut self, action: Action, ok: bool, now: i64) {
        match action {
            Action::Switch(backend, _) if ok => {
                self.backend = backend;
                if backend == Backend::Duplication {
                    self.duplication_retry = DUPLICATION_RETRY_MIN;
                }
            }
            Action::Switch(Backend::Duplication, _) => {
                self.duplication_blocked_until = now + self.duplication_retry;
                self.duplication_retry = (self.duplication_retry * 2).min(DUPLICATION_RETRY_MAX);
            }
            _ => {}
        }
        // A fresh session gets the full stall window before it is judged.
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
                        // Desktop Duplication follows the monitor, not a window: a recreated
                        // fullscreen window needs no new session (a mode switch shows up as a
                        // lost duplication instead).
                        if relevant_change(applied, front) && self.backend == Backend::Wgc {
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
                if let Some(action) = self.backend_action(front, now, Reason::Settled) {
                    return Some(action);
                }
                // A late display mode switch invalidates a duplication by itself (reported as lost
                // and reopened then); refreshing a healthy one only drops pictures. 0.1.8 did, and
                // its new session failed with E_INVALIDARG while the old one was still held.
                return (self.backend == Backend::Wgc).then_some(Action::Refresh(Reason::Settled));
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

/// Order of operations when one capture session replaces another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReopenOrder {
    /// Open the new session, then shut the old one down: a failed open keeps the old session.
    OpenNewFirst,
    /// Release the old session before opening the new one. DXGI allows one
    /// `IDXGIOutputDuplication` per output and process: `DuplicateOutput` fails with
    /// E_INVALIDARG while the old one is alive, and releasing the old one after a successful
    /// open invalidates the new one (ACCESS_LOST on its first frame).
    ReleaseOldFirst,
}

pub fn reopen_order(from: Backend, to: Backend) -> ReopenOrder {
    if from == Backend::Duplication && to == Backend::Duplication {
        ReopenOrder::ReleaseOldFirst
    } else {
        ReopenOrder::OpenNewFirst
    }
}

/// The service asks for a fresh session when an OpenGL fullscreen app shows up (windows hidden
/// from WGC can leave its capture frozen). Desktop Duplication is not affected by that, and a
/// lost one is already being reopened by `DuplicationRecovery`.
pub fn external_refresh_needed(backend: Backend) -> bool {
    backend == Backend::Wgc
}

/// What `IDXGIOutputDuplication::AcquireNextFrame` failing with `hresult` means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcquireFailure {
    /// No new picture yet: keep the session and the last picture.
    NoNewFrame,
    /// ACCESS_LOST (mode switch, secure desktop, fullscreen transition) or anything else the
    /// session can't continue from: release it and reopen.
    Lost,
}

pub fn acquire_failure(hresult: i32) -> AcquireFailure {
    if hresult == HR_WAIT_TIMEOUT {
        AcquireFailure::NoNewFrame
    } else {
        AcquireFailure::Lost
    }
}

/// What the caller should log about a lost Desktop Duplication. Everything else stays quiet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryNote {
    /// A loss episode started.
    Lost,
    /// The session keeps getting lost right after being reopened.
    KeepsLosing { losses: u32, seconds: i64 },
    /// First successful reopen in this episode.
    Reopened { after_ms: i64, attempts: u32 },
    /// Reopening failed (first failure, then a reminder at most every `REOPEN_LOG_EVERY`).
    ReopenFailing { attempts: u32, seconds: i64 },
    /// The episode ended after more than one loss.
    Recovered { losses: u32, attempts: u32, seconds: i64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LostStep {
    pub note: Option<RecoveryNote>,
    /// Reopen now (otherwise wait: backoff).
    pub try_now: bool,
}

/// Reopening a lost Desktop Duplication with backoff and quiet logging. It never gives up on
/// duplication while it is the wanted backend: WGC would hide the game's cursor and freeze.
#[derive(Default)]
pub struct DuplicationRecovery {
    episode: Option<Episode>,
}

struct Episode {
    started: i64,
    losses: u32,
    attempts: u32,
    failures: u32,
    /// Lost and not reopened yet.
    waiting: bool,
    delay: i64,
    next_try: i64,
    last_ok: Option<i64>,
    last_note: Option<i64>,
}

fn next_delay(delay: i64) -> i64 {
    if delay == 0 {
        REOPEN_MIN
    } else {
        (delay * 2).min(REOPEN_MAX)
    }
}

impl Episode {
    /// Reminders are rate-limited; the first note of each kind is always logged.
    fn note_due(&mut self, now: i64) -> bool {
        let due = self.last_note.is_none_or(|at| now - at >= REOPEN_LOG_EVERY);
        if due {
            self.last_note = Some(now);
        }
        due
    }
}

impl DuplicationRecovery {
    /// Call on every loop pass while the duplication is lost (or released).
    pub fn lost(&mut self, now: i64) -> LostStep {
        if self.episode.as_ref().is_some_and(|ep| !ep.waiting && ep.last_ok.is_some_and(|ok| now - ok >= REOPEN_STABLE))
        {
            self.episode = None;
        }
        let mut note = None;
        let ep = self.episode.get_or_insert_with(|| {
            note = Some(RecoveryNote::Lost);
            Episode {
                started: now,
                losses: 0,
                attempts: 0,
                failures: 0,
                waiting: false,
                delay: 0,
                next_try: now,
                last_ok: None,
                last_note: Some(now),
            }
        });
        if !ep.waiting {
            ep.waiting = true;
            ep.losses += 1;
            if ep.losses > 1 {
                // Lost again soon after a reopen: don't reopen in a tight loop.
                ep.delay = next_delay(ep.delay);
                ep.next_try = now + ep.delay;
                if ep.note_due(now) || ep.losses == 2 {
                    note = Some(RecoveryNote::KeepsLosing {
                        losses: ep.losses,
                        seconds: (now - ep.started) / HNS_PER_SEC,
                    });
                }
            }
        }
        LostStep { note, try_now: now >= ep.next_try }
    }

    /// The reopen asked for by `lost` succeeded.
    pub fn reopened(&mut self, now: i64) -> Option<RecoveryNote> {
        let ep = self.episode.as_mut()?;
        ep.attempts += 1;
        ep.waiting = false;
        let first = ep.last_ok.is_none();
        ep.last_ok = Some(now);
        first.then_some(RecoveryNote::Reopened {
            after_ms: (now - ep.started) / (HNS_PER_SEC / 1000),
            attempts: ep.attempts,
        })
    }

    /// The reopen asked for by `lost` failed; the duplication stays released.
    pub fn failed(&mut self, now: i64) -> Option<RecoveryNote> {
        let ep = self.episode.as_mut()?;
        ep.attempts += 1;
        ep.failures += 1;
        ep.delay = next_delay(ep.delay);
        ep.next_try = now + ep.delay;
        (ep.note_due(now) || ep.failures == 1)
            .then_some(RecoveryNote::ReopenFailing { attempts: ep.attempts, seconds: (now - ep.started) / HNS_PER_SEC })
    }

    /// Call while the duplication is healthy. Ends an episode once the reopened session lasted.
    pub fn tick(&mut self, now: i64) -> Option<RecoveryNote> {
        let ep = self.episode.as_ref()?;
        if ep.waiting || ep.last_ok.is_none_or(|ok| now - ok < REOPEN_STABLE) {
            return None;
        }
        let ep = self.episode.take()?;
        (ep.losses > 1).then_some(RecoveryNote::Recovered {
            losses: ep.losses,
            attempts: ep.attempts,
            seconds: (now - ep.started) / HNS_PER_SEC,
        })
    }

    /// A different session took over (backend switch or a fresh duplication from the policy).
    pub fn reset(&mut self) {
        self.episode = None;
    }

    #[cfg(test)]
    fn waiting(&self) -> bool {
        self.episode.as_ref().is_some_and(|ep| ep.waiting)
    }
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
        // No settle refresh of a healthy duplication (0.1.8 did one, and it failed).
        assert_eq!(actions, vec![(11 * S, Action::Switch(Backend::Duplication, Reason::FullscreenChanged))]);
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
    fn d3d_fullscreen_on_wgc_still_gets_settle_refresh() {
        let mut w = CaptureWatch::new(0);
        run(&mut w, DESKTOP, 0, 5 * S, true);
        let actions = run(&mut w, D3D_FULLSCREEN, 5 * S, 20 * S, true);
        assert_eq!(
            actions,
            vec![(6 * S, Action::Refresh(Reason::FullscreenChanged)), (10 * S, Action::Refresh(Reason::Settled))]
        );
    }

    #[test]
    fn recreated_opengl_fullscreen_window_keeps_duplication_session() {
        let mut w = CaptureWatch::new(0);
        run(&mut w, MC_FULLSCREEN, 0, 10 * S, true);
        assert_eq!(w.backend(), Backend::Duplication);
        // LWJGL2 replaces its fullscreen window: the duplication follows the monitor.
        let replaced = Front { hwnd: 12, ..MC_FULLSCREEN };
        assert!(run(&mut w, replaced, 10 * S, 30 * S, true).is_empty());
        assert_eq!(w.backend(), Backend::Duplication);
    }

    #[test]
    fn alt_tab_out_and_back_switches_once_each_way() {
        let mut w = CaptureWatch::new(0);
        run(&mut w, MC_FULLSCREEN, 0, 10 * S, true);
        let out = run(&mut w, DESKTOP, 10 * S, 20 * S, true);
        assert_eq!(
            out,
            vec![
                (11 * S, Action::Switch(Backend::Wgc, Reason::FullscreenChanged)),
                (15 * S, Action::Refresh(Reason::Settled))
            ]
        );
        let back = run(&mut w, MC_FULLSCREEN, 20 * S, 40 * S, true);
        assert_eq!(back, vec![(21 * S, Action::Switch(Backend::Duplication, Reason::FullscreenChanged))]);
    }

    #[test]
    fn failed_duplication_retries_with_growing_backoff() {
        let mut w = CaptureWatch::new(0);
        let mut tries = Vec::new();
        for t in 0..200 {
            w.frame_arrived(t * S);
            if let Some(action) = w.check(t * S, MC_FULLSCREEN, ACTIVE) {
                assert_eq!(action, Action::Switch(Backend::Duplication, Reason::FullscreenChanged));
                // Fails until 100 s.
                w.applied(action, t >= 100, t * S);
                tries.push(t);
            }
        }
        assert_eq!(tries, vec![0, 2, 6, 14, 30, 62, 122]);
        assert_eq!(w.backend(), Backend::Duplication);
        // Success resets the backoff.
        assert_eq!(w.duplication_retry, DUPLICATION_RETRY_MIN);
    }

    #[test]
    fn duplication_is_reopened_release_first() {
        assert_eq!(reopen_order(Backend::Duplication, Backend::Duplication), ReopenOrder::ReleaseOldFirst);
        assert_eq!(reopen_order(Backend::Wgc, Backend::Duplication), ReopenOrder::OpenNewFirst);
        assert_eq!(reopen_order(Backend::Duplication, Backend::Wgc), ReopenOrder::OpenNewFirst);
        assert_eq!(reopen_order(Backend::Wgc, Backend::Wgc), ReopenOrder::OpenNewFirst);
    }

    #[test]
    fn external_refresh_skips_duplication() {
        assert!(external_refresh_needed(Backend::Wgc));
        assert!(!external_refresh_needed(Backend::Duplication));
    }

    #[test]
    fn wait_timeout_is_not_access_lost() {
        assert_eq!(acquire_failure(HR_WAIT_TIMEOUT), AcquireFailure::NoNewFrame);
        let access_lost = 0x887A_0026_u32 as i32;
        assert_eq!(acquire_failure(access_lost), AcquireFailure::Lost);
        let invalid_call = 0x887A_0001_u32 as i32;
        assert_eq!(acquire_failure(invalid_call), AcquireFailure::Lost);
    }

    #[test]
    fn single_loss_reopens_immediately_with_two_log_lines() {
        let mut r = DuplicationRecovery::default();
        let step = r.lost(100 * S);
        assert_eq!(step, LostStep { note: Some(RecoveryNote::Lost), try_now: true });
        assert_eq!(r.reopened(100 * S), Some(RecoveryNote::Reopened { after_ms: 0, attempts: 1 }));
        for t in 101..200 {
            assert_eq!(r.tick(t * S), None);
        }
        // Lost again much later: a new episode, reopened right away again.
        assert_eq!(r.lost(300 * S), LostStep { note: Some(RecoveryNote::Lost), try_now: true });
    }

    /// The 0.1.8 log: the reopened duplication was lost again on every poll and reopened once a
    /// second, 127 times in a row, with a log line each time.
    #[test]
    fn repeated_losses_back_off_and_log_little() {
        let mut r = DuplicationRecovery::default();
        let step = S / 144;
        let (mut reopens, mut notes) = (0, 0);
        let mut t = 0;
        while t < 130 * S {
            let lost = r.lost(t);
            notes += usize::from(lost.note.is_some());
            if lost.try_now {
                reopens += 1;
                notes += usize::from(r.reopened(t).is_some());
            }
            t += step;
        }
        assert!(reopens <= 2 + 130 / 5 + 5, "{reopens} reopens");
        // Lost, Reopened, KeepsLosing, and a reminder per minute.
        assert!(notes <= 5, "{notes} log lines");
    }

    #[test]
    fn failing_reopens_keep_trying_with_backoff_and_quiet_log() {
        let mut r = DuplicationRecovery::default();
        let mut tries = Vec::new();
        let mut notes = Vec::new();
        let mut t = 0;
        while t < 120 * S {
            let lost = r.lost(t);
            notes.extend(lost.note);
            if lost.try_now {
                tries.push(t);
                notes.extend(r.failed(t));
            }
            t += S / 100;
        }
        // Never gives up (no fallback to WGC), never faster than the backoff allows.
        assert!(tries.len() > 20 && tries.len() < 40, "{} tries", tries.len());
        for pair in tries.windows(2).skip(5) {
            assert!(pair[1] - pair[0] >= REOPEN_MAX);
        }
        assert!(r.waiting());
        // Lost + first failure + one reminder per minute.
        assert!(notes.len() <= 4, "{notes:?}");
        assert_eq!(notes[0], RecoveryNote::Lost);
        assert!(matches!(notes[1], RecoveryNote::ReopenFailing { attempts: 1, .. }));
        // Then it works again.
        let lost = r.lost(t + REOPEN_MAX);
        assert!(lost.try_now);
        assert!(matches!(r.reopened(t + REOPEN_MAX), Some(RecoveryNote::Reopened { .. })));
        assert!(!r.waiting());
    }

    #[test]
    fn flapping_episode_ends_with_one_summary() {
        let mut r = DuplicationRecovery::default();
        for i in 0..3 {
            let t = i * S;
            let lost = r.lost(t);
            assert!(i == 0 || !lost.try_now || lost.note.is_none());
            let t = t + REOPEN_MAX;
            assert!(r.lost(t).try_now);
            r.reopened(t);
        }
        let end = 2 * S + REOPEN_MAX + REOPEN_STABLE;
        assert_eq!(r.tick(end - 1), None);
        assert!(matches!(r.tick(end), Some(RecoveryNote::Recovered { losses: 3, .. })));
        assert_eq!(r.tick(end + S), None);
    }
}
