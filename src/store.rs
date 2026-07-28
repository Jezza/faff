//! SQLite persistence. One database per repo. See spec §4.
//!
//! The TUI process owns a `Store`; short-lived `faf report-event` processes open
//! their own connection. WAL mode + a busy timeout keep those from colliding.

use crate::domain::{Autonomy, Task, TaskId, TaskStatus};
use anyhow::{Context, Result, bail};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::path::{Path, PathBuf};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS tasks (
  id            INTEGER PRIMARY KEY,
  prompt        TEXT NOT NULL,
  title         TEXT,
  status        TEXT NOT NULL,
  priority      INTEGER NOT NULL DEFAULT 0,
  autonomy      TEXT NOT NULL DEFAULT 'inherit',
  created_at    INTEGER NOT NULL,
  started_at    INTEGER,
  finished_at   INTEGER,
  archived_at   INTEGER,
  fork_point    TEXT,
  ws_name       TEXT,
  ws_path       TEXT,
  ws_change_id  TEXT,
  pane_id       INTEGER,
  session_id    TEXT
);
CREATE TABLE IF NOT EXISTS activity (
  id       INTEGER PRIMARY KEY,
  task_id  INTEGER NOT NULL REFERENCES tasks(id),
  ts       INTEGER NOT NULL,
  kind     TEXT NOT NULL,
  tool     TEXT,
  detail   TEXT
);
CREATE INDEX IF NOT EXISTS idx_activity_task ON activity(task_id, id);
CREATE TABLE IF NOT EXISTS config (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
"#;

/// One activity-feed entry (spec §9: from PostToolUse / lifecycle hooks).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activity {
    pub id: i64,
    pub task_id: TaskId,
    pub ts: i64,
    pub kind: String,
    pub tool: Option<String>,
    pub detail: Option<String>,
}

pub struct Store {
    conn: Connection,
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

impl Store {
    /// Open (creating if needed) the database at `path`, applying the schema.
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating data dir {}", parent.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::init(conn)
    }

