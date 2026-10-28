//! `faff logs`' engine: the log view on its own, standing in for `watch jj peek`.
//!
//! It is faff's log view, minus the Tab to the agents view. It shares faff's UI, its
//! [`Cmd`]/[`Msg`] protocol and its log-view writes ([`revops`]), and reads faff's tasks so
//! agents' rows look the same — but only ever *reads* faff's store, starts no agents and
//! creates no tasks, so it can run beside a faff (or another `faff logs`) without either
//! stepping on the other. It loads the user's own `revsets.log` (or `-r`), reloads the
//! moment jj records an operation, and snapshots the checkout it was started in when files
//! change, holding off while any jj command that might rewrite commits is running here
//! (see [`crate::watch`]).

use super::engine::{Cmd, LogModel, Msg, Outbox, View};
use super::{describe, model, revops};
use crate::domain::{DESCRIBE_PROMPT, Task, TaskId};
use crate::store::Store;
use crate::watch::{self, Aliases};
use crate::{config, jj, wezterm};
use anyhow::Result;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

/// Reload this often even with no operation seen, in case the watcher missed one.
const FALLBACK_REFRESH: Duration = Duration::from_secs(10);

/// Snapshot once files have been quiet this long, so a save storm is one operation.
const SNAPSHOT_QUIET: Duration = Duration::from_secs(1);

/// While a writing jj command runs, look again this often.
const SNAPSHOT_RETRY: Duration = Duration::from_secs(1);

/// How often faff's tasks are re-read, as faff itself does, so agents' status keeps up
/// without waiting on a jj operation.
const TASK_REFRESH: Duration = Duration::from_secs(1);

/// Floor between reloads, so the burst of events one operation makes is one reload.
const MIN_REFRESH_GAP: Duration = Duration::from_millis(150);

pub struct LogsEngine {
    /// The checkout `faff logs` runs in: what it snapshots and edits.
    root: PathBuf,
    /// The user's `revsets.log`.
    base: String,
    /// Generations of older history scrolling down has loaded (see [`Cmd::LoadMore`]).
    depth: usize,
    /// What the search found in the files, kept loaded while it stands.
    files: revops::FileSearch,
    /// Branches a search reached past `base` (see [`Cmd::PullIn`]).
    pulled: Vec<String>,
    /// Squashes made here, kept loaded even when `base` wouldn't show them.
    pinned: Vec<String>,
    notices: Outbox,
    aliases: Aliases,
    describing: HashSet<String>,
    describer: fn(&str) -> Result<String>,
    wake: Sender<Cmd>,
    view: View,
    last_refresh: Instant,
    /// A reload asked for inside [`MIN_REFRESH_GAP`] of the last one.
    refresh_due: bool,
    /// When to snapshot, once the files go quiet.
    snapshot_due: Option<Instant>,
    /// The last load error, so a broken revset is reported once, not every tick.
    last_error: Option<String>,
    /// faff's store, read-only, for its tasks; `None` when this repo has no faff.
    store: Option<Store>,
    /// What jj last said, so a task tick rebuilds the rows without asking jj again.
    revs: Vec<jj::RevInfo>,
    workspaces: Vec<jj::Workspace>,
    last_tasks: Instant,
    /// Keep the watchers alive; dropping one stops its events.
    _watchers: Vec<notify::RecommendedWatcher>,
}

impl LogsEngine {
    /// Set up for the checkout at `root`. Nothing here asks jj or watches anything yet:
    /// [`LogsEngine::start`] does, so the UI draws meanwhile.
    pub fn open(root: PathBuf, tx: Sender<Msg>, wake: Sender<Cmd>) -> LogsEngine {
        let store = config::db_path(&root)
            .ok()
            .filter(|p| p.exists())
            .and_then(|p| Store::open_readonly(&p).ok());
        LogsEngine {
            aliases: Aliases::new(),
            root,
            base: String::new(),
            depth: 0,
            files: revops::FileSearch::default(),
            pulled: Vec::new(),
            pinned: Vec::new(),
            notices: Outbox::new(tx),
            describing: HashSet::new(),
            describer: describe::ask_claude,
            wake,
            view: View::default(),
            last_refresh: Instant::now(),
            refresh_due: false,
            snapshot_due: None,
            last_error: None,
            store,
            revs: Vec::new(),
            workspaces: Vec::new(),
            last_tasks: Instant::now(),
            _watchers: Vec::new(),
        }
    }

