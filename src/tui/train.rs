//! The merge train: the set of finished agent revisions the user has marked to integrate
//! (`a`). faff drains it into the main workspace one revision at a time — waiting for each
//! agent to rebase onto the growing tip, describing the revision if it lacks a description,
//! then taking it over with `jj new` and retiring the agent. A member that needs the user's
//! attention or ends up conflicted can't be cleanly accepted, so it is dropped from the set
//! (left as an ordinary task) while the rest carry on.
//!
//! This module holds the *state* and its pure membership operations. The state machine that
//! advances it each refresh tick lives in `tui::mod` (`App::tick_merge_train`), because
//! stepping it shells out to jj/wezterm and mutates the app.

use crate::domain::TaskId;
use std::time::{Duration, Instant};

/// After faff injects a prompt (describe / rebase) into a member's agent it waits at least
/// this long before touching that member again. It's a debounce: the agent takes a beat to
/// pick the prompt up (its `UserPromptSubmit` hook flips it to `working`), and until then the
/// store still reads the pre-injection `idle` — without the cooldown a fast tick could re-send
/// the same prompt into that window.
pub const INJECT_COOLDOWN: Duration = Duration::from_secs(3);

/// A prompt faff injected into a member's agent — recorded so the panel can label what a
/// *working* member is actually doing (vs. an agent still on its original task).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Injected {
    Describe,
    Rebase,
}

/// What a member is currently doing, computed fresh each tick for the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// The agent is busy on its own work — accepted but not yet driven by the train.
    Working,
    /// Off the tip — waiting to be asked to rebase, or the agent is rebasing now.
    Rebasing,
    /// On the tip but undescribed — waiting for / running `jj describe`.
    Describing,
    /// On the tip, idle, described — eligible to be taken over.
    Ready,
    /// Being taken over this tick (`jj new`).
    Merging,
    /// The agent's revision has conflicts it's still resolving.
    Resolving,
}

impl Stage {
    /// A short glyph + word for the panel.
    pub fn label(self) -> &'static str {
        match self {
            Stage::Working => "⋯ working",
            Stage::Rebasing => "⚙ rebasing",
            Stage::Describing => "✎ describing",
            Stage::Ready => "· ready",
            Stage::Merging => "⤵ merging",
            Stage::Resolving => "✗ resolving conflict",
        }
    }
}

/// One accepted revision in the merge train.
#[derive(Debug, Clone)]
pub struct Member {
    pub task: TaskId,
    /// Monotonic accept order — the ready-first tie-break and the panel's display order.
    pub seq: u64,
    /// Set after an injection; the member is left alone until it elapses (see
    /// [`INJECT_COOLDOWN`]).
    pub cooldown_until: Option<Instant>,
    /// The last prompt faff injected — labels a working member accurately (display only).
    pub last_inject: Option<Injected>,
    /// Last stage computed for the panel (display only).
    pub stage: Stage,
}

impl Member {
    /// Whether the injection debounce is still active at `now`.
    pub fn cooling(&self, now: Instant) -> bool {
        matches!(self.cooldown_until, Some(t) if now < t)
    }
}

/// The merge train owned by the TUI app.
#[derive(Debug, Default)]
pub struct Train {
    pub members: Vec<Member>,
    /// A train-level note for the panel (e.g. "paused — your @ has content").
    pub note: String,
    next_seq: u64,
}

impl Train {
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    pub fn len(&self) -> usize {
        self.members.len()
    }

    pub fn contains(&self, id: TaskId) -> bool {
        self.members.iter().any(|m| m.task == id)
    }

    /// Add a task to the set, preserving accept order. No-op if already present.
    pub fn add(&mut self, id: TaskId) {
        if self.contains(id) {
            return;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.members.push(Member {
            task: id,
            seq,
            cooldown_until: None,
            last_inject: None,
            stage: Stage::Working,
        });
    }

    /// Remove a task from the set. Returns whether it was present.
    pub fn remove(&mut self, id: TaskId) -> bool {
        let before = self.members.len();
        self.members.retain(|m| m.task != id);
        self.members.len() != before
    }

    pub fn clear(&mut self) {
        self.members.clear();
        self.note.clear();
    }

    pub fn member_mut(&mut self, id: TaskId) -> Option<&mut Member> {
        self.members.iter_mut().find(|m| m.task == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: i64) -> TaskId {
        TaskId(n)
    }

    #[test]
    fn add_is_idempotent_and_preserves_accept_order() {
        let mut t = Train::default();
        t.add(id(7));
        t.add(id(3));
        t.add(id(7)); // duplicate ignored
        assert_eq!(t.len(), 2);
        let order: Vec<i64> = t.members.iter().map(|m| m.task.0).collect();
        assert_eq!(order, vec![7, 3], "insertion order, not sorted");
        // seq is monotonic and keeps the accept order even after the duplicate.
        assert!(t.members[0].seq < t.members[1].seq);
    }

    #[test]
    fn remove_reports_presence_and_seq_keeps_growing() {
        let mut t = Train::default();
        t.add(id(1));
        t.add(id(2));
        assert!(t.remove(id(1)));
        assert!(!t.remove(id(1)), "already gone");
        // A re-add gets a fresh, higher seq (it goes to the back of the order).
        t.add(id(1));
        assert!(t.contains(id(1)));
        assert!(
            t.members.iter().find(|m| m.task == id(1)).unwrap().seq
                > t.members.iter().find(|m| m.task == id(2)).unwrap().seq,
            "re-added task queues behind the one that stayed"
        );
    }

    #[test]
    fn clear_empties_and_drops_note() {
        let mut t = Train::default();
        t.add(id(1));
        t.note = "paused".into();
        t.clear();
        assert!(t.is_empty());
        assert!(t.note.is_empty());
    }

    #[test]
    fn cooling_respects_the_deadline() {
        let now = Instant::now();
        let mut m = Member {
            task: id(1),
            seq: 0,
            cooldown_until: Some(now + INJECT_COOLDOWN),
            last_inject: None,
            stage: Stage::Rebasing,
        };
        assert!(m.cooling(now), "still within the cooldown window");
        assert!(
            !m.cooling(now + INJECT_COOLDOWN + Duration::from_millis(1)),
            "past the deadline"
        );
        m.cooldown_until = None;
        assert!(!m.cooling(now), "no cooldown set");
    }
}