    /// In-memory database (tests).
    pub fn open_memory() -> Result<Store> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Store> {
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA).context("applying schema")?;
        Ok(Store { conn })
    }

    /// Insert a new task (awaiting its first prompt) and return it fully populated.
    pub fn create_task(&self, prompt: &str, priority: i64, autonomy: Autonomy) -> Result<Task> {
        let created = now_ms();
        self.conn.execute(
            "INSERT INTO tasks (prompt, status, priority, autonomy, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                prompt,
                TaskStatus::NeedsInput.as_str(),
                priority,
                autonomy.as_str(),
                created
            ],
        )?;
        let id = TaskId(self.conn.last_insert_rowid());
        self.get_task(id)
    }

    pub fn get_task(&self, id: TaskId) -> Result<Task> {
        self.try_get_task(id)?
            .with_context(|| format!("no task {id}"))
    }

    /// Delete a task and its activity (used to roll back a failed create).
    pub fn delete_task(&self, id: TaskId) -> Result<()> {
        self.conn
            .execute("DELETE FROM activity WHERE task_id = ?1", params![id.0])?;
        self.conn
            .execute("DELETE FROM tasks WHERE id = ?1", params![id.0])?;
        Ok(())
    }

    pub fn try_get_task(&self, id: TaskId) -> Result<Option<Task>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {COLS} FROM tasks WHERE id = ?1"),
                params![id.0],
                row_to_task,
            )
            .optional()?)
    }

    pub fn list_tasks(&self) -> Result<Vec<Task>> {
        self.query(&format!("SELECT {COLS} FROM tasks ORDER BY id"), params![])
    }

    fn query(&self, sql: &str, p: &[&dyn rusqlite::ToSql]) -> Result<Vec<Task>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(p, row_to_task)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Update status, stamping the relevant timestamp column as a side effect.
    pub fn update_status(&self, id: TaskId, status: TaskStatus) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE tasks SET status = ?2 WHERE id = ?1",
            params![id.0, status.as_str()],
        )?;
        if n == 0 {
            bail!("no task {id}");
        }
        let now = now_ms();
        match status {
            TaskStatus::Working => {
                self.conn.execute(
                    "UPDATE tasks SET started_at = ?2 WHERE id = ?1 AND started_at IS NULL",
                    params![id.0, now],
                )?;
            }
            TaskStatus::Idle => {
                self.conn.execute(
                    "UPDATE tasks SET finished_at = ?2 WHERE id = ?1",
                    params![id.0, now],
                )?;
            }
            _ => {}
        }
        Ok(())
    }

    pub fn set_workspace(
        &self,
        id: TaskId,
        name: &str,
        path: &Path,
        change_id: &str,
        fork_point: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE tasks SET ws_name = ?2, ws_path = ?3, ws_change_id = ?4, fork_point = ?5 WHERE id = ?1",
            params![id.0, name, path.to_string_lossy(), change_id, fork_point],
        )?;
        Ok(())
    }

    /// Update just the recorded agent revision (after a swap moves its workspace `@`).
    /// The graph rebuilds from jj regardless; this keeps the stored row honest.
    pub fn set_ws_change_id(&self, id: TaskId, change_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE tasks SET ws_change_id = ?2 WHERE id = ?1",
            params![id.0, change_id],
        )?;
        Ok(())
    }

    pub fn set_pane(&self, id: TaskId, pane_id: Option<u64>) -> Result<()> {
        self.conn.execute(
            "UPDATE tasks SET pane_id = ?2 WHERE id = ?1",
            params![id.0, pane_id.map(|v| v as i64)],
        )?;
        Ok(())
    }

    pub fn set_prompt(&self, id: TaskId, prompt: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE tasks SET prompt = ?2 WHERE id = ?1",
            params![id.0, prompt],
        )?;
        Ok(())
    }

    pub fn set_title(&self, id: TaskId, title: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE tasks SET title = ?2 WHERE id = ?1",
            params![id.0, title],
        )?;
        Ok(())
    }

    pub fn set_session(&self, id: TaskId, session_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE tasks SET session_id = ?2 WHERE id = ?1",
            params![id.0, session_id],
        )?;
        Ok(())
    }

    pub fn add_activity(
        &self,
        id: TaskId,
        kind: &str,
        tool: Option<&str>,
        detail: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO activity (task_id, ts, kind, tool, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id.0, now_ms(), kind, tool, detail],
        )?;
        Ok(())
    }

    /// Most recent `limit` activity rows for a task, newest last.
    pub fn activity_for(&self, id: TaskId, limit: usize) -> Result<Vec<Activity>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, ts, kind, tool, detail FROM activity
             WHERE task_id = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![id.0, limit as i64], |r| {
            Ok(Activity {
                id: r.get(0)?,
                task_id: TaskId(r.get(1)?),
                ts: r.get(2)?,
                kind: r.get(3)?,
                tool: r.get(4)?,
                detail: r.get(5)?,
            })
        })?;
        let mut v = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        v.reverse(); // newest last for display
        Ok(v)
    }

    pub fn config_get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT value FROM config WHERE key = ?1",
                params![key],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
    }

    pub fn config_set(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO config (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
}

const COLS: &str = "id, prompt, title, status, priority, autonomy, created_at, started_at, \
                    finished_at, archived_at, fork_point, ws_name, ws_path, ws_change_id, \
                    pane_id, session_id";

fn row_to_task(r: &Row) -> rusqlite::Result<Task> {
    let status_s: String = r.get(3)?;
    let autonomy_s: String = r.get(5)?;
    let ws_path: Option<String> = r.get(12)?;
    let pane: Option<i64> = r.get(14)?;
    Ok(Task {
        id: TaskId(r.get(0)?),
        prompt: r.get(1)?,
        title: r.get(2)?,
        status: TaskStatus::from_str(&status_s).unwrap_or(TaskStatus::Idle),
        priority: r.get(4)?,
        autonomy: Autonomy::from_str(&autonomy_s).unwrap_or(Autonomy::AcceptEdits),
        created_at: r.get(6)?,
        started_at: r.get(7)?,
        finished_at: r.get(8)?,
        archived_at: r.get(9)?,
        fork_point: r.get(10)?,
        ws_name: r.get(11)?,
        ws_path: ws_path.map(PathBuf::from),
        ws_change_id: r.get(13)?,
        pane_id: pane.map(|v| v as u64),
        session_id: r.get(15)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn store() -> Store {
        Store::open_memory().unwrap()
    }

    #[test]
    fn create_and_get_round_trips() {
        let s = store();
        let t = s.create_task("do the thing", 5, Autonomy::Bypass).unwrap();
        assert_eq!(t.id, TaskId(1));
        assert_eq!(t.prompt, "do the thing");
        assert_eq!(t.priority, 5);
        assert_eq!(t.autonomy, Autonomy::Bypass);
        assert_eq!(t.status, TaskStatus::NeedsInput);
        assert!(t.title.is_none());
        assert!(t.created_at > 0);

        let got = s.get_task(t.id).unwrap();
        assert_eq!(got, t);
    }

    #[test]
    fn delete_task_removes_row_and_activity() {
        let s = store();
        let t = s.create_task("x", 0, Autonomy::AcceptEdits).unwrap();
        s.add_activity(t.id, "tool", None, None).unwrap();
        s.delete_task(t.id).unwrap();
        assert_eq!(s.try_get_task(t.id).unwrap(), None);
        assert!(s.activity_for(t.id, 10).unwrap().is_empty());
        assert!(s.list_tasks().unwrap().is_empty());
    }

    #[test]
    fn missing_task_errors_but_try_get_is_none() {
        let s = store();
        assert!(s.get_task(TaskId(99)).is_err());
        assert_eq!(s.try_get_task(TaskId(99)).unwrap(), None);
    }

    #[test]
    fn status_update_stamps_timestamps() {
        let s = store();
        let t = s.create_task("x", 0, Autonomy::AcceptEdits).unwrap();
        s.update_status(t.id, TaskStatus::Working).unwrap();
        let w = s.get_task(t.id).unwrap();
        assert_eq!(w.status, TaskStatus::Working);
        assert!(w.started_at.is_some());
        assert!(w.finished_at.is_none());

        s.update_status(t.id, TaskStatus::Idle).unwrap();
        let i = s.get_task(t.id).unwrap();
        assert!(i.finished_at.is_some());
        let started = i.started_at;

        // Re-entering Working must not overwrite the original started_at.
        s.update_status(t.id, TaskStatus::Working).unwrap();
        assert_eq!(s.get_task(t.id).unwrap().started_at, started);
    }

    #[test]
    fn workspace_pane_title_session_setters() {
        let s = store();
        let t = s.create_task("x", 0, Autonomy::AcceptEdits).unwrap();
        s.set_workspace(t.id, "faf-task-1", Path::new("/ws/0001-x"), "abcd", "fork1")
            .unwrap();
        s.set_pane(t.id, Some(42)).unwrap();
        s.set_title(t.id, "do X").unwrap();
        s.set_session(t.id, "sess-9").unwrap();
        let g = s.get_task(t.id).unwrap();
        assert_eq!(g.ws_name.as_deref(), Some("faf-task-1"));
        assert_eq!(g.ws_path, Some(PathBuf::from("/ws/0001-x")));
        assert_eq!(g.ws_change_id.as_deref(), Some("abcd"));
        assert_eq!(g.fork_point.as_deref(), Some("fork1"));
        assert_eq!(g.pane_id, Some(42));
        assert_eq!(g.title.as_deref(), Some("do X"));
        assert_eq!(g.session_id.as_deref(), Some("sess-9"));
    }

    #[test]
    fn activity_append_and_ordering_newest_last() {
        let s = store();
        let t = s.create_task("x", 0, Autonomy::AcceptEdits).unwrap();
        s.add_activity(t.id, "tool", Some("Edit"), Some("auth.rs"))
            .unwrap();
        s.add_activity(t.id, "tool", Some("Bash"), Some("cargo test"))
            .unwrap();
        let acts = s.activity_for(t.id, 10).unwrap();
        assert_eq!(acts.len(), 2);
        assert_eq!(acts[0].tool.as_deref(), Some("Edit"));
        assert_eq!(acts[1].tool.as_deref(), Some("Bash"));
        // limit keeps the most recent
        let one = s.activity_for(t.id, 1).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].tool.as_deref(), Some("Bash"));
    }

    #[test]
    fn config_get_set_upserts() {
        let s = store();
        assert_eq!(s.config_get("theme").unwrap(), None);
        s.config_set("theme", "dark").unwrap();
        assert_eq!(s.config_get("theme").unwrap().as_deref(), Some("dark"));
        s.config_set("theme", "light").unwrap();
        assert_eq!(s.config_get("theme").unwrap().as_deref(), Some("light"));
    }

    #[test]
    fn schema_is_idempotent_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("faf.db");
        {
            let s = Store::open(&path).unwrap();
            s.create_task("persist", 1, Autonomy::AcceptEdits).unwrap();
        }
        let s2 = Store::open(&path).unwrap();
        assert_eq!(s2.list_tasks().unwrap().len(), 1);
    }
}
