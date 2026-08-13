//! Applying hook events to the store. See spec §7. `apply_event` performs the status
//! transitions and appends to the activity feed.

use crate::domain::TaskStatus;
use crate::events::Event;
use crate::store::Store;
use anyhow::Result;

/// Apply a hook event to the store: status transitions + activity feed.
pub fn apply_event(store: &Store, ev: &Event) -> Result<()> {
    match ev {
        Event::Prompt { task, text } => {
            // First user prompt of a task created empty (via `n`) becomes its prompt,
            // which the TUI seeds the jj description from. Only the first one is captured.
            if let Ok(t) = store.get_task(*task)
                && t.prompt.trim().is_empty()
                && !text.trim().is_empty()
            {
                store.set_prompt(*task, text)?;
            }
            store.update_status(*task, TaskStatus::Working)
        }
        Event::Working { task } => store.update_status(*task, TaskStatus::Working),
        Event::NeedsInput { task } => {
            // A Notification means "blocked, needs you" only when it interrupts *active*
            // work — a permission prompt mid-turn. Claude Code also fires a Notification
            // ~60s after the prompt goes idle ("Claude is waiting for your input"), which
            // just means the agent is done. Tell them apart by state, not by message text:
            // only flip to NeedsInput from Working. From Idle (or anything else) the
            // notification is the idle-waiting kind and is ignored, so a finished agent
            // isn't stuck showing 🔔 — and isn't wrongly dropped from the merge train.
            if let Ok(t) = store.get_task(*task)
                && t.status == TaskStatus::Working
            {
                store.update_status(*task, TaskStatus::NeedsInput)?;
            }
            Ok(())
        }
        Event::Idle { task } => store.update_status(*task, TaskStatus::Idle),
        Event::Activity {
            task,
            kind,
            tool,
            detail,
        } => {
            // Tool activity is proof the agent is working — clear a stale NeedsInput/
            // Idle (e.g. a Notification fired, then the agent resumed). A removed task
            // has no row, so get_task fails and we simply skip it.
            if let Ok(t) = store.get_task(*task) {
                if t.status != TaskStatus::Working {
                    store.update_status(*task, TaskStatus::Working)?;
                }
                store.add_activity(*task, kind, tool.as_deref(), detail.as_deref())?;
            }
            Ok(())
        }
        Event::SessionStart { task, session_id } => store.set_session(*task, session_id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Autonomy;

    #[test]
    fn apply_event_transitions_and_activity() {
        let store = Store::open_memory().unwrap();
        let t = store.create_task("x", 0, Autonomy::AcceptEdits).unwrap();

        apply_event(&store, &Event::Working { task: t.id }).unwrap();
        assert_eq!(store.get_task(t.id).unwrap().status, TaskStatus::Working);

        apply_event(&store, &Event::NeedsInput { task: t.id }).unwrap();
        assert_eq!(store.get_task(t.id).unwrap().status, TaskStatus::NeedsInput);

        apply_event(&store, &Event::Idle { task: t.id }).unwrap();
        assert_eq!(store.get_task(t.id).unwrap().status, TaskStatus::Idle);

        apply_event(
            &store,
            &Event::SessionStart {
                task: t.id,
                session_id: "s1".into(),
            },
        )
        .unwrap();
        assert_eq!(
            store.get_task(t.id).unwrap().session_id.as_deref(),
            Some("s1")
        );

        // Put it in NeedsInput (only reachable from Working — a permission prompt mid-turn),
        // then a tool-activity event must clear that back to Working (the sticky-🔔 fix) and
        // record the activity.
        apply_event(&store, &Event::Working { task: t.id }).unwrap();
        apply_event(&store, &Event::NeedsInput { task: t.id }).unwrap();
        assert_eq!(store.get_task(t.id).unwrap().status, TaskStatus::NeedsInput);
        apply_event(
            &store,
            &Event::Activity {
                task: t.id,
                kind: "tool".into(),
                tool: Some("Edit".into()),
                detail: Some("lib.rs".into()),
            },
        )
        .unwrap();
        assert_eq!(store.get_task(t.id).unwrap().status, TaskStatus::Working);
        let acts = store.activity_for(t.id, 10).unwrap();
        assert_eq!(acts.len(), 1);
        assert_eq!(acts[0].tool.as_deref(), Some("Edit"));
    }

    #[test]
    fn notification_blocks_only_from_working_not_idle() {
        let store = Store::open_memory().unwrap();
        let t = store.create_task("x", 0, Autonomy::Inherit).unwrap();

        // A permission prompt mid-work: Working → NeedsInput.
        apply_event(&store, &Event::Working { task: t.id }).unwrap();
        apply_event(&store, &Event::NeedsInput { task: t.id }).unwrap();
        assert_eq!(store.get_task(t.id).unwrap().status, TaskStatus::NeedsInput);

        // The idle-waiting notification that fires ~60s after an agent finishes: it's
        // already Idle, so the Notification is ignored — a done agent must never be flipped
        // back to NeedsInput (that's what was wrongly ejecting it from the merge train).
        apply_event(&store, &Event::Idle { task: t.id }).unwrap();
        apply_event(&store, &Event::NeedsInput { task: t.id }).unwrap();
        assert_eq!(
            store.get_task(t.id).unwrap().status,
            TaskStatus::Idle,
            "an idle agent's notification is the idle-waiting notice, not a block"
        );
    }

    #[test]
    fn first_prompt_is_captured_then_frozen() {
        let store = Store::open_memory().unwrap();
        let t = store.create_task("", 0, Autonomy::Inherit).unwrap(); // created empty via `n`
        apply_event(
            &store,
            &Event::Prompt {
                task: t.id,
                text: "migrate cell_serde to codec".into(),
            },
        )
        .unwrap();
        let got = store.get_task(t.id).unwrap();
        assert_eq!(got.prompt, "migrate cell_serde to codec");
        assert_eq!(got.status, TaskStatus::Working);

        // A later prompt must NOT overwrite the captured task prompt.
        apply_event(
            &store,
            &Event::Prompt {
                task: t.id,
                text: "also do something else".into(),
            },
        )
        .unwrap();
        assert_eq!(
            store.get_task(t.id).unwrap().prompt,
            "migrate cell_serde to codec"
        );
    }
}
