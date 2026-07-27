//! Events emitted by injected Claude Code hooks (via `faf report-event`) and the
//! Unix-socket transport that carries them to the running TUI. See spec §9.
//!
//! `report-event` always writes to the durable store; the socket send is a
//! best-effort "wake up now" nudge to the TUI, so a not-running TUI is harmless.

use crate::domain::TaskId;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

/// A state/activity event for one task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Agent began a turn (UserPromptSubmit); `text` is the submitted prompt, used to
    /// capture + title a task created empty via `n`.
    Prompt { task: TaskId, text: String },
    /// Agent began working (no prompt text available).
    Working { task: TaskId },
    /// Agent needs attention (Notification).
    NeedsInput { task: TaskId },
    /// Agent finished a turn (Stop).
    Idle { task: TaskId },
    /// A tool call / activity line (PostToolUse).
    Activity {
        task: TaskId,
        kind: String,
        tool: Option<String>,
        detail: Option<String>,
    },
    /// Session started; carries the claude session id for correlation.
    SessionStart { task: TaskId, session_id: String },
}

impl Event {
    pub fn task(&self) -> TaskId {
        match self {
            Event::Prompt { task, .. }
            | Event::Working { task }
            | Event::NeedsInput { task }
            | Event::Idle { task }
            | Event::Activity { task, .. }
            | Event::SessionStart { task, .. } => *task,
        }
    }
}

/// A short, stable per-repo socket path in the runtime dir (avoids the ~108-byte
/// Unix socket path limit that the long encoded repo path would blow).
pub fn socket_path(repo: &Path) -> PathBuf {
    use std::hash::{Hash, Hasher};
    // DefaultHasher::new() uses fixed keys, so this is stable across faf processes.
    let mut h = std::collections::hash_map::DefaultHasher::new();
    repo.hash(&mut h);
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join(format!("faf-{:x}.sock", h.finish()))
}

/// Connect to the TUI's socket and send one event as a JSON line.
pub fn send(socket: &Path, ev: &Event) -> Result<()> {
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("connecting to {}", socket.display()))?;
    let mut line = serde_json::to_string(ev)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()?;
    Ok(())
}

/// Bind the socket and spawn a background thread that decodes incoming JSON-line
/// events onto the returned channel. Clears any stale socket file first.
pub fn spawn_listener(socket: &Path) -> Result<Receiver<Event>> {
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let _ = std::fs::remove_file(socket);
    let listener =
        UnixListener::bind(socket).with_context(|| format!("binding {}", socket.display()))?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            // A read timeout stops a client that connects but never sends a full
            // line (or never closes) from wedging the single-threaded accept loop.
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
            let reader = BufReader::new(stream);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Event>(&line) {
                    Ok(ev) => {
                        if tx.send(ev).is_err() {
                            return; // receiver dropped; stop listening
                        }
                    }
                    Err(_) => continue, // ignore malformed lines
                }
            }
        }
    });
    Ok(rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn event_json_round_trips() {
        let evs = vec![
            Event::Prompt {
                task: TaskId(1),
                text: "do the thing".into(),
            },
            Event::Working { task: TaskId(1) },
            Event::NeedsInput { task: TaskId(2) },
            Event::Idle { task: TaskId(3) },
            Event::Activity {
                task: TaskId(4),
                kind: "tool".into(),
                tool: Some("Edit".into()),
                detail: Some("auth.rs".into()),
            },
            Event::SessionStart {
                task: TaskId(5),
                session_id: "sess-9".into(),
            },
        ];
        for ev in evs {
            let s = serde_json::to_string(&ev).unwrap();
            let back: Event = serde_json::from_str(&s).unwrap();
            assert_eq!(ev, back);
            assert_eq!(back.task(), ev.task());
        }
    }

    #[test]
    fn task_id_serialises_as_bare_int() {
        let ev = Event::Working { task: TaskId(7) };
        let s = serde_json::to_string(&ev).unwrap();
        assert_eq!(s, r#"{"type":"working","task":7}"#);
    }

    #[test]
    fn send_and_listen_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("t.sock");
        let rx = spawn_listener(&sock).unwrap();

        send(&sock, &Event::Working { task: TaskId(1) }).unwrap();
        send(
            &sock,
            &Event::Activity {
                task: TaskId(1),
                kind: "tool".into(),
                tool: Some("Bash".into()),
                detail: Some("cargo test".into()),
            },
        )
        .unwrap();

        let first = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(first, Event::Working { task: TaskId(1) });
        let second = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(second, Event::Activity { .. }));
    }
}
