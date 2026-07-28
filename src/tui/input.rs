//! Keybinding → Action mapping (pure). See spec §11 footer.

use ratatui::crossterm::event::{KeyCode, KeyEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Quit,
    Up,
    Down,
    NewTask,
    /// Enter: open / detach / retarget the selected task's session (toggle).
    ToggleSession,
    /// Remove the task: kill its pane, tear down its workspace, drop it from faf.
    Remove,
    /// `s`: swap HEAD's `@` with the selected agent's revision (trade checkouts).
    Swap,
    /// `S`: snapshot the selected agent's workspace (fold its edits into its `@`).
    Snapshot,
    None,
}

/// Map a key event to an Action in the normal (non-modal) view.
pub fn map_key(key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('q') => Action::Quit,
        KeyCode::Up | KeyCode::Char('k') => Action::Up,
        KeyCode::Down | KeyCode::Char('j') => Action::Down,
        KeyCode::Char('n') => Action::NewTask,
        KeyCode::Enter => Action::ToggleSession,
        KeyCode::Char('x') => Action::Remove,
        KeyCode::Char('s') => Action::Swap,
        KeyCode::Char('S') => Action::Snapshot,
        _ => Action::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyEventKind, KeyModifiers};

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: ratatui::crossterm::event::KeyEventState::NONE,
        }
    }

    #[test]
    fn maps_core_keys() {
        assert_eq!(map_key(k(KeyCode::Char('q'))), Action::Quit);
        assert_eq!(map_key(k(KeyCode::Up)), Action::Up);
        assert_eq!(map_key(k(KeyCode::Char('k'))), Action::Up);
        assert_eq!(map_key(k(KeyCode::Down)), Action::Down);
        assert_eq!(map_key(k(KeyCode::Char('j'))), Action::Down);
        assert_eq!(map_key(k(KeyCode::Char('n'))), Action::NewTask);
        assert_eq!(map_key(k(KeyCode::Enter)), Action::ToggleSession);
        assert_eq!(map_key(k(KeyCode::Char('x'))), Action::Remove);
        assert_eq!(map_key(k(KeyCode::Char('s'))), Action::Swap);
        assert_eq!(map_key(k(KeyCode::Char('S'))), Action::Snapshot);
        assert_eq!(map_key(k(KeyCode::Char('z'))), Action::None);
    }
}
