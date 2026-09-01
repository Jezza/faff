//! The `Enter` toggle decision (pure). See spec §11: open / detach / retarget.

/// What pressing Enter should do, given the currently-open agent pane and the
/// selected task's agent pane (if any).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toggle {
    /// Nothing to do (selected task has no running agent).
    Nothing,
    /// Open the selected agent beside faff.
    Open(u64),
    /// Detach the currently-open agent (selection == open).
    Detach(u64),
    /// Detach the open one and open the selected one.
    Retarget { detach: u64, open: u64 },
    /// Recreate a lost instance: the task has no pane but its workspace survives.
    Revive,
}

/// Whether faff can put an agent back into this task: the pane is gone but the jj
/// workspace it was working in is still there. Says nothing about *what* comes back —
/// see `resumable` for whether there is a conversation to restore or only a blank agent.
pub fn revivable(pane_id: Option<u64>, has_workspace: bool) -> bool {
    pane_id.is_none() && has_workspace
}

/// Whether a task is a *lost instance* rather than a finished one: no live pane, but a
/// conversation on disk that `claude --resume` can bring back. `transcript` comes from
/// `workspace::session_exists`.
pub fn resumable(pane_id: Option<u64>, session_id: Option<&str>, transcript: bool) -> bool {
    pane_id.is_none() && session_id.is_some() && transcript
}

/// Decide the toggle. `open` is the pane currently docked beside faff (if any);
/// `selected` is the selected task's pane (None if it has no agent); `revivable` says
/// whether a paneless selection can have an agent put back into it (see `revivable`).
pub fn decide(open: Option<u64>, selected: Option<u64>, revivable: bool) -> Toggle {
    match (open, selected) {
        (_, None) if revivable => Toggle::Revive,
        (_, None) => Toggle::Nothing,
        (None, Some(sel)) => Toggle::Open(sel),
        (Some(o), Some(sel)) if o == sel => Toggle::Detach(o),
        (Some(o), Some(sel)) => Toggle::Retarget {
            detach: o,
            open: sel,
        },
    }
}

/// Whether a pane launch starts a new conversation or rehydrates an existing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Launch {
    /// No id on the row (a task from a database written before faff minted them):
    /// launch as faff always did and let the SessionStart hook record claude's own id.
    Bare,
    /// No transcript yet: create the conversation at faff's pre-allocated id.
    Fresh,
    /// A transcript exists: recreate the lost instance from it.
    Resume,
}

