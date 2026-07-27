//! Command-line surface. The default action is the TUI; `report-event` is the
//! internal subcommand invoked by auto-injected Claude Code hooks (spec §9).

use crate::domain::TaskId;
use crate::events::{self, Event};
use crate::scheduler;
use crate::store::Store;
use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(
    name = "faff",
    version,
    about = "jj-native TUI for parallel Claude Code agents"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run the dashboard TUI (default).
    Tui {
        /// Path to the jj repo (master). Defaults to the current directory.
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Internal: report a Claude Code hook event (invoked by injected hooks).
    ReportEvent {
        #[arg(long)]
        task: i64,
        #[arg(long)]
        event: String,
        #[arg(long)]
        socket: PathBuf,
        /// The repo's faf.db — report-event writes here so state survives a
        /// not-running TUI (the socket is only a wake-up nudge).
        #[arg(long)]
        db: PathBuf,
    },
}

/// Map a hook `event` name + payload to a faf `Event` (pure; unit-tested).
pub fn map_event(task: TaskId, event: &str, payload: &Value) -> Result<Event> {
    Ok(match event {
        "stop" => Event::Idle { task },
        "notification" => Event::NeedsInput { task },
        "prompt" => Event::Prompt {
            task,
            text: payload
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        },
        "session-start" => Event::SessionStart {
            task,
            session_id: payload
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        },
        "post-tool" => Event::Activity {
            task,
            kind: "tool".to_string(),
            tool: payload
                .get("tool_name")
                .and_then(Value::as_str)
                .map(str::to_string),
            detail: extract_detail(payload),
        },
        other => bail!("unknown --event {other:?}"),
    })
}

/// Best-effort human detail from a tool payload (file path or command).
fn extract_detail(payload: &Value) -> Option<String> {
    let ti = payload.get("tool_input")?;
    if let Some(f) = ti.get("file_path").and_then(Value::as_str) {
        return Some(f.to_string());
    }
    if let Some(c) = ti.get("command").and_then(Value::as_str) {
        return Some(c.chars().take(60).collect());
    }
    None
}

/// Handle the `report-event` subcommand: read the hook payload from stdin, map it,
/// **persist it to the store** (so state survives whether or not the TUI is running),
/// then nudge the running TUI over the socket to refresh promptly. WAL + a busy
/// timeout let this short-lived process write `faf.db` alongside the TUI. Errors are
/// swallowed so a hook never fails the agent.
pub fn report_event(task: i64, event: &str, socket: &Path, db: &Path) -> Result<()> {
    let task = TaskId(task);
    let mut buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut buf);
    let payload: Value = serde_json::from_str(&buf).unwrap_or(Value::Null);
    let ev = map_event(task, event, &payload)?;

    // Durable write first, then the best-effort nudge.
    if let Ok(store) = Store::open(db) {
        let _ = scheduler::apply_event(&store, &ev);
    }
    let _ = events::send(socket, &ev);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_lifecycle_events() {
        let t = TaskId(3);
        assert_eq!(
            map_event(t, "stop", &Value::Null).unwrap(),
            Event::Idle { task: t }
        );
        assert_eq!(
            map_event(t, "notification", &Value::Null).unwrap(),
            Event::NeedsInput { task: t }
        );
        assert_eq!(
            map_event(t, "prompt", &json!({"prompt": "build X"})).unwrap(),
            Event::Prompt {
                task: t,
                text: "build X".into()
            }
        );
    }

    #[test]
    fn maps_session_start_with_id() {
        let ev = map_event(TaskId(1), "session-start", &json!({"session_id": "abc123"})).unwrap();
        assert_eq!(
            ev,
            Event::SessionStart {
                task: TaskId(1),
                session_id: "abc123".into()
            }
        );
    }

    #[test]
    fn maps_post_tool_with_file_detail() {
        let ev = map_event(
            TaskId(1),
            "post-tool",
            &json!({"tool_name": "Edit", "tool_input": {"file_path": "src/auth.rs"}}),
        )
        .unwrap();
        assert_eq!(
            ev,
            Event::Activity {
                task: TaskId(1),
                kind: "tool".into(),
                tool: Some("Edit".into()),
                detail: Some("src/auth.rs".into()),
            }
        );
    }

    #[test]
    fn maps_post_tool_with_command_detail_truncated() {
        let long = "x".repeat(100);
        let ev = map_event(
            TaskId(1),
            "post-tool",
            &json!({"tool_name": "Bash", "tool_input": {"command": long}}),
        )
        .unwrap();
        if let Event::Activity { detail, .. } = ev {
            assert_eq!(detail.unwrap().chars().count(), 60);
        } else {
            panic!("expected Activity");
        }
    }

    #[test]
    fn unknown_event_errors() {
        assert!(map_event(TaskId(1), "bogus", &Value::Null).is_err());
    }
}
