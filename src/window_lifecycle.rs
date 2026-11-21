#[derive(Debug, PartialEq, Eq)]
pub enum WindowAction {
    Keep,
    Hide,
    Close,
}

/// A standalone window can start the service. Once attached, it closes when that service ends.
pub fn window_action(service: Option<bool>, close_requested: bool) -> WindowAction {
    match (service, close_requested) {
        (Some(false), _) | (None, true) => WindowAction::Close,
        (Some(true), true) => WindowAction::Hide,
        _ => WindowAction::Keep,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_bar_close_hides_the_window_only_while_the_service_runs() {
        assert_eq!(window_action(Some(true), true), WindowAction::Hide);
        assert_eq!(window_action(Some(true), false), WindowAction::Keep);
        assert_eq!(window_action(None, true), WindowAction::Close);
    }

    #[test]
    fn tray_quit_and_service_death_close_even_a_hidden_window() {
        assert_eq!(window_action(Some(false), false), WindowAction::Close);
        assert_eq!(window_action(Some(false), true), WindowAction::Close);
        assert_eq!(window_action(None, false), WindowAction::Keep);
    }
}
