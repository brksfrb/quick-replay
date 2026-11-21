//! The manual update check that has to wait for the tray service to start.

use std::io;
use std::time::{Duration, Instant};

const SERVICE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(isize)]
pub enum UpdateStatus {
    Checking = 1,
    UpToDate = 2,
    Available = 3,
    Failed = 4,
    Disabled = 5,
    Installed = 6,
    InstallFailed = 7,
}

impl UpdateStatus {
    #[cfg(windows)]
    pub fn decode(value: usize) -> Option<Self> {
        [
            Self::Checking,
            Self::UpToDate,
            Self::Available,
            Self::Failed,
            Self::Disabled,
            Self::Installed,
            Self::InstallFailed,
        ]
        .into_iter()
        .find(|status| *status as usize == value)
    }

    fn message(self) -> &'static str {
        match self {
            Self::Checking => "Checking for updates…",
            Self::UpToDate => "QuickReplay is up to date.",
            Self::Available => "A QuickReplay update is available.",
            Self::Failed => "Couldn't check for updates.",
            Self::Disabled => "Updates are off in this build.",
            Self::Installed => "QuickReplay updated successfully.",
            Self::InstallFailed => "Couldn't install the QuickReplay update.",
        }
    }
}

#[derive(Default)]
pub enum CheckNow {
    #[default]
    Idle,
    Starting(Instant),
    Checking(Instant),
    Finished(UpdateStatus),
    Error(&'static str),
}

impl CheckNow {
    /// Returns true when the existing running-service path accepted the check.
    pub fn click(
        &mut self,
        now: Instant,
        post_check: impl FnOnce() -> bool,
        service_running: impl FnOnce() -> bool,
        start_service: impl FnOnce() -> io::Result<()>,
    ) -> bool {
        if self.busy() {
            return false;
        }
        *self = Self::Idle;
        if post_check() {
            return true;
        }
        // The service takes its mutex before creating the IPC window.
        if !service_running() {
            if let Err(err) = start_service() {
                log::error!("could not start QuickReplay to check for updates: {err}");
                *self = Self::Error("Couldn't start QuickReplay to check for updates.");
                return false;
            }
        }
        *self = Self::Starting(now);
        false
    }

    pub fn poll(&mut self, now: Instant, request: impl FnOnce(bool) -> Option<UpdateStatus>) {
        let (start_check, since) = match *self {
            Self::Starting(since) => (true, since),
            Self::Checking(since) => (false, since),
            Self::Idle => {
                if let Some(status @ (UpdateStatus::Installed | UpdateStatus::InstallFailed)) = request(false) {
                    *self = Self::Finished(status);
                }
                return;
            }
            _ => return,
        };
        *self = match request(start_check) {
            Some(UpdateStatus::Checking) => Self::Checking(now),
            Some(status) => Self::Finished(status),
            None if now.duration_since(since) >= SERVICE_TIMEOUT => {
                if start_check {
                    log::warn!("QuickReplay did not accept the update check within {SERVICE_TIMEOUT:?} of startup");
                    Self::Error("QuickReplay didn't finish starting. Try checking for updates again.")
                } else {
                    log::warn!("QuickReplay stopped responding while checking for updates");
                    Self::Error("Lost contact with QuickReplay while checking for updates.")
                }
            }
            None => return,
        };
    }

    pub fn busy(&self) -> bool {
        matches!(self, Self::Starting(_) | Self::Checking(_))
    }

    pub fn message(&self) -> Option<&'static str> {
        match *self {
            Self::Idle => None,
            Self::Starting(_) => Some("Starting QuickReplay and checking for updates…"),
            Self::Checking(_) => Some(UpdateStatus::Checking.message()),
            Self::Finished(status) => Some(status.message()),
            Self::Error(message) => Some(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopped_service_is_started_before_checking_once() {
        let now = Instant::now();
        let mut check = CheckNow::default();
        let mut starts = 0;
        assert!(!check.click(
            now,
            || false,
            || false,
            || {
                starts += 1;
                Ok(())
            }
        ));
        assert_eq!(starts, 1);
        assert_eq!(check.message(), Some("Starting QuickReplay and checking for updates…"));
        assert!(!check.click(now, || panic!("already waiting"), || unreachable!(), || unreachable!()));
        check.poll(now, |start| {
            assert!(start);
            None
        });
        assert!(check.busy());
        check.poll(now, |start| {
            assert!(start);
            Some(UpdateStatus::Checking)
        });
        assert_eq!(check.message(), Some("Checking for updates…"));
        check.poll(now, |start| {
            assert!(!start);
            Some(UpdateStatus::UpToDate)
        });
        assert!(!check.busy());
        assert_eq!(check.message(), Some("QuickReplay is up to date."));
    }

    #[test]
    fn running_service_gets_the_existing_check_without_starting() {
        let mut check = CheckNow::default();
        assert!(check.click(Instant::now(), || true, || unreachable!(), || unreachable!()));
        assert_eq!(check.message(), None);
        assert!(!check.busy());
    }

    #[test]
    fn launch_error_is_shown_and_can_be_retried() {
        let now = Instant::now();
        let mut check = CheckNow::default();
        assert!(!check.click(now, || false, || false, || Err(io::ErrorKind::PermissionDenied.into())));
        assert_eq!(check.message(), Some("Couldn't start QuickReplay to check for updates."));
        assert!(!check.busy());
        assert!(check.click(now, || true, || unreachable!(), || unreachable!()));
        assert_eq!(check.message(), None);
    }

    #[test]
    fn already_starting_service_is_reused_and_startup_times_out() {
        let now = Instant::now();
        let mut check = CheckNow::default();
        assert!(!check.click(now, || false, || true, || panic!("service already starting")));
        check.poll(now + SERVICE_TIMEOUT, |_| None);
        assert_eq!(check.message(), Some("QuickReplay didn't finish starting. Try checking for updates again."));
        assert!(!check.busy());
    }

    #[test]
    fn check_can_finish_after_the_service_restarts_for_an_update() {
        let now = Instant::now();
        let mut check = CheckNow::Checking(now);
        check.poll(now + Duration::from_secs(1), |_| None);
        assert!(check.busy());
        check.poll(now + Duration::from_secs(2), |_| Some(UpdateStatus::Installed));
        assert_eq!(check.message(), Some("QuickReplay updated successfully."));
        assert!(!check.busy());
    }

    #[test]
    fn check_reports_when_the_service_does_not_return() {
        let now = Instant::now();
        let mut check = CheckNow::Checking(now);
        check.poll(now + SERVICE_TIMEOUT, |_| None);
        assert_eq!(check.message(), Some("Lost contact with QuickReplay while checking for updates."));
        assert!(!check.busy());
    }

    #[test]
    fn the_first_window_shows_the_result_of_an_update_restart() {
        let mut check = CheckNow::default();
        check.poll(Instant::now(), |start| {
            assert!(!start);
            Some(UpdateStatus::Installed)
        });
        assert_eq!(check.message(), Some("QuickReplay updated successfully."));
    }

    #[test]
    fn other_update_results_are_visible_in_settings() {
        for (status, message) in [
            (UpdateStatus::Available, "A QuickReplay update is available."),
            (UpdateStatus::Failed, "Couldn't check for updates."),
            (UpdateStatus::Disabled, "Updates are off in this build."),
            (UpdateStatus::InstallFailed, "Couldn't install the QuickReplay update."),
        ] {
            let now = Instant::now();
            let mut check = CheckNow::Checking(now);
            check.poll(now, |_| Some(status));
            assert_eq!(check.message(), Some(message));
            assert!(!check.busy());
        }
    }
}
