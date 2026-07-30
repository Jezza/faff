//! Keybinding → Action mapping (pure). See spec §11 footer.

use ratatui::crossterm::event::{KeyCode, KeyEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Quit,
    Up,
    Down,
    NewTask,
    /// `N` (Shift+n): hand off your current work — spawn an agent that takes over your
    /// current revision (continues your exact commit) and retreat your own `@` to the
    /// fork point from before your changes. See `workspace::handoff`.
    Handoff,
    /// Enter: open / detach / retarget the selected task's session (toggle).
    ToggleSession,
    /// `x`: remove the task — kill its pane, tear down its workspace, drop it from faf.
    /// Real work on the revision is preserved (only an all-empty branch is abandoned).
    Remove,
    /// `X` (Shift+x): remove the task *and* abandon its revision, discarding the work.
    RemoveDiscard,
    /// `s`: swap HEAD's `@` with the selected agent's revision (trade checkouts).
    Swap,
    /// `S`: snapshot the selected agent's workspace (fold its edits into its `@`).
    Snapshot,
    /// `r`: refresh the selected agent onto the latest fork point, freezing your WIP
    /// first (like `create`). faff injects a rebase prompt; the agent does the rebase.
    Rebase,
    /// `R`: like `Rebase`, but base the agent on your parent line (read-only, WIP excluded).
    RebaseParent,
    /// `d`: describe — tell the agent to set a short 4-7 word jj description summarising
    /// the end result of its current revision. faff injects the prompt; the agent runs
    /// `jj describe` itself.
    Describe,
    None,
}

/// Map a key event to an Action in the normal (non-modal) view.
pub fn map_key(key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('q') => Action::Quit,
        KeyCode::Up | KeyCode::Char('k') => Action::Up,
        KeyCode::Down | KeyCode::Char('j') => Action::Down,
        KeyCode::Char('n') => Action::NewTask,
        KeyCode::Char('N') => Action::Handoff,
        KeyCode::Enter => Action::ToggleSession,
        KeyCode::Char('x') => Action::Remove,
        KeyCode::Char('X') => Action::RemoveDiscard,
        KeyCode::Char('s') => Action::Swap,
        KeyCode::Char('S') => Action::Snapshot,
        KeyCode::Char('r') => Action::Rebase,
        KeyCode::Char('R') => Action::RebaseParent,
        KeyCode::Char('d') => Action::Describe,
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
        assert_eq!(map_key(k(KeyCode::Char('N'))), Action::Handoff);
        assert_eq!(map_key(k(KeyCode::Enter)), Action::ToggleSession);
        assert_eq!(map_key(k(KeyCode::Char('x'))), Action::Remove);
        assert_eq!(map_key(k(KeyCode::Char('X'))), Action::RemoveDiscard);
        assert_eq!(map_key(k(KeyCode::Char('s'))), Action::Swap);
        assert_eq!(map_key(k(KeyCode::Char('S'))), Action::Snapshot);
        assert_eq!(map_key(k(KeyCode::Char('r'))), Action::Rebase);
        assert_eq!(map_key(k(KeyCode::Char('R'))), Action::RebaseParent);
        assert_eq!(map_key(k(KeyCode::Char('d'))), Action::Describe);
        assert_eq!(map_key(k(KeyCode::Char('z'))), Action::None);
    }
}
