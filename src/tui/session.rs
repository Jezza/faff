//! The `Enter` toggle decision (pure). See spec §11: open / detach / retarget.

/// What pressing Enter should do, given the currently-open agent pane and the
/// selected task's agent pane (if any).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toggle {
    /// Nothing to do (selected task has no running agent).
    Nothing,
    /// Open the selected agent beside faf.
    Open(u64),
    /// Detach the currently-open agent (selection == open).
    Detach(u64),
    /// Detach the open one and open the selected one.
    Retarget { detach: u64, open: u64 },
}

/// Decide the toggle. `open` is the pane currently docked beside faf (if any);
/// `selected` is the selected task's pane (None if it has no agent).
pub fn decide(open: Option<u64>, selected: Option<u64>) -> Toggle {
    match (open, selected) {
        (_, None) => Toggle::Nothing,
        (None, Some(sel)) => Toggle::Open(sel),
        (Some(o), Some(sel)) if o == sel => Toggle::Detach(o),
        (Some(o), Some(sel)) => Toggle::Retarget {
            detach: o,
            open: sel,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_when_selected_has_no_agent() {
        assert_eq!(decide(None, None), Toggle::Nothing);
        assert_eq!(decide(Some(5), None), Toggle::Nothing);
    }

    #[test]
    fn open_when_none_open() {
        assert_eq!(decide(None, Some(12)), Toggle::Open(12));
    }

    #[test]
    fn detach_when_selected_is_open() {
        assert_eq!(decide(Some(12), Some(12)), Toggle::Detach(12));
    }

    #[test]
    fn retarget_when_different() {
        assert_eq!(
            decide(Some(12), Some(13)),
            Toggle::Retarget {
                detach: 12,
                open: 13
            }
        );
    }
}
