/// Try the saved endpoint first and return any error that caused a fallback.
/// A missing preference goes directly to the current Windows default.
pub fn connect<E>(
    preferred: Option<&str>,
    mut connect_endpoint: impl FnMut(Option<&str>) -> Result<(), E>,
) -> (Option<E>, Result<(), E>) {
    let preferred_error = match preferred {
        Some(id) => match connect_endpoint(Some(id)) {
            Ok(()) => return (None, Ok(())),
            Err(err) => Some(err),
        },
        None => None,
    };
    (preferred_error, connect_endpoint(None))
}

#[cfg(test)]
mod tests {
    use super::connect;

    #[test]
    fn windows_default_is_used_without_a_preference() {
        let result = connect(None, |id| {
            assert_eq!(id, None);
            Ok::<_, ()>(())
        });
        assert_eq!(result, (None, Ok(())));
    }

    #[test]
    fn missing_endpoint_falls_back_then_returns_to_saved_endpoint() {
        let saved = Some("sonar-gaming");
        let mut attempts = Vec::new();
        let mut available = false;
        for expected in [(Some("missing"), Ok(())), (None, Ok(()))] {
            let result = connect(saved, |id| {
                assert!(id.is_none() || id == saved);
                attempts.push(id.is_some());
                if id.is_some() && !available {
                    Err("missing")
                } else {
                    Ok(())
                }
            });
            assert_eq!(result, expected);
            available = true;
        }
        assert_eq!(attempts, [true, false, true]);
    }

    #[test]
    fn both_endpoints_can_be_unavailable() {
        let result = connect(Some("saved"), |id| Err(if id.is_some() { "missing" } else { "no default" }));
        assert_eq!(result, (Some("missing"), Err("no default")));
    }
}
