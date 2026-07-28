//! Core domain types shared across faf.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Stable identifier for a task (also the SQLite rowid). Serialises as a bare integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskId(pub i64);

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Lifecycle state of a task. See spec §6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    /// Agent actively working (between UserPromptSubmit and Stop).
    Working,
    /// Agent needs attention (permission / idle waiting) — Notification hook.
    NeedsInput,
    /// Agent alive at its prompt; Stop fired. "review-ready" when diff is non-empty.
    Idle,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Working => "working",
            TaskStatus::NeedsInput => "needs_input",
            TaskStatus::Idle => "idle",
        }
    }

    #[allow(clippy::should_implement_trait)] // intentional Option-returning parser
    pub fn from_str(s: &str) -> Option<TaskStatus> {
        Some(match s {
            "working" => TaskStatus::Working,
            "needs_input" => TaskStatus::NeedsInput,
            "idle" => TaskStatus::Idle,
            _ => return None,
        })
    }
}

/// How much autonomy the agent launches with. See spec §10.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Autonomy {
    /// Inherit the user's own `claude` default (their `permissions.defaultMode`).
    /// No `--permission-mode` flag is passed. This is the default — passing a mode
    /// would *override* (and usually downgrade) the user's configured autonomy.
    #[default]
    Inherit,
    /// Force `acceptEdits`: auto-apply edits, still gate risky shell/network.
    AcceptEdits,
    /// Force `bypassPermissions`: maximum autonomy in the disposable workspace.
    Bypass,
}

impl Autonomy {
    pub fn as_str(&self) -> &'static str {
        match self {
            Autonomy::Inherit => "inherit",
            Autonomy::AcceptEdits => "accept_edits",
            Autonomy::Bypass => "bypass",
        }
    }

    #[allow(clippy::should_implement_trait)] // intentional Option-returning parser
    pub fn from_str(s: &str) -> Option<Autonomy> {
        Some(match s {
            "inherit" => Autonomy::Inherit,
            "accept_edits" => Autonomy::AcceptEdits,
            "bypass" => Autonomy::Bypass,
            _ => return None,
        })
    }

    /// The value to pass to `claude --permission-mode`, or `None` to pass no flag
    /// (inherit the user's default).
    pub fn permission_mode(&self) -> Option<&'static str> {
        match self {
            Autonomy::Inherit => None,
            Autonomy::AcceptEdits => Some("acceptEdits"),
            Autonomy::Bypass => Some("bypassPermissions"),
        }
    }
}

/// A task record (mirrors the `tasks` table). Timestamps are unix epoch millis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: TaskId,
    pub prompt: String,
    pub status: TaskStatus,
    pub priority: i64,
    pub autonomy: Autonomy,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub archived_at: Option<i64>,
    pub fork_point: Option<String>,
    pub ws_name: Option<String>,
    pub ws_path: Option<PathBuf>,
    pub ws_change_id: Option<String>,
    pub pane_id: Option<u64>,
    pub session_id: Option<String>,
}

impl Task {
    /// Display label: the first line of the prompt, else a placeholder (a task created
    /// via `n` before its first prompt). Returned untruncated — the log view clips it to
    /// the current pane width at render time. The jj change's own description, when set,
    /// is preferred over this in the graph (see `tui::model::build`).
    pub fn label(&self) -> String {
        let from_prompt = self.prompt.lines().next().unwrap_or("").trim();
        if from_prompt.is_empty() {
            "(new task — awaiting prompt)".to_string()
        } else {
            from_prompt.to_string()
        }
    }
}

/// Max display length for a task title / label. Generous enough for a 3-6 word title
/// so it isn't clipped, while still fitting a reasonably wide graph pane.
pub const LABEL_WIDTH: usize = 56;

/// First line of `s`, trimmed and truncated to `max` chars (ellipsis if cut). Prefers to
/// cut on a word boundary so a title never ends mid-word (e.g. "… bridges to J…"); falls
/// back to a hard character cut only for a single over-long word.
pub fn truncate_first_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    if line.chars().count() <= max {
        return line.to_string();
    }
    let budget = max.saturating_sub(1); // leave a column for the ellipsis
    let head: String = line.chars().take(budget).collect();
    let cut = match head.rfind(' ') {
        // Break at the last space, but only if it keeps a reasonable amount of text.
        Some(i) if i >= budget / 2 => &head[..i],
        _ => head.as_str(),
    };
    format!("{}…", cut.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_round_trips() {
        for s in [
            TaskStatus::Working,
            TaskStatus::NeedsInput,
            TaskStatus::Idle,
        ] {
            assert_eq!(TaskStatus::from_str(s.as_str()), Some(s));
        }
        assert_eq!(TaskStatus::from_str("nonsense"), None);
    }

    #[test]
    fn autonomy_round_trips_and_maps_to_permission_mode() {
        for a in [Autonomy::Inherit, Autonomy::AcceptEdits, Autonomy::Bypass] {
            assert_eq!(Autonomy::from_str(a.as_str()), Some(a));
        }
        // Inherit passes no flag; the others force an explicit mode.
        assert_eq!(Autonomy::default(), Autonomy::Inherit);
        assert_eq!(Autonomy::Inherit.permission_mode(), None);
        assert_eq!(Autonomy::AcceptEdits.permission_mode(), Some("acceptEdits"));
        assert_eq!(
            Autonomy::Bypass.permission_mode(),
            Some("bypassPermissions")
        );
    }

    #[test]
    fn truncate_first_line_handles_multiline_and_length() {
        assert_eq!(truncate_first_line("hello world", 40), "hello world");
        assert_eq!(truncate_first_line("  spaced  \nsecond", 40), "spaced");
        assert_eq!(truncate_first_line("", 40), "");
        let long = "a".repeat(50);
        let out = truncate_first_line(&long, 10);
        assert_eq!(out.chars().count(), 10); // 9 chars + ellipsis
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncate_breaks_on_word_boundary_not_mid_word() {
        // The real case: a 42-char title fits at LABEL_WIDTH (56) — no ellipsis at all.
        let title = "Convert HTTP/MQTT bridges postcard to JSON";
        assert_eq!(truncate_first_line(title, LABEL_WIDTH), title);

        // When it must cut, it cuts on a space, never mid-word.
        let cut = truncate_first_line(title, 40);
        assert!(cut.ends_with('…'));
        assert!(!cut.contains(" to J…"), "must not cut mid-word: {cut:?}");
        assert_eq!(cut, "Convert HTTP/MQTT bridges postcard to…");

        // A single over-long word (no space) still falls back to a hard cut.
        let hard = truncate_first_line("supercalifragilisticexpialidocious", 10);
        assert_eq!(hard.chars().count(), 10);
        assert!(hard.ends_with('…'));
    }
}
