//! Decisions shared by the update check and the restart controls.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadyAction {
    Notify,
    Confirm,
    Restart,
}

#[derive(Default)]
pub struct RestartPrompt(bool);

impl RestartPrompt {
    pub fn offer(&mut self, explicit: bool, capture_active: bool) -> ReadyAction {
        self.0 = explicit && capture_active;
        match (explicit, self.0) {
            (false, _) => ReadyAction::Notify,
            (true, true) => ReadyAction::Confirm,
            (true, false) => ReadyAction::Restart,
        }
    }

    pub fn is_open(&self) -> bool {
        self.0
    }

    pub fn dismiss(&mut self) {
        self.0 = false;
    }
}

pub fn apply_on_restart(auto_update: bool, recording: bool) -> bool {
    auto_update && !recording
}

/// Keep the original request while a check runs, upgrading it if the user clicks Check now.
#[derive(Default)]
pub struct CheckRequest(Option<bool>);

impl CheckRequest {
    pub fn begin(&mut self, explicit: bool) -> bool {
        let start_worker = self.0.is_none();
        self.0 = Some(explicit || self.0 == Some(true));
        start_worker
    }

    pub fn finish(&mut self) -> bool {
        self.0.take() == Some(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_download_only_notifies_even_when_idle() {
        let mut prompt = RestartPrompt::default();
        assert_eq!(prompt.offer(false, false), ReadyAction::Notify);
        assert_eq!(prompt.offer(false, true), ReadyAction::Notify);
        assert!(apply_on_restart(true, false));
        assert!(!apply_on_restart(true, true));
        assert!(!apply_on_restart(false, false));
    }

    #[test]
    fn explicit_update_restarts_or_asks_before_interrupting_capture() {
        let mut prompt = RestartPrompt::default();
        assert_eq!(prompt.offer(true, false), ReadyAction::Restart);
        assert_eq!(prompt.offer(true, true), ReadyAction::Confirm);
        assert!(prompt.is_open());
        // Later closes the prompt. The staged update can still be chosen with an active buffer.
        prompt.dismiss();
        assert!(!prompt.is_open());
        assert_eq!(prompt.offer(true, true), ReadyAction::Confirm);
    }

    #[test]
    fn explicit_check_takes_over_the_in_flight_background_download() {
        let mut check = CheckRequest::default();
        assert!(check.begin(false));
        assert!(!check.begin(true));
        assert!(!check.begin(false));
        assert!(check.finish());
        assert!(check.begin(false));
        assert!(!check.finish());
    }
}