    /// Show the first screen as soon as jj has said what's there, then watch the checkout
    /// and its repo's operations. Watch failures are reported and leave the view polling.
    fn start(&mut self) {
        self.base = revops::base_revset(&self.root);
        self.refresh();
        self.publish();
        let ops = self.wake.clone();
        match watch::watch_ops(&self.root, move || {
            let _ = ops.send(Cmd::OpChanged);
        }) {
            Ok(w) => self._watchers.push(w),
            Err(e) => self
                .notices
                .warn(format!("not watching jj's operations: {e:#}")),
        }
        let files = self.wake.clone();
        match watch::watch_files(&self.root, move || {
            let _ = files.send(Cmd::FilesChanged);
        }) {
            Ok(w) => self._watchers.push(w),
            Err(e) => self
                .notices
                .warn(format!("not watching files, so not snapshotting: {e:#}")),
        }
        self.aliases = watch::load_aliases(&self.root);
    }

    pub fn run(mut self, rx: Receiver<Cmd>) {
        self.start();
        loop {
            match rx.recv_timeout(self.next_wake()) {
                Ok(Cmd::OpChanged) => self.refresh_due = true,
                Ok(Cmd::FilesChanged) => self.snapshot_due = Some(Instant::now() + SNAPSHOT_QUIET),
                Ok(Cmd::PullIn(revsets)) => {
                    self.pulled = revsets;
                    self.refresh();
                    self.publish();
                }
                Ok(Cmd::LoadMore) => {
                    self.load_more();
                    self.publish();
                }
                Ok(Cmd::SearchFiles(query)) => {
                    if self.files.ask(&self.root, query, &self.wake) {
                        self.refresh();
                        self.publish();
                    }
                }
                Ok(Cmd::FilesFound { found, errors }) => {
                    if self.files.answer(found, errors, &self.notices) {
                        self.refresh();
                        self.publish();
                    }
                }
                Ok(cmd @ Cmd::Described { .. }) => {
                    self.handle(cmd);
                    self.publish();
                }
                Ok(cmd) => {
                    self.handle(cmd);
                    self.publish();
                    self.notices.send(Msg::Done);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            let now = Instant::now();
            if self.snapshot_due.is_some_and(|t| now >= t) {
                self.snapshot();
            }
            let since = self.last_refresh.elapsed();
            if (self.refresh_due && since >= MIN_REFRESH_GAP) || since >= FALLBACK_REFRESH {
                self.refresh();
                self.publish();
            } else if self.last_tasks.elapsed() >= TASK_REFRESH {
                self.retask();
            }
        }
    }

    /// [`Cmd::LoadMore`]: the next page of older history.
    fn load_more(&mut self) {
        self.depth += revops::PAGE;
        self.refresh();
    }

    /// How long to wait for a command before there's something of our own to do.
    fn next_wake(&self) -> Duration {
        let since = self.last_refresh.elapsed();
        let mut wait = FALLBACK_REFRESH.saturating_sub(since);
        if self.refresh_due {
            wait = wait.min(MIN_REFRESH_GAP.saturating_sub(since));
        }
        if let Some(t) = self.snapshot_due {
            wait = wait.min(t.saturating_duration_since(Instant::now()));
        }
        if self.store.is_some() {
            wait = wait.min(TASK_REFRESH.saturating_sub(self.last_tasks.elapsed()));
        }
        wait
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Edit(rev) => {
                if revops::edit(&self.root, &self.aliases, &rev, &self.notices) {
                    self.refresh();
                }
            }
            Cmd::Squash { rev, base, name } => {
                if let Some(id) =
                    revops::squash(&self.root, &self.aliases, &rev, &base, &name, &self.notices)
                {
                    self.pinned.push(id.clone());
                    self.refresh();
                    self.view.select_rev = Some(id);
                }
            }
            Cmd::AddWorkspace { rev, take } => {
                let tasks = self.tasks();
                if let Some(id) = revops::add_workspace(
                    &self.root,
                    &self.aliases,
                    &rev,
                    take,
                    &tasks,
                    &self.notices,
                ) {
                    self.refresh();
                    self.view.select_rev = Some(id);
                }
            }
            // An agent's task and pane are faff's: faff drops the task once it sees the
            // workspace gone.
            Cmd::RemoveWorkspace { rev, discard } => {
                let tasks = self.tasks();
                if revops::remove_workspace(
                    &self.root,
                    &self.aliases,
                    &rev,
                    discard,
                    &tasks,
                    &self.notices,
                ) {
                    self.refresh();
                }
            }
            Cmd::Describe(id) => self.describe_task(id),
            Cmd::DescribeRev(rev) => self.describe_rev(rev),
            Cmd::Described { rev, result } => self.described(rev, result),
            // Creating, opening and removing tasks writes faff's store: faff's alone.
            _ => {}
        }
    }

    /// faff's tasks, as its store last recorded them.
    fn tasks(&self) -> Vec<Task> {
        self.store
            .as_ref()
            .and_then(|s| s.list_tasks().ok())
            .unwrap_or_default()
    }

    /// Re-read faff's tasks and, if any changed, rebuild the rows from what jj last said.
    fn retask(&mut self) {
        self.last_tasks = Instant::now();
        let tasks = self.tasks();
        if tasks != self.view.tasks {
            self.view.log = LogModel::build(&self.revs, &self.workspaces, &tasks);
            self.view.tasks = tasks;
            self.publish();
        }
    }

    fn refresh(&mut self) {
        self.last_refresh = Instant::now();
        self.last_tasks = Instant::now();
        self.refresh_due = false;
        let tasks = self.tasks();
        let pinned = [
            self.pinned.clone(),
            revops::Found::revisions(&self.files.found),
        ]
        .concat();
        let revset = revops::log_revset(&self.base, &self.pulled, &pinned, self.depth);
        let loaded = revops::load(&self.root, &revset, None);
        let workspaces = loaded.workspaces.unwrap_or_default();
        match loaded.revs {
            Ok(revs) => {
                self.last_error = None;
                self.view.id_display = revs
                    .iter()
                    .map(|r| {
                        (
                            r.change_id.clone(),
                            (r.id_prefix.clone(), r.id_rest.clone()),
                        )
                    })
                    .collect();
                self.view.working_copy = revs
                    .iter()
                    .find(|r| r.is_current_wc)
                    .map(|r| r.change_id.clone());
                self.view.fork_point = model::current_fork_point(&revs);
                self.view.log = LogModel::build(&revs, &workspaces, &tasks);
                self.view.more = revops::more_below(&revs);
                self.revs = revs;
                self.workspaces = workspaces;
            }
            Err(e) => {
                let e = format!("jj error: {e}");
                if self.last_error.as_ref() != Some(&e) {
                    self.notices.err(e.clone());
                    self.last_error = Some(e);
                }
            }
        }
        self.view.tasks = tasks;
        self.view.found = self.files.found.clone();
        if let Ok(b) = loaded.bookmarks {
            self.view.bookmarks = b;
        }
    }

    fn publish(&mut self) {
        let mut view = self.view.clone();
        view.describing = self.describing.clone();
        // Hand a fresh squash to the UI's selection once.
        self.view.select_rev = None;
        self.notices.send(Msg::View(Box::new(view)));
    }

    /// The running jj command that blocks a write of ours, if any.
    fn busy(&self) -> Option<String> {
        watch::busy_jj(&self.root, &self.aliases)
    }

    /// Fold the checkout's edits into its `@` — unless a jj command that might rewrite
    /// commits is running here, in which case wait for it, saying so.
    fn snapshot(&mut self) {
        if let Some(busy) = self.busy() {
            let waiting = Some(format!("snapshot deferred: {}", revops::shorten(&busy)));
            if self.view.activity != waiting {
                self.view.activity = waiting;
                self.publish();
            }
            self.snapshot_due = Some(Instant::now() + SNAPSHOT_RETRY);
            return;
        }
        self.snapshot_due = None;
        let was_waiting = self.view.activity.take().is_some();
        if let Err(e) = jj::snapshot_in(&self.root) {
            self.notices.err(format!("snapshot failed: {e}"));
        }
        // The operation it recorded (if anything changed) wakes a reload by itself.
        if was_waiting {
            self.publish();
        }
    }

    /// `d` on an agent's revision: ask that agent, as faff does. Only a prompt into its
    /// pane — nothing is written to faff's store.
    fn describe_task(&mut self, id: TaskId) {
        let Some(t) = self.view.tasks.iter().find(|t| t.id == id).cloned() else {
            return;
        };
        let Some(pane) = t.pane_id else {
            self.notices.warn("no pane to describe for this task");
            return;
        };
        match wezterm::send_text(pane, DESCRIBE_PROMPT) {
            Ok(()) => self.notices.ok(format!("sent describe to #{}", id.0)),
            Err(e) => self.notices.err(format!("describe send failed: {e}")),
        }
    }

    /// `d`: describe `rev` with a headless claude, off this thread (it can take a while).
    fn describe_rev(&mut self, rev: String) {
        let short = short(&rev);
        if self.describing.contains(&rev) {
            self.notices.warn(format!("already describing {short}"));
            return;
        }
        if let Some(busy) = self.busy() {
            self.notices.warn(format!(
                "not describing {short} while `{}` runs",
                revops::shorten(&busy)
            ));
            return;
        }
        let diff = match jj::diff_git(&self.root, &rev) {
            Ok(d) => d,
            Err(e) => {
                self.notices.err(format!("describe {short} failed: {e}"));
                return;
            }
        };
        if diff.trim().is_empty() {
            self.notices
                .warn(format!("nothing to describe — {short} has no changes"));
            return;
        }
        self.describing.insert(rev.clone());
        let (ask, wake) = (self.describer, self.wake.clone());
        std::thread::spawn(move || {
            let result = ask(&diff).map_err(|e| e.to_string());
            let _ = wake.send(Cmd::Described { rev, result });
        });
    }

    /// A headless describe finished: write it — but claude can take a while, so first make
    /// sure no jj command that could rewrite `rev` started meanwhile. If one has, try again
    /// shortly (the row keeps spinning) rather than race it.
    fn described(&mut self, rev: String, result: std::result::Result<String, String>) {
        if result.is_ok() && self.busy().is_some() {
            let wake = self.wake.clone();
            std::thread::spawn(move || {
                std::thread::sleep(SNAPSHOT_RETRY);
                let _ = wake.send(Cmd::Described { rev, result });
            });
            return;
        }
        self.describing.remove(&rev);
        let short = short(&rev);
        let written = result.and_then(|d| {
            jj::describe(&self.root, &rev, &d)
                .map(|()| d)
                .map_err(|e| e.to_string())
        });
        match written {
            Ok(d) => {
                self.notices.ok(format!("{short} → {d}"));
                self.refresh();
            }
            Err(e) => self.notices.err(format!("describe {short} failed: {e}")),
        }
    }
}

fn short(change_id: &str) -> String {
    change_id.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::mpsc;

    fn jj(dir: &std::path::Path, args: &[&str]) -> String {
        let out = Command::new("jj")
            .args(["--config", "user.name=T", "--config", "user.email=t@t"])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("jj runs");
        assert!(
            out.status.success(),
            "jj {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn engine(root: PathBuf, revset: &str) -> (LogsEngine, Receiver<Msg>) {
        let (tx, rx) = mpsc::channel();
        let (wake, _) = mpsc::channel();
        let notices = Outbox::new(tx);
        let e = LogsEngine {
            aliases: Aliases::new(),
            root,
            base: revset.to_string(),
            depth: 0,
            files: revops::FileSearch::default(),
            pulled: Vec::new(),
            pinned: Vec::new(),
            notices,
            describing: HashSet::new(),
            describer: |_| Ok("unused".into()),
            wake,
            view: View::default(),
            last_refresh: Instant::now(),
            refresh_due: false,
            snapshot_due: None,
            last_error: None,
            store: None,
            revs: Vec::new(),
            workspaces: Vec::new(),
            last_tasks: Instant::now(),
            _watchers: Vec::new(),
        };
        (e, rx)
    }

    /// trunk `main` ← a ← b (bookmark `feature`), with `@` an empty child of `main`.
    fn repo() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        jj(&root, &["git", "init"]);
        // No remote here, so name trunk outright.
        jj(
            &root,
            &[
                "config",
                "set",
                "--repo",
                "revset-aliases.'trunk()'",
                "main",
            ],
        );
        std::fs::write(root.join("f"), "base\n").unwrap();
        jj(&root, &["commit", "-m", "base"]);
        jj(&root, &["bookmark", "create", "main", "-r", "@-"]);
        std::fs::write(root.join("f"), "a\n").unwrap();
        jj(&root, &["commit", "-m", "a"]);
        std::fs::write(root.join("g"), "b\n").unwrap();
        jj(&root, &["commit", "-m", "b"]);
        jj(&root, &["bookmark", "create", "feature", "-r", "@-"]);
        jj(&root, &["new", "main"]);
        (tmp, root)
    }

    fn id(root: &std::path::Path, revset: &str) -> String {
        jj(
            root,
            &["log", "--no-graph", "-r", revset, "-T", "change_id"],
        )
    }

    #[test]
    fn squash_makes_one_revision_beside_the_branch_without_moving_at() {
        let (_tmp, root) = repo();
        let at = id(&root, "@");
        let feature = id(&root, "feature");
        let (mut e, _rx) = engine(root.clone(), "all()");
        e.handle(Cmd::Squash {
            rev: feature.clone(),
            base: format!("fork_point({feature} | trunk())"),
            name: "feature".into(),
        });
        assert_eq!(id(&root, "@"), at, "the working copy stayed put");
        let made = e.view.select_rev.clone().expect("the squash is selected");
        assert_eq!(id(&root, &format!("{made}-")), id(&root, "main"));
        let desc = jj(
            &root,
            &["log", "--no-graph", "-r", &made, "-T", "description"],
        );
        assert_eq!(desc.trim(), "squashed: feature");
        let diff = jj(
            &root,
            &["diff", "--from", "feature", "--to", &made, "--summary"],
        );
        assert!(
            diff.trim().is_empty(),
            "same files as the branch tip: {diff}"
        );
    }

    #[test]
    fn x_removes_a_workspace_keeping_its_work_and_shift_x_discards_it() {
        let (tmp, root) = repo();
        let mut heads = Vec::new();
        for name in ["kept", "dropped"] {
            let dir = tmp.path().join(name);
            let dir_s = dir.to_string_lossy();
            jj(
                &root,
                &["workspace", "add", "--name", name, "-r", "feature", &dir_s],
            );
            std::fs::write(dir.join(name), "work\n").unwrap();
            jj(&dir, &["util", "snapshot"]);
            heads.push((dir, id(&root, &format!("{name}@"))));
        }
        let (mut e, _rx) = engine(root.clone(), "all()");
        e.handle(Cmd::RemoveWorkspace {
            rev: heads[0].1.clone(),
            discard: false,
        });
        e.handle(Cmd::RemoveWorkspace {
            rev: heads[1].1.clone(),
            discard: true,
        });
        let list = jj(&root, &["workspace", "list"]);
        assert!(
            !list.contains("kept") && !list.contains("dropped"),
            "{list}"
        );
        assert!(!heads[0].0.exists() && !heads[1].0.exists());
        let live = |rev: &str| !id(&root, &format!("present({rev})")).trim().is_empty();
        assert!(live(&heads[0].1), "x keeps a revision with changes");
        assert!(!live(&heads[1].1), "X discards it");
    }

    #[test]
    fn x_refuses_the_workspace_it_runs_in() {
        let (_tmp, root) = repo();
        let at = id(&root, "@");
        let (mut e, rx) = engine(root.clone(), "all()");
        e.handle(Cmd::RemoveWorkspace {
            rev: at,
            discard: true,
        });
        assert!(root.join(".jj").exists());
        assert!(jj(&root, &["workspace", "list"]).contains("default"));
        assert!(rx.try_iter().any(|m| matches!(
            m,
            Msg::Notice(_, t) if t.contains("the workspace this runs in")
        )));
    }

    #[test]
    fn squashing_a_trunk_revision_is_refused() {
        let (_tmp, root) = repo();
        let main = id(&root, "main");
        let (mut e, rx) = engine(root.clone(), "all()");
        e.handle(Cmd::Squash {
            rev: main.clone(),
            base: format!("fork_point({main} | trunk())"),
            name: "main".into(),
        });
        assert!(e.view.select_rev.is_none());
        let notes: Vec<String> = rx
            .try_iter()
            .filter_map(|m| match m {
                Msg::Notice(_, t) => Some(t),
                _ => None,
            })
            .collect();
        assert!(
            notes.iter().any(|n| n.contains("already in trunk")),
            "{notes:?}"
        );
    }

    #[test]
    fn older_history_loads_a_page_at_a_time_until_none_is_left() {
        let (_tmp, root) = repo();
        let (mut e, _rx) = engine(root.clone(), "@");
        e.refresh();
        assert_eq!(e.view.log.nodes.len(), 1, "just @");
        assert!(e.view.more, "@'s parent is left to load");
        e.load_more();
        let descs: Vec<&str> = e
            .view
            .log
            .nodes
            .iter()
            .map(|n| n.lines[0].as_str())
            .collect();
        assert!(descs.contains(&"base"), "@'s ancestors loaded: {descs:?}");
        assert!(!descs.contains(&"b"), "but not other branches: {descs:?}");
        assert!(!e.view.more, "down to the root: {descs:?}");
    }

    /// Search the files for `query` as the engine does, waiting for the answer.
    fn search_files(e: &mut LogsEngine, query: &str) {
        let (tx, rx) = mpsc::channel();
        if !e.files.ask(&e.root.clone(), query.into(), &tx)
            && let Ok(Cmd::FilesFound { found, errors }) = rx.recv_timeout(Duration::from_secs(60))
        {
            e.files.answer(found, errors, &e.notices);
        }
        e.refresh();
    }

    #[test]
    fn a_file_search_finds_changed_paths_and_lines_and_loads_them() {
        let (_tmp, root) = repo();
        let (mut e, _rx) = engine(root.clone(), "@");
        let found = |e: &LogsEngine, kind: jj::MatchKind| -> Vec<(String, String)> {
            let mut m: Vec<(String, String)> = e
                .view
                .found
                .iter()
                .flat_map(|f| &f.matches)
                .filter(|m| m.kind == kind)
                .map(|m| (m.change_id.clone(), m.text.clone()))
                .collect();
            m.sort();
            m
        };
        let loaded =
            |e: &LogsEngine, cid: &str| e.view.log.nodes.iter().any(|n| n.change_id == cid);

        // `base` added the line and `a` removed it; case doesn't matter.
        search_files(&mut e, "BASE");
        let mut want = vec![
            (id(&root, "main"), "base".to_string()),
            (id(&root, "feature-"), "base".to_string()),
        ];
        want.sort();
        assert_eq!(found(&e, jj::MatchKind::Line), want);
        assert!(found(&e, jj::MatchKind::Path).is_empty());
        assert!(
            loaded(&e, &id(&root, "feature-")),
            "a found revision is loaded"
        );

        // Only `b` touched `g`.
        search_files(&mut e, "g");
        assert_eq!(
            found(&e, jj::MatchKind::Path),
            vec![(id(&root, "feature"), "g".to_string())]
        );
        assert!(loaded(&e, &id(&root, "feature")));
        assert!(
            !loaded(&e, &id(&root, "feature-")),
            "the last search's are dropped"
        );

        search_files(&mut e, "");
        assert_eq!(e.view.found, None);
        assert!(!loaded(&e, &id(&root, "feature")));
    }

    #[test]
    fn a_pulled_in_branch_loads_from_its_fork_point() {
        let (_tmp, root) = repo();
        let (mut e, _rx) = engine(root.clone(), "@");
        e.refresh();
        assert_eq!(e.view.log.nodes.len(), 1, "just @");
        e.pulled = vec![
            jj::BookmarkRef {
                name: "feature".into(),
                remote: None,
                change_id: String::new(),
            }
            .revset(),
        ];
        e.refresh();
        let descs: Vec<&str> = e
            .view
            .log
            .nodes
            .iter()
            .map(|n| n.lines[0].as_str())
            .collect();
        assert_eq!(descs[0], "(no description set)", "@ first: {descs:?}");
        for d in ["b", "a", "base"] {
            assert!(descs.contains(&d), "{d} pulled in: {descs:?}");
        }
    }
}