/// Build the `claude` argv for a task's pane.
///
/// The two modes are not interchangeable: `--session-id` is *create-only* (claude exits
/// with "Session ID ... is already in use" if the conversation exists), and `--resume`
/// needs a transcript to rehydrate. `Resume` also drops the prompt — the conversation
/// already contains it, so re-passing it would land as a fresh user message.
pub fn claude_argv(
    session_id: &str,
    launch: Launch,
    prompt: &str,
    permission_mode: Option<&str>,
) -> Vec<String> {
    let mut argv = vec!["claude".to_string()];
    if let Some(mode) = permission_mode {
        argv.push("--permission-mode".to_string());
        argv.push(mode.to_string());
    }
    match launch {
        Launch::Bare => {
            if !prompt.is_empty() {
                argv.push(prompt.to_string());
            }
        }
        Launch::Fresh => {
            argv.push("--session-id".to_string());
            argv.push(session_id.to_string());
            if !prompt.is_empty() {
                argv.push(prompt.to_string());
            }
        }
        Launch::Resume => {
            argv.push("--resume".to_string());
            argv.push(session_id.to_string());
        }
    }
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_when_selected_has_no_agent() {
        assert_eq!(decide(None, None, false), Toggle::Nothing);
        assert_eq!(decide(Some(5), None, false), Toggle::Nothing);
    }

    #[test]
    fn open_when_none_open() {
        assert_eq!(decide(None, Some(12), false), Toggle::Open(12));
    }

    #[test]
    fn detach_when_selected_is_open() {
        assert_eq!(decide(Some(12), Some(12), false), Toggle::Detach(12));
    }

    #[test]
    fn retarget_when_different() {
        assert_eq!(
            decide(Some(12), Some(13), false),
            Toggle::Retarget {
                detach: 12,
                open: 13
            }
        );
    }

    // ---- launch argv ----

    const SID: &str = "95121771-ebba-4005-a1ea-b48b58f1116f";

    #[test]
    fn fresh_launch_pins_the_session_id() {
        // faff mints the id before the process exists, so a lost pane is recoverable
        // even if the agent died before its SessionStart hook ever fired.
        assert_eq!(
            claude_argv(SID, Launch::Fresh, "", None),
            vec!["claude", "--session-id", SID]
        );
    }

    #[test]
    fn fresh_launch_passes_a_non_empty_prompt_last() {
        assert_eq!(
            claude_argv(SID, Launch::Fresh, "do the thing", None),
            vec!["claude", "--session-id", SID, "do the thing"]
        );
    }

    #[test]
    fn resume_launch_rehydrates_the_conversation() {
        assert_eq!(
            claude_argv(SID, Launch::Resume, "", None),
            vec!["claude", "--resume", SID]
        );
    }

    #[test]
    fn resume_never_resends_the_prompt() {
        // `--session-id` is create-only, and re-passing the original prompt on resume
        // would land it as a brand-new user message in the restored conversation.
        assert_eq!(
            claude_argv(SID, Launch::Resume, "do the thing", None),
            vec!["claude", "--resume", SID]
        );
    }

    #[test]
    fn permission_mode_precedes_the_session_flag_in_both_modes() {
        assert_eq!(
            claude_argv(SID, Launch::Fresh, "", Some("acceptEdits")),
            vec![
                "claude",
                "--permission-mode",
                "acceptEdits",
                "--session-id",
                SID
            ]
        );
        assert_eq!(
            claude_argv(SID, Launch::Resume, "", Some("bypassPermissions")),
            vec![
                "claude",
                "--permission-mode",
                "bypassPermissions",
                "--resume",
                SID
            ]
        );
    }

    // ---- reviving a lost instance ----

    #[test]
    fn revive_when_the_pane_is_gone_but_the_workspace_is_not() {
        // The task kept its jj workspace; only the pane died.
        assert_eq!(decide(None, None, true), Toggle::Revive);
        assert_eq!(decide(Some(9), None, true), Toggle::Revive);
    }

    #[test]
    fn a_live_agent_is_never_revivable() {
        // Enter on a running agent still docks/detaches it; resumability is irrelevant.
        assert_eq!(decide(None, Some(12), true), Toggle::Open(12));
        assert_eq!(decide(Some(12), Some(12), true), Toggle::Detach(12));
    }

    #[test]
    fn resumable_needs_a_dead_pane_an_id_and_a_transcript() {
        let sid = Some("95121771-ebba-4005-a1ea-b48b58f1116f");
        assert!(resumable(None, sid, true));
        // A live pane is not lost, however resumable the conversation looks.
        assert!(!resumable(Some(42), sid, true));
        // No transcript: the agent never got going, so this is a fresh spawn, not a resume.
        assert!(!resumable(None, sid, false));
        // No id at all (a row from before faff minted them).
        assert!(!resumable(None, None, true));
    }

    #[test]
    fn a_row_with_no_session_id_launches_bare() {
        // Tasks from a database written before faff minted ids have nothing to pin, so
        // they launch exactly as faff always launched them and the SessionStart hook
        // records whatever id claude picks — after which they become resumable.
        assert_eq!(
            claude_argv("", Launch::Bare, "do the thing", None),
            vec!["claude", "do the thing"]
        );
        assert_eq!(
            claude_argv("", Launch::Bare, "", Some("acceptEdits")),
            vec!["claude", "--permission-mode", "acceptEdits"]
        );
    }

    #[test]
    fn revivable_needs_a_dead_pane_and_a_workspace() {
        assert!(revivable(None, true));
        // A live agent is not lost.
        assert!(!revivable(Some(42), true));
        // No workspace: there is nowhere to launch an agent into.
        assert!(!revivable(None, false));
    }

    #[test]
    fn a_task_that_died_before_its_first_prompt_is_revivable_but_not_resumable() {
        // This is the normal shape of an `n` task nobody has typed into yet: claude
        // writes no transcript until the first message, so there is nothing to resume —
        // but the workspace is intact and deserves a fresh agent.
        assert!(revivable(None, true));
        assert!(!resumable(
            None,
            Some("95121771-ebba-4005-a1ea-b48b58f1116f"),
            false
        ));
    }
}
