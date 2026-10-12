//! The engine: every side effect the dashboard performs, run on a worker thread so the
//! UI thread never blocks on jj, wezterm, SQLite, or the filesystem.
//!
//! The UI sends [`Cmd`]s and receives [`Msg`]s. The engine owns the store connection, the
//! merge train, and the refresh cadence (idle ticks plus hook-event nudges), and after each
//! command or tick publishes a [`View`] — an owned snapshot the UI renders from. Commands
//! run strictly one at a time, so faff never races itself on a jj mutation, and a refresh
//! can never observe a task half-way through being created or removed.

use super::notify::Level;
use super::{model, session, train};
use crate::domain::{Autonomy, DESCRIBE_PROMPT, Task, TaskId, TaskStatus};
use crate::graph::{self, GraphRow};
use crate::store::Store;
use crate::{config, events, jj, wezterm, workspace};
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

/// Idle refresh cadence: with nothing happening the view is rebuilt about once a second.
const IDLE_REFRESH: Duration = Duration::from_millis(1000);

/// Floor between refreshes driven by hook events, so a chatty agent can't drive
/// unbounded subprocess churn.
const MIN_REFRESH_GAP: Duration = Duration::from_millis(400);

/// Cap on prompt injections a single merge-train tick will make. `wezterm::send_text` blocks
/// the engine (~150ms each), so a large fan-out of rebase prompts is spread over ticks to
/// keep commands queued behind a tick from waiting long. Merges are separate and always
/// at most one per tick.
const MAX_MERGE_INJECTIONS_PER_TICK: usize = 4;

/// Live per-member facts gathered once per merge-train tick, so the decision logic reads
/// them instead of re-shelling to jj. `empty`/`conflict` are consumed during the drop pass
/// and don't survive into the struct.
struct MergeFacts {
    id: TaskId,
    name: String,
    /// The agent's turn is over — idle *or* needs-input — so its revision is settled and the
    /// train can act on it. `accept` is an explicit "merge this", so faff sequences on the
    /// agent (wait while it's `working`, proceed once it isn't) rather than ejecting on a
    /// status heuristic. Only `working` blocks a member from progressing.
    stable: bool,
    on_tip: bool,
    described: bool,
    /// The revision still carries conflicts (only reached while the agent is working — a
    /// settled conflicted member is dropped before it becomes a fact).
    conflict: bool,
    seq: u64,
}

/// An action requested by the UI. Task ids come from the UI's latest [`View`]; the engine
/// looks each one up afresh and quietly drops a command whose task has since vanished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    /// `n` (`handoff == false`) / `N` (`handoff == true`).
    New { handoff: bool },
    /// `↵` on the given selection: open, detach, retarget, or revive.
    Toggle(Option<TaskId>),
    /// `x` / `X`.
    Remove { id: TaskId, discard: bool },
    /// `s`, already confirmed if the agent was working.
    Swap(TaskId),
    /// `S`.
    Snapshot(TaskId),
    /// `r` / `R`, already confirmed if the agent was working.
    Rebase { id: TaskId, freeze: bool },
    /// `d`, already confirmed if the agent was working.
    Describe(TaskId),
    /// `a`: toggle into / out of the merge train.
    Accept(TaskId),
    /// `A`.
    AbortTrain,
    /// A hook event arrived: refresh soon. Not a user command, so it gets no [`Msg::Done`].
    Nudge,
}

/// What the engine reports back to the UI.
#[derive(Debug)]
pub enum Msg {
    Notice(Level, String),
    View(Box<View>),
    /// One non-[`Cmd::Nudge`] command finished (commands complete in the order sent).
    Done,
}

/// Everything the UI renders, as an owned snapshot. Rebuilt by the engine after every
/// command and refresh tick; the UI never reaches past it into jj, wezterm, or the store.
#[derive(Debug, Clone, Default)]
pub struct View {
    pub tasks: Vec<Task>,
    pub rows: Vec<GraphRow>,
    /// Parallel to `rows`: which task (if any) each row represents.
    pub task_of_node: Vec<Option<TaskId>>,
    /// The current fork point (`heads(::@ ~ empty())`) — the revision new agents branch
    /// from. Its glyph (`◆`, or `@` when it coincides) is tinted cyan in the graph.
    pub fork_point: Option<String>,
    /// Navigable order: tasks with a graph node (in graph order), then detached ones.
    pub task_order: Vec<TaskId>,
    /// Live tasks with no distinct graph node (workspace inlined into HEAD, or a
    /// stale row). Still selectable/removable; shown in a "detached" footer list.
    pub detached: Vec<TaskId>,
    /// The agent pane docked beside faff, if any.
    pub open_pane: Option<u64>,
    /// change_id -> (unique prefix, padding rest) for the id column, from jj.
    pub id_display: HashMap<String, (String, String)>,
    pub merge_train: train::Train,
    /// Pane-less tasks `↵` can put an agent back into (see `session::revivable`).
    pub revivable: HashSet<TaskId>,
    /// The subset of `revivable` with a conversation to restore (see `session::resumable`).
    pub resumable: HashSet<TaskId>,
    /// A freshly created task (`n`/`N`) the UI should move its selection onto.
    pub select: Option<TaskId>,
}

/// The engine's notice sink: the same `ok`/`warn`/… surface as `notify::Notices`, but each
/// note is sent to the UI rather than stored. A send only fails once the UI has gone, at
/// which point nobody is left to read it.
pub struct Outbox {
    tx: Sender<Msg>,
}

impl Outbox {
    fn push(&self, level: Level, text: impl Into<String>) {
        let _ = self.tx.send(Msg::Notice(level, text.into()));
    }

    pub fn ok(&self, text: impl Into<String>) {
        self.push(Level::Success, text);
    }

    pub fn warn(&self, text: impl Into<String>) {
        self.push(Level::Warn, text);
    }

    pub fn err(&self, text: impl Into<String>) {
        self.push(Level::Error, text);
    }

    fn send(&self, msg: Msg) {
        let _ = self.tx.send(msg);
    }
}

pub struct Engine {
    repo: PathBuf,
    faff_exe: PathBuf,
    socket: PathBuf,
    db: PathBuf,
    store: Store,
    /// Root of Claude Code's per-project data (`~/.claude/projects`). Held rather than
    /// looked up per call so tests can point it at a scratch dir.
    claude_projects: PathBuf,
    faff_pane: Option<u64>,
    notices: Outbox,

    // Derived state, recomputed on refresh and published as a `View`.
    tasks: Vec<Task>,
    rows: Vec<GraphRow>,
    task_of_node: Vec<Option<TaskId>>,
    fork_point: Option<String>,
    task_order: Vec<TaskId>,
    detached: Vec<TaskId>,
    open_pane: Option<u64>,
    id_display: HashMap<String, (String, String)>,
    last_refresh: Instant,
    /// Set to a freshly-created task's id (by `n`/`N`) and handed to the UI with the next
    /// view, so the selection lands on the new row. Consumed on publish.
    pending_select: Option<TaskId>,
    /// The merge train: revisions marked with `a` to be drained into your workspace one at
    /// a time. Advanced each refresh tick by `tick_merge_train`. See `tui::train`.
    merge_train: train::Train,
}

impl Engine {
    /// Open the store and bind the hook-event socket for `repo`. Hook events are forwarded
    /// onto `nudge` as [`Cmd::Nudge`] so the engine's single command queue is also its
    /// wake-up signal. Messages for the UI go to `tx`.
    pub fn open(repo: PathBuf, tx: Sender<Msg>, nudge: Sender<Cmd>) -> Result<Engine> {
        let db = config::db_path(&repo)?;
        let store = Store::open(&db)?;
        let socket = events::socket_path(&repo);
        let events_rx = events::spawn_listener(&socket)?;
        // Finish deleting any workspace dirs a previous run renamed aside but exited
        // before clearing.
        if let Ok(root) = config::workspace_root(&repo) {
            workspace::sweep_trash(&root);
        }
        std::thread::spawn(move || {
            // report-event already wrote the store; each event is only a refresh nudge.
            while events_rx.recv().is_ok() {
                if nudge.send(Cmd::Nudge).is_err() {
                    return;
                }
            }
        });
        let faff_exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("faff"));
        let faff_pane = std::env::var("WEZTERM_PANE")
            .ok()
            .and_then(|s| s.parse().ok());
        Ok(Engine::new(
            repo,
            faff_exe,
            socket,
            db,
            store,
            workspace::claude_projects_dir(),
            faff_pane,
            tx,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        repo: PathBuf,
        faff_exe: PathBuf,
        socket: PathBuf,
        db: PathBuf,
        store: Store,
        claude_projects: PathBuf,
        faff_pane: Option<u64>,
        tx: Sender<Msg>,
    ) -> Engine {
        Engine {
            repo,
            faff_exe,
            socket,
            db,
            store,
            claude_projects,
            faff_pane,
            notices: Outbox { tx },
            tasks: Vec::new(),
            rows: Vec::new(),
            task_of_node: Vec::new(),
            fork_point: None,
            task_order: Vec::new(),
            detached: Vec::new(),
            open_pane: None,
            id_display: HashMap::new(),
            last_refresh: Instant::now(),
            pending_select: None,
            merge_train: train::Train::default(),
        }
    }

    /// The worker loop: run each command as it arrives, refresh on the idle cadence or
    /// soon after a hook nudge, and publish a fresh view after either. Returns once every
    /// sender is gone.
    pub fn run(mut self, rx: Receiver<Cmd>) {
        self.refresh();
        self.publish();
        let mut nudged = false;
        loop {
            let gap = if nudged { MIN_REFRESH_GAP } else { IDLE_REFRESH };
            match rx.recv_timeout(gap.saturating_sub(self.last_refresh.elapsed())) {
                Ok(Cmd::Nudge) => nudged = true,
                Ok(cmd) => {
                    self.handle(cmd);
                    self.publish();
                    self.notices.send(Msg::Done);
                    continue;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            let elapsed = self.last_refresh.elapsed();
            if elapsed >= IDLE_REFRESH || (nudged && elapsed >= MIN_REFRESH_GAP) {
                nudged = false;
                self.refresh();
                // Advance the merge train on the freshly reconciled task state. Runs on the
                // refresh cadence so it never adds subprocess churn beyond the throttle.
                self.tick_merge_train();
                self.publish();
            }
        }
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::New { handoff: false } => self.new_task(),
            Cmd::New { handoff: true } => self.handoff_task(),
            Cmd::Toggle(id) => self.toggle_session(id),
            Cmd::Remove { id, discard } => self.remove(id, discard),
            Cmd::Swap(id) => self.perform_swap(id),
            Cmd::Snapshot(id) => self.snapshot_task(id),
            Cmd::Rebase { id, freeze } => self.perform_rebase(id, freeze),
            Cmd::Describe(id) => self.perform_describe(id),
            Cmd::Accept(id) => self.accept(id),
            Cmd::AbortTrain => self.abort_train(),
            Cmd::Nudge => {}
        }
    }

    /// Snapshot the derived state for the UI. The revive/resume checks stat transcript
    /// files, which is exactly why they are computed here and not at render time.
    fn view(&mut self) -> View {
        let revivable = self
            .tasks
            .iter()
            .filter(|t| self.task_revivable(t))
            .map(|t| t.id)
            .collect();
        let resumable = self
            .tasks
            .iter()
            .filter(|t| self.task_resumable(t))
            .map(|t| t.id)
            .collect();
        View {
            tasks: self.tasks.clone(),
            rows: self.rows.clone(),
            task_of_node: self.task_of_node.clone(),
            fork_point: self.fork_point.clone(),
            task_order: self.task_order.clone(),
            detached: self.detached.clone(),
            open_pane: self.open_pane,
            id_display: self.id_display.clone(),
            merge_train: self.merge_train.clone(),
            revivable,
            resumable,
            select: self.pending_select.take(),
        }
    }

    fn publish(&mut self) {
        let view = self.view();
        self.notices.send(Msg::View(Box::new(view)));
    }

    fn task(&self, id: TaskId) -> Option<Task> {
        self.tasks.iter().find(|t| t.id == id).cloned()
    }


    /// `n`: create an empty task, fork its workspace, spawn a bare `claude`, and open
    /// it beside faff so the user types their task straight into the session. The task
    /// prompt (and title) are captured later from the first UserPromptSubmit hook.
    fn new_task(&mut self) {
        match self.try_new_task(false) {
            Ok(id) => {
                self.notices
                    .ok(format!("new task #{id} — type your task in the pane"));
                self.pending_select = Some(id);
            }
            Err(e) => self.notices.err(format!("new task failed: {e}")),
        }
        self.refresh();
    }

    /// `N` (Shift+n): hand off your current work. Like `new_task`, but the agent takes
    /// over your current revision — it continues your exact commit — and your own `@`
    /// retreats to the fork point from before your changes (see `workspace::handoff`).
    /// You still type what you want the agent to finish into its pane, exactly as `n`.
    fn handoff_task(&mut self) {
        match self.try_new_task(true) {
            Ok(id) => {
                self.notices.ok(format!(
                    "handed off #{id} — type what to finish in the pane"
                ));
                self.pending_select = Some(id);
            }
            Err(e) => self.notices.err(format!("handoff failed: {e}")),
        }
        self.refresh();
    }

    /// Shared task-creation path for `n` (`handoff == false`) and `N` (`handoff == true`).
    /// The only difference is how the workspace is forked (`prepare_workspace`); spawning,
    /// docking, rollback, and status are identical.
    fn try_new_task(&mut self, handoff: bool) -> Result<TaskId> {
        let faff = self
            .faff_pane
            .context("run faff inside WezTerm to spawn agents")?;
        let task = self.store.create_task("", 0, Autonomy::Inherit)?;
        // Roll the row back if workspace prep fails (no invisible zombie).
        if let Err(e) = self.prepare_workspace(task.id, "", handoff) {
            let _ = self.store.delete_task(task.id);
            return Err(e);
        }
        let t = self.store.get_task(task.id)?;
        match self.spawn_claude(&t) {
            Ok(pane) => {
                let _ = self.store.set_pane(task.id, Some(pane));
                // Awaiting the user's first prompt — literally needs their input.
                let _ = self.store.update_status(task.id, TaskStatus::NeedsInput);
                // Dock it beside faff (detaching any already-open session first) and
                // focus it — a new task lands the user in the agent to type the task.
                self.open_session(faff, pane, true);
                Ok(task.id)
            }
            Err(e) => {
                // Full rollback: tear the workspace back down and drop the row.
                if let (Some(n), Some(p), Some(f)) = (&t.ws_name, &t.ws_path, &t.fork_point) {
                    let _ = workspace::teardown(&self.repo, n, p, f);
                }
                let _ = self.store.delete_task(task.id);
                Err(e)
            }
        }
    }

    fn prepare_workspace(&self, id: TaskId, prompt: &str, handoff: bool) -> Result<()> {
        let slug = config::slugify(prompt, 4);
        let name = format!("faf-task-{}", id.0);
        let path = config::task_workspace_dir(&self.repo, id.0, &slug)?;
        let ws = if handoff {
            workspace::handoff(&self.repo, &name, &path)?
        } else {
            workspace::create(&self.repo, &name, &path)?
        };
        // The workspace now exists on disk + in jj. If recording it fails, tear it
        // back down so we don't leak an untracked jj workspace (the DB row is rolled
        // back separately by the caller).
        if let Err(e) =
            self.store
                .set_workspace(id, &ws.name, &ws.path, &ws.change_id, &ws.fork_point)
        {
            let _ = workspace::teardown(&self.repo, &ws.name, &ws.path, &ws.fork_point);
            return Err(e);
        }
        // Best-effort: memory seed, hook injection, and pre-trust the workspace dir
        // so the agent doesn't hit the "trust this folder?" dialog on spawn.
        let claude_projects = workspace::claude_projects_dir();
        let _ = workspace::seed_memory(&claude_projects, &self.repo, &ws.path);
        let mem_dir = claude_projects
            .join(config::encode_repo_path(&ws.path))
            .join("memory");
        let _ = workspace::write_hooks(
            &ws.path,
            id.0,
            &self.faff_exe,
            &self.socket,
            &self.db,
            &mem_dir,
        );
        let _ = workspace::trust_workspace(&ws.path);
        Ok(())
    }

    /// Whether this task's conversation is already on disk, i.e. whether relaunching it
    /// must resume rather than create. Keyed on the task's *workspace* path, because that
    /// is the directory claude was launched in and thus the project key it wrote under.
    fn transcript_exists(&self, t: &Task, session_id: &str) -> bool {
        t.ws_path
            .as_deref()
            .is_some_and(|ws| workspace::session_exists(&self.claude_projects, ws, session_id))
    }

    /// The `claude` argv for (re)launching this task's pane: pin the minted id on a first
    /// boot, resume the conversation on every boot after that.
    fn task_argv(&self, t: &Task) -> Vec<String> {
        let sid = t.session_id.as_deref();
        let launch = match sid {
            None => session::Launch::Bare,
            Some(s) if self.transcript_exists(t, s) => session::Launch::Resume,
            Some(_) => session::Launch::Fresh,
        };
        session::claude_argv(
            sid.unwrap_or_default(),
            launch,
            &t.prompt,
            t.autonomy.permission_mode(),
        )
    }

    /// Whether Enter can put an agent back into this task. Broader than `task_resumable`:
    /// a task whose pane died before its first prompt has no conversation to restore, but
    /// its workspace is intact and a blank agent belongs in it. That is the commonest lost
    /// instance in faff, because `n` deliberately leaves the agent waiting to be typed at.
    fn task_revivable(&self, t: &Task) -> bool {
        session::revivable(t.pane_id, t.ws_path.is_some())
    }

    /// A *lost instance*: the agent's pane is gone but its conversation survived, so
    /// faff can bring it back. Distinct from a task that simply never started.
    fn task_resumable(&self, t: &Task) -> bool {
        let sid = t.session_id.as_deref();
        session::resumable(
            t.pane_id,
            sid,
            sid.is_some_and(|s| self.transcript_exists(t, s)),
        )
    }

    /// Launch a `claude` pane in the task's workspace and title its tab. The argv comes
    /// from `task_argv`, so this is also the relaunch path for a lost instance: a first
    /// boot pins the task's minted session id, a later one resumes the conversation that
    /// id names. A task created via `n` spawns with no prompt so the user types the task
    /// live; the prompt is passed as the initial message only if the task already has one
    /// (and never on a resume — the conversation already holds it). `--permission-mode` is
    /// passed only for an explicit override — otherwise the user's own default (e.g.
    /// `permissions.defaultMode=auto`) is inherited.
    fn spawn_claude(&self, t: &Task) -> Result<u64> {
        let ws_path = t.ws_path.clone().context("task has no workspace")?;
        let argv = self.task_argv(t);
        let prog: Vec<&str> = argv.iter().map(String::as_str).collect();
        let pane = wezterm::spawn(&ws_path, &prog)?;
        let _ = wezterm::set_tab_title(pane, &format!("#{}", t.id.0));
        // `wezterm cli spawn` activates the new tab, stealing focus from faff. Return
        // focus to faff so spawning an agent never yanks the user out of the TUI (a new
        // task re-focuses its own pane afterward — see open_session).
        if let Some(faff) = self.faff_pane {
            let _ = wezterm::activate_pane(faff);
        }
        Ok(pane)
    }

    /// Dock `pane` beside faff, ensuring only one session is ever docked: any
    /// currently-open session is detached first. Idempotent if `pane` is already open.
    /// `focus` decides where the cursor lands: a new task focuses its fresh agent so
    /// the user can type the task straight away; every other dock leaves focus on faff
    /// (the user swaps with their own WezTerm keybinds).
    fn open_session(&mut self, faff: u64, pane: u64, focus: bool) {
        if let Some(prev) = self.open_pane
            && prev != pane
        {
            let _ = wezterm::detach(prev);
        }
        if wezterm::open_beside(faff, pane).is_ok() {
            self.open_pane = Some(pane);
            // open_beside activates the moved pane; set focus explicitly either way.
            let _ = wezterm::activate_pane(if focus { pane } else { faff });
        }
    }

    /// Recreate a lost instance: relaunch `claude --resume` in the task's jj workspace,
    /// which the pane's death never touched, and dock it focused so the user can carry
    /// on typing. The conversation comes back; the *process* does not — a turn in flight
    /// when the pane died is gone, and MCP/background state starts over.
    ///
    /// Deliberately manual rather than automatic in `reconcile_panes`: a pane usually
    /// vanishes because the user closed it, and respawning that on sight would be a
    /// fight, not a feature.
    fn revive_task(&mut self, faff: u64, t: &Task) {
        let restoring = self.task_resumable(t);
        match self.spawn_claude(t) {
            Ok(pane) => {
                let _ = self.store.set_pane(t.id, Some(pane));
                if restoring {
                    self.notices
                        .ok(format!("revived #{} — conversation restored", t.id));
                } else {
                    self.notices.ok(format!(
                        "started #{} — fresh agent, no conversation to restore",
                        t.id
                    ));
                }
                self.open_session(faff, pane, true);
            }
            Err(e) => self.notices.err(format!("revive #{} failed: {e}", t.id)),
        }
        self.refresh();
    }

    /// `↵` on task `id` (the UI's selection): open, detach, retarget, or revive — decided
    /// here against live pane state, not the UI's possibly-stale snapshot.
    fn toggle_session(&mut self, id: Option<TaskId>) {
        let selected = id.and_then(|id| self.task(id));
        let selected_pane = selected.as_ref().and_then(|t| t.pane_id);
        let revivable = selected.as_ref().is_some_and(|t| self.task_revivable(t));
        let Some(faff) = self.faff_pane else {
            self.notices
                .warn("no WEZTERM_PANE; run faff inside WezTerm");
            return;
        };
        match session::decide(self.open_pane, selected_pane, revivable) {
            session::Toggle::Nothing => self.notices.warn("no session for this task"),
            // Open and Retarget both route through open_session (which detaches any
            // currently-docked session first).
            session::Toggle::Open(p) | session::Toggle::Retarget { open: p, .. } => {
                // Docking to view an existing agent keeps focus on faff.
                self.open_session(faff, p, false)
            }
            // The pane died but the conversation did not: bring the agent back.
            session::Toggle::Revive => {
                if let Some(t) = selected {
                    self.revive_task(faff, &t);
                }
            }
            session::Toggle::Detach(p) => {
                if wezterm::detach(p).is_ok() {
                    self.open_pane = None;
                    // Ejecting the pane to a new tab activates it; stay on faff.
                    let _ = wezterm::activate_pane(faff);
                }
            }
        }
    }

    /// Remove task `id`: kill its pane, tear down its jj workspace, and drop the
    /// row from the store. There is no archive/history — a removed task is gone.
    ///
    /// `x` (`discard_revision = false`) preserves the task's real work as ordinary jj
    /// history; `X`/Shift+x (`discard_revision = true`) abandons the revision too,
    /// throwing the work away (see `workspace::teardown_discarding_revision`).
    fn remove(&mut self, id: TaskId, discard_revision: bool) {
        if self.task(id).is_none() {
            return;
        }
        self.remove_task(id, discard_revision);
        let msg = if discard_revision {
            format!("removed #{} and discarded its revision", id.0)
        } else {
            format!("removed #{}", id.0)
        };
        self.notices.ok(msg);
        self.refresh();
    }

    /// Tear a task down by id: undock and kill its pane, retire its workspace (abandoning
    /// its revision only when `discard_revision`), drop the row, and drop it from the merge
    /// train. Sets no status and does not refresh — the caller owns both.
    fn remove_task(&mut self, id: TaskId, discard_revision: bool) {
        let Some(t) = self.tasks.iter().find(|t| t.id == id).cloned() else {
            return;
        };
        if let Some(p) = t.pane_id {
            if self.open_pane == Some(p) {
                self.open_pane = None;
            }
            let _ = wezterm::kill_pane(p);
        }
        if let (Some(name), Some(path), Some(fork)) =
            (t.ws_name.clone(), t.ws_path.clone(), t.fork_point.clone())
        {
            let _ = if discard_revision {
                workspace::teardown_discarding_revision(&self.repo, &name, &path, &fork)
            } else {
                workspace::teardown(&self.repo, &name, &path, &fork)
            };
        }
        let _ = self.store.delete_task(id);
        self.merge_train.remove(id);
    }


    /// Run the swap for task `id` (already validated / confirmed) and refresh.
    fn perform_swap(&mut self, id: TaskId) {
        let Some(t) = self.tasks.iter().find(|t| t.id == id).cloned() else {
            return;
        };
        let (Some(name), Some(path)) = (t.ws_name.clone(), t.ws_path.clone()) else {
            self.notices.warn("no workspace to swap for this task");
            return;
        };
        match workspace::swap(&self.repo, &name, &path) {
            Ok(new_rev) => {
                // Keep the recorded revision honest (the graph rebuilds from jj anyway).
                let _ = self.store.set_ws_change_id(id, &new_rev);
                self.notices.ok(format!("swapped @ ⇄ #{}", id.0));
            }
            Err(e) => self.notices.err(format!("swap failed: {e}")),
        }
        self.refresh();
    }

    /// `S`: snapshot task `id`'s workspace so edits from an agent that hasn't run a jj
    /// command land in its `@` and show up in the graph.
    fn snapshot_task(&mut self, id: TaskId) {
        let Some(t) = self.task(id) else {
            return;
        };
        let Some(path) = t.ws_path.clone() else {
            self.notices.warn("no workspace to snapshot for this task");
            return;
        };
        match workspace::snapshot(&path) {
            Ok(()) => self.notices.ok(format!("snapshotted #{}", t.id.0)),
            Err(e) => self.notices.err(format!("snapshot failed: {e}")),
        }
        self.refresh();
    }


    /// Compute the new base and inject the rebase prompt into the agent's pane (already
    /// validated / confirmed). No-op when the agent is already on the latest base.
    fn perform_rebase(&mut self, id: TaskId, freeze: bool) {
        let Some(t) = self.tasks.iter().find(|t| t.id == id).cloned() else {
            return;
        };
        let (Some(name), Some(pane)) = (t.ws_name.clone(), t.pane_id) else {
            self.notices
                .warn("no workspace/pane to rebase for this task");
            return;
        };
        match workspace::refresh(&self.repo, &name, freeze) {
            Ok(workspace::Refresh::AlreadyFresh) => {
                self.notices
                    .warn(format!("#{} is already on the latest base", id.0));
            }
            Ok(workspace::Refresh::Rebase { prompt, .. }) => {
                match wezterm::send_text(pane, &prompt) {
                    Ok(()) => self.notices.ok(format!("sent rebase to #{}", id.0)),
                    Err(e) => self.notices.err(format!("rebase send failed: {e}")),
                }
            }
            Err(e) => self.notices.err(format!("rebase failed: {e}")),
        }
        self.refresh();
    }


    /// Inject the describe prompt into the agent's pane (already validated / confirmed).
    fn perform_describe(&mut self, id: TaskId) {
        let Some(t) = self.tasks.iter().find(|t| t.id == id).cloned() else {
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
        self.refresh();
    }

    // ---- merge train (`a` / `A`) ----

    /// `a`: toggle task `id`'s revision into/out of the merge train. Rejects a task
    /// with nothing to merge or no live pane, and refuses to *start* a train while your own `@`
    /// still holds work: the train lands revisions onto an empty, description-less `@`.
    fn accept(&mut self, id: TaskId) {
        let Some(t) = self.task(id) else {
            return;
        };
        // Already queued → toggle it back out.
        if self.merge_train.contains(t.id) {
            self.merge_train.remove(t.id);
            self.notices
                .ok(format!("#{} removed from the merge train", t.id.0));
            return;
        }
        let Some(name) = t.ws_name.clone() else {
            self.notices.warn("no workspace to accept for this task");
            return;
        };
        if t.pane_id.is_none() {
            self.notices.warn(format!(
                "no live pane to drive #{} through the merge train",
                t.id.0
            ));
            return;
        }
        // Nothing to merge if the agent's revision is empty.
        if !jj::any_revision(&self.repo, &format!("{name}@ ~ empty()")).unwrap_or(false) {
            self.notices
                .warn(format!("#{} has no content to merge", t.id.0));
            return;
        }
        // Starting a train needs a clear `@` to land the first revision onto.
        if self.merge_train.is_empty() && !self.at_is_clear() {
            self.notices
                .warn("your @ has content — commit or hand it off before accepting");
            return;
        }
        self.merge_train.add(t.id);
        self.notices
            .ok(format!("#{} accepted into the merge train", t.id.0));
    }

    /// `A`: abort the merge train — dequeue everything still pending. Revisions already
    /// taken over stay merged; nothing is rolled back.
    fn abort_train(&mut self) {
        if self.merge_train.is_empty() {
            self.notices.warn("no merge train to abort");
            return;
        }
        let n = self.merge_train.len();
        self.merge_train.clear();
        self.notices
            .ok(format!("merge train aborted — {n} dequeued"));
    }

    /// Whether your `@` is an empty, description-less commit — the landing spot the train
    /// needs. On a jj error it reports "not clear" (never merge onto an unknown `@`).
    fn at_is_clear(&self) -> bool {
        let empty = !jj::any_revision(&self.repo, "@ ~ empty()").unwrap_or(true);
        let described = !jj::description(&self.repo, "@")
            .unwrap_or_default()
            .is_empty();
        empty && !described
    }

    fn member_cooling(&self, id: TaskId, now: Instant) -> bool {
        self.merge_train
            .members
            .iter()
            .find(|m| m.task == id)
            .map(|m| m.cooling(now))
            .unwrap_or(false)
    }

    /// Advance the merge train by at most one integration step (called each refresh tick).
    ///
    /// Each tick: drop members faff genuinely can't merge (conflicted, empty, or vanished);
    /// recompute every survivor's display stage; then, if `@` is a clear landing spot, take
    /// over one ready revision (`jj new`). If none is ready, nudge work forward concurrently
    /// — describe the earliest on-tip member that still lacks a description, and ask off-tip
    /// members to rebase onto the tip (the ready-first fan-out; injections are capped per
    /// tick so `send_text` never freezes the UI for long). A `working` member is simply
    /// waited on — never ejected — since `accept` is an explicit request to merge it.
    fn tick_merge_train(&mut self) {
        if self.merge_train.is_empty() {
            return;
        }
        let now = Instant::now();
        self.merge_train.note.clear();

        // The tip the set lands on: the newest non-empty ancestor of @ (which advances as
        // revisions are taken over). A transient jj error just defers to the next tick.
        let Ok(tip) = jj::resolve_change_id(&self.repo, "heads(::@ ~ empty())") else {
            return;
        };

        // 1. Gather live facts, dropping members that can't be cleanly accepted.
        let mut facts: Vec<MergeFacts> = Vec::new();
        let member_ids: Vec<TaskId> = self.merge_train.members.iter().map(|m| m.task).collect();
        for id in member_ids {
            let Some(t) = self.tasks.iter().find(|t| t.id == id).cloned() else {
                self.merge_train.remove(id); // task vanished
                continue;
            };
            let Some(name) = t.ws_name.clone() else {
                self.merge_train.remove(id);
                continue;
            };
            let info = jj::log(&self.repo, &format!("{name}@"))
                .ok()
                .and_then(|v| v.into_iter().next());
            let (empty, conflict, described) = match &info {
                Some(r) => (r.empty, r.conflict, !r.description.is_empty()),
                None => (false, false, false),
            };
            // The agent's turn is over (not `working`) → its revision is settled. A working
            // agent is waited on, not dropped.
            let stable = t.status != TaskStatus::Working;
            // A settled-but-conflicted revision, or a settled one gone empty, genuinely
            // can't merge — drop it (leaving it as an ordinary task to handle by hand).
            if stable && conflict {
                self.merge_train.remove(id);
                self.notices.warn(format!(
                    "#{} has conflicts — dropped from the merge train",
                    id.0
                ));
                continue;
            }
            if stable && empty {
                self.merge_train.remove(id);
                self.notices
                    .warn(format!("#{} has no content to merge — dropped", id.0));
                continue;
            }
            let on_tip =
                jj::any_revision(&self.repo, &format!("{tip} & ::({name}@)")).unwrap_or(false);
            let seq = self
                .merge_train
                .members
                .iter()
                .find(|m| m.task == id)
                .map(|m| m.seq)
                .unwrap_or(0);
            facts.push(MergeFacts {
                id,
                name,
                stable,
                on_tip,
                described,
                conflict,
                seq,
            });
        }
        if facts.is_empty() {
            return;
        }

        // 2. Recompute each survivor's display stage. A settled member is labelled by where it
        //    sits in the pipeline; a working one by whatever prompt faff last handed it (so an
        //    agent still on its own task reads "working", not "describing").
        for f in &facts {
            let last = self
                .merge_train
                .members
                .iter()
                .find(|m| m.task == f.id)
                .and_then(|m| m.last_inject);
            let stage = if f.conflict {
                train::Stage::Resolving
            } else if !f.stable {
                match last {
                    Some(train::Injected::Rebase) => train::Stage::Rebasing,
                    Some(train::Injected::Describe) => train::Stage::Describing,
                    None => train::Stage::Working,
                }
            } else if !f.on_tip {
                train::Stage::Rebasing
            } else if !f.described {
                train::Stage::Describing
            } else {
                train::Stage::Ready
            };
            if let Some(m) = self.merge_train.member_mut(f.id) {
                m.stage = stage;
            }
        }

        // 3. The landing spot must be a clear `@` — never merge onto your own WIP.
        if !self.at_is_clear() {
            self.merge_train.note = "paused — your @ has content; clear it to resume".to_string();
            return;
        }

        // 4. Take over one ready revision (settled, on the tip, described), earliest accepted.
        if let Some(f) = facts
            .iter()
            .filter(|f| f.stable && f.on_tip && f.described)
            .min_by_key(|f| f.seq)
        {
            let id = f.id;
            if let Some(m) = self.merge_train.member_mut(id) {
                m.stage = train::Stage::Merging;
            }
            self.merge_train_take_over(id);
            return; // state changed under us; the next tick continues the drain
        }

        // 5. Not merging this tick — nudge work forward concurrently (capped injections).
        let mut sent = 0usize;
        // (a) Describe the earliest on-tip settled member that still lacks a description.
        if let Some(f) = facts
            .iter()
            .filter(|f| f.stable && f.on_tip && !f.described)
            .min_by_key(|f| f.seq)
        {
            let id = f.id;
            if !self.member_cooling(id, now) {
                self.merge_train_inject(
                    id,
                    train::Injected::Describe,
                    DESCRIBE_PROMPT.to_string(),
                    now,
                );
                sent += 1;
            }
        }
        // (b) Ask off-tip settled members to rebase onto the tip (the ready-first fan-out).
        let mut off_tip: Vec<&MergeFacts> =
            facts.iter().filter(|f| f.stable && !f.on_tip).collect();
        off_tip.sort_by_key(|f| f.seq);
        for f in off_tip {
            if sent >= MAX_MERGE_INJECTIONS_PER_TICK {
                break;
            }
            let id = f.id;
            if self.member_cooling(id, now) {
                continue;
            }
            if let Ok(workspace::Refresh::Rebase { prompt, .. }) =
                workspace::refresh_onto(&self.repo, &f.name, &tip)
            {
                self.merge_train_inject(id, train::Injected::Rebase, prompt, now);
                sent += 1;
            }
        }
    }

    /// Inject `prompt` into a member's agent pane, record what was sent (for the panel), and
    /// start its cooldown (see `train::INJECT_COOLDOWN`). Best-effort: a send failure is left
    /// for the next tick.
    fn merge_train_inject(
        &mut self,
        id: TaskId,
        kind: train::Injected,
        prompt: String,
        now: Instant,
    ) {
        let Some(pane) = self
            .tasks
            .iter()
            .find(|t| t.id == id)
            .and_then(|t| t.pane_id)
        else {
            return;
        };
        let _ = wezterm::send_text(pane, &prompt);
        if let Some(m) = self.merge_train.member_mut(id) {
            m.cooldown_until = Some(now + train::INJECT_COOLDOWN);
            m.last_inject = Some(kind);
        }
    }

    /// Take a ready member's revision over into your workspace (`jj new <head>`) and retire
    /// its agent. On success the revision is now integrated into `::@`, so `teardown` keeps
    /// it. On failure the member stays in the train and is retried next tick.
    fn merge_train_take_over(&mut self, id: TaskId) {
        let Some(t) = self.tasks.iter().find(|t| t.id == id).cloned() else {
            return;
        };
        let (Some(name), Some(path)) = (t.ws_name.clone(), t.ws_path.clone()) else {
            return;
        };
        match workspace::take_over(&self.repo, &name, &path) {
            Ok(_head) => {
                self.remove_task(id, false); // also drops it from the train
                self.notices
                    .ok(format!("merged #{} into your workspace", id.0));
            }
            Err(e) => self.notices.err(format!("merge of #{} failed: {e}", id.0)),
        }
        self.refresh();
    }

    // ---- refresh (recompute view from store + jj) ----

    fn refresh(&mut self) {
        self.tasks = self.store.list_tasks().unwrap_or_default();

        // Fetch the authoritative jj workspace list once (shared by reconcile + graph).
        // The poll peeks (no snapshot, no git import) so it never records an operation
        // that could race your own `jj git fetch`/rebase into a divergent op log.
        let mut ws_list = jj::workspace_list_peek(&self.repo).ok();

        // Heal orphans so stale rows don't linger forever or misreport their status.
        let panes = wezterm::list().ok();
        if let Some(p) = &panes {
            self.reconcile_panes(p);
        }
        if let Some(ws) = &ws_list {
            self.reconcile_workspaces(ws);
        }
        // Forget faff workspaces no task tracks — ghosts from a remove/swap that didn't
        // fully unregister — and drop them from the list so the graph doesn't redraw a
        // workspace we just forgot. This is what stops a stale `faf-task-N` registration
        // from blocking the next `n` with a workspace-name collision.
        if let Some(ws) = &mut ws_list {
            let forgotten = self.reconcile_orphan_workspaces(ws);
            ws.retain(|w| !forgotten.contains(&w.name));
        }
        // Reload to reflect any status changes the reconcile passes made.
        self.tasks = self.store.list_tasks().unwrap_or_default();

        // Recover which agent (if any) is already docked beside faff. Derived from the
        // live layout each refresh, so a restart — where `open_pane` starts `None` —
        // doesn't leave faff blind to an already-docked session and re-open (double) it.
        if let Some(p) = &panes {
            self.open_pane = self.detect_open_pane(p);
        }

        // Build the revision graph from the (already-fetched) workspace list.
        if let Some(ws) = &ws_list {
            match self.build_rows(ws) {
                Ok((rows, task_of, id_display, fork_point)) => {
                    self.rows = rows;
                    self.task_of_node = task_of;
                    self.id_display = id_display;
                    self.fork_point = fork_point;
                }
                Err(e) => self.notices.err(format!("jj error: {e}")),
            }
        }

        // Navigable list = ALL live tasks (store-driven, not graph-driven): tasks that
        // appear as graph nodes first (in graph order), then any detached tasks
        // (workspace inlined into HEAD, or otherwise not a distinct node). This keeps
        // every task selectable/removable even after its change is integrated.
        let graph_tasks: Vec<TaskId> = self.task_of_node.iter().flatten().copied().collect();
        let in_graph: std::collections::HashSet<TaskId> = graph_tasks.iter().copied().collect();
        self.detached = self
            .tasks
            .iter()
            .filter(|t| !in_graph.contains(&t.id))
            .map(|t| t.id)
            .collect();
        self.task_order = graph_tasks;
        self.task_order.extend(self.detached.iter().copied());

        self.last_refresh = Instant::now();
    }

    /// Heal orphaned tasks so their status stays honest. Both cases below are flipped
    /// back to `Idle` (and any dead pane cleared):
    /// - a task whose agent pane has vanished (finished/died while we weren't looking);
    /// - a `Working` task that never got a pane (its `wezterm spawn` failed).
    fn reconcile_panes(&self, panes: &[wezterm::Pane]) {
        let alive: std::collections::HashSet<u64> = panes.iter().map(|p| p.pane_id).collect();
        for t in &self.tasks {
            match t.pane_id {
                Some(p) if !alive.contains(&p) => {
                    let _ = self.store.set_pane(t.id, None);
                    let _ = self.store.update_status(t.id, TaskStatus::Idle);
                }
                None if matches!(t.status, TaskStatus::Working | TaskStatus::NeedsInput) => {
                    // Working/NeedsInput imply a running agent, but there's no pane —
                    // a failed spawn. Park it as Idle.
                    let _ = self.store.update_status(t.id, TaskStatus::Idle);
                }
                _ => {}
            }
        }
    }

    /// Derive which agent pane (if any) is currently docked beside faff: a known task
    /// pane that shares faff's WezTerm tab (that is how `open_beside` docks it). This
    /// lets faff recover the docked session after a restart — when `open_pane` starts
    /// `None` — instead of treating Enter as a fresh open and spawning a duplicate
    /// split. An agent detached to its own tab, or a non-agent pane, is not "open".
    fn detect_open_pane(&self, panes: &[wezterm::Pane]) -> Option<u64> {
        let faff = self.faff_pane?;
        let faff_tab = panes.iter().find(|p| p.pane_id == faff)?.tab_id;
        let agent_panes: std::collections::HashSet<u64> =
            self.tasks.iter().filter_map(|t| t.pane_id).collect();
        panes
            .iter()
            .find(|p| p.tab_id == faff_tab && p.pane_id != faff && agent_panes.contains(&p.pane_id))
            .map(|p| p.pane_id)
    }

    /// Drop tasks whose jj workspace has vanished (integrated + cleaned, or forgotten
    /// outside faff): the workspace is gone, so the task is done — remove it. Keeps the
    /// active list honest. Only runs with an authoritative workspace list.
    fn reconcile_workspaces(&self, workspaces: &[jj::Workspace]) {
        let live: std::collections::HashSet<&str> =
            workspaces.iter().map(|w| w.name.as_str()).collect();
        for t in &self.tasks {
            if let Some(name) = &t.ws_name
                && !live.contains(name.as_str())
            {
                if let Some(p) = &t.ws_path {
                    let _ = workspace::discard_dir(p); // best-effort dir cleanup
                }
                let _ = self.store.delete_task(t.id);
            }
        }
    }

    /// faff-owned jj workspaces (`faf-task-<id>`) that no task row claims — ghosts left by
    /// a remove or swap that didn't fully unregister. A workspace is a ghost only when the
    /// store has **no task row for its id** (`faf-task-<id>` ↔ task `<id>` is fixed; see
    /// `prepare_workspace`).
    ///
    /// This keys on row existence, not on the task's `ws_name` — and that is what makes it
    /// safe against a *concurrent* faff instance mid-create. `create_task` commits the row
    /// *before* `jj workspace add` registers the workspace, whereas `ws_name` is written
    /// only *after* the add. So a sibling instance that keyed on `ws_name` would forget an
    /// in-flight workspace whose task is perfectly live (the bug: "workspace … missing
    /// after add"). The store is read fresh here rather than from `self.tasks`: `self.tasks`
    /// is snapshotted at the top of `refresh`, possibly before the sibling committed its
    /// row, whereas a read *now* is necessarily after we listed the workspace — hence after
    /// its row was committed. Pure w.r.t. jj: returns names to forget, no side effects.
    fn orphaned_workspaces(&self, workspaces: &[jj::Workspace]) -> Vec<String> {
        let live: std::collections::HashSet<i64> = self
            .store
            .list_tasks()
            .unwrap_or_default()
            .iter()
            .map(|t| t.id.0)
            .collect();
        workspaces
            .iter()
            .filter_map(|w| {
                // Only faff-owned names with a numeric id; anything else is left alone.
                let id: i64 = w.name.strip_prefix("faf-task-")?.parse().ok()?;
                (!live.contains(&id)).then(|| w.name.clone())
            })
            .collect()
    }

    /// Forget the ghost workspaces `orphaned_workspaces` finds, so a future task's id and
    /// its `faf-task-<id>` name can never collide with a stale registration on `n`.
    /// Returns the names forgotten so the caller can drop them from this cycle's graph.
    /// Best-effort: a forget that fails is retried next refresh.
    fn reconcile_orphan_workspaces(&self, workspaces: &[jj::Workspace]) -> Vec<String> {
        let orphans = self.orphaned_workspaces(workspaces);
        for name in &orphans {
            let _ = jj::workspace_forget(&self.repo, name);
        }
        orphans
    }

    #[allow(clippy::type_complexity)]
    fn build_rows(
        &self,
        workspaces: &[jj::Workspace],
    ) -> Result<(
        Vec<GraphRow>,
        Vec<Option<TaskId>>,
        std::collections::HashMap<String, (String, String)>,
        Option<String>,
    )> {
        let revset = jj::workspace_ancestors_revset(workspaces, 25);
        let mut revs = jj::log_peek(&self.repo, &revset)?;
        // HEAD's `@` leads the log on lane 0; every agent is lifted to sit directly above
        // the trunk revision it forked from, so each folds to a one-row `├─●` stub.
        model::order_by_fork_point(&mut revs, workspaces, &self.tasks);
        // change_id -> (unique prefix, padding rest) for the id column.
        let id_display = revs
            .iter()
            .map(|r| {
                (
                    r.change_id.clone(),
                    (r.id_prefix.clone(), r.id_rest.clone()),
                )
            })
            .collect();
        let m = model::build(&revs, workspaces, &self.tasks);
        let rows = graph::render(&m.nodes);
        // task_of is parallel to rows: which task (if any) each row represents. For the
        // combined HEAD+agent node this hangs on the agent line, not the HEAD header.
        let task_of = model::row_tasks(&rows, &m.nodes, &m.task_of);
        Ok((rows, task_of, id_display, m.fork_point))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    // Build a minimal Engine with an in-memory store (no jj/wezterm needed). The receiver
    // collects everything the engine would have sent the UI.
    fn test_engine() -> (Engine, Receiver<Msg>) {
        let (tx, rx) = mpsc::channel();
        let engine = Engine::new(
            PathBuf::from("/tmp/repo"),
            PathBuf::from("/bin/faff"),
            PathBuf::from("/tmp/faf.sock"),
            PathBuf::from("/tmp/faf.db"),
            Store::open_memory().unwrap(),
            PathBuf::from("/nope/projects"),
            Some(1),
            tx,
        );
        (engine, rx)
    }

    /// The notices the engine has sent so far, oldest first.
    fn notices(rx: &Receiver<Msg>) -> Vec<(Level, String)> {
        rx.try_iter()
            .filter_map(|m| match m {
                Msg::Notice(level, text) => Some((level, text)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_command_for_a_vanished_task_is_dropped_quietly() {
        // The UI's view can trail the store by a tick: a remove for a task that is already
        // gone must not claim to have removed it.
        let (mut e, rx) = test_engine();
        e.handle(Cmd::Remove {
            id: TaskId(99),
            discard: false,
        });
        assert!(notices(&rx).is_empty());
    }

    #[test]
    fn view_carries_a_created_task_for_selection_exactly_once() {
        let (mut e, _rx) = test_engine();
        let t = e.store.create_task("a", 0, Autonomy::Inherit).unwrap();
        e.pending_select = Some(t.id);
        assert_eq!(e.view().select, Some(t.id));
        assert_eq!(e.view().select, None, "consumed by the first publish");
    }

    #[test]
    fn accept_toggles_a_queued_task_back_out() {
        // Pressing `a` on a task already in the train removes it (no jj needed on this path).
        let (mut e, rx) = test_engine();
        let t = e.store.create_task("do x", 0, Autonomy::Inherit).unwrap();
        e.tasks = e.store.list_tasks().unwrap();
        e.merge_train.add(t.id);

        e.handle(Cmd::Accept(t.id));
        assert!(!e.merge_train.contains(t.id), "second a dequeues it");
        let (level, text) = notices(&rx).pop().unwrap();
        assert_eq!(level, Level::Success);
        assert!(text.contains("removed from the merge train"));
    }

    #[test]
    fn abort_train_clears_all_and_reports() {
        let (mut e, rx) = test_engine();
        e.handle(Cmd::AbortTrain);
        assert_eq!(
            notices(&rx),
            vec![(Level::Warn, "no merge train to abort".to_string())]
        );

        e.merge_train.add(TaskId(1));
        e.merge_train.add(TaskId(2));
        e.handle(Cmd::AbortTrain);
        assert!(e.merge_train.is_empty());
        let (level, text) = notices(&rx).pop().unwrap();
        assert_eq!(level, Level::Success);
        assert!(text.contains("aborted"), "notice: {text}");
    }

    #[test]
    fn reconcile_heals_orphaned_panes_to_idle() {
        let (mut e, _rx) = test_engine();
        // Task A: Working with a pane that is still alive.
        let a = e.store.create_task("a", 0, Autonomy::AcceptEdits).unwrap();
        e.store.set_pane(a.id, Some(10)).unwrap();
        e.store.update_status(a.id, TaskStatus::Working).unwrap();
        // Task B: Working with a pane that has vanished.
        let b = e.store.create_task("b", 0, Autonomy::AcceptEdits).unwrap();
        e.store.set_pane(b.id, Some(99)).unwrap();
        e.store.update_status(b.id, TaskStatus::Working).unwrap();
        // Task C: Working but never got a pane (a failed spawn).
        let c = e.store.create_task("c", 0, Autonomy::AcceptEdits).unwrap();
        e.store.update_status(c.id, TaskStatus::Working).unwrap();
        e.tasks = e.store.list_tasks().unwrap();

        // Only pane 10 is alive.
        let panes = vec![wezterm::Pane {
            window_id: 1,
            tab_id: 1,
            pane_id: 10,
            workspace: String::new(),
            title: String::new(),
            cwd: String::new(),
        }];
        e.reconcile_panes(&panes);

        // A stays Working (pane alive); B healed (dead pane); C healed (no pane).
        assert_eq!(e.store.get_task(a.id).unwrap().status, TaskStatus::Working);
        let b2 = e.store.get_task(b.id).unwrap();
        assert_eq!(b2.status, TaskStatus::Idle);
        assert_eq!(b2.pane_id, None);
        assert_eq!(e.store.get_task(c.id).unwrap().status, TaskStatus::Idle);
    }

    #[test]
    fn detects_docked_agent_pane_after_restart() {
        fn pane(tab: u64, id: u64, title: &str) -> wezterm::Pane {
            wezterm::Pane {
                window_id: 1,
                tab_id: tab,
                pane_id: id,
                workspace: String::new(),
                title: title.into(),
                cwd: String::new(),
            }
        }

        let (mut e, _rx) = test_engine(); // faff_pane = Some(1), open_pane = None (fresh start)
        let t = e.store.create_task("x", 0, Autonomy::Inherit).unwrap();
        e.store.set_pane(t.id, Some(42)).unwrap(); // persisted across the restart
        e.tasks = e.store.list_tasks().unwrap();

        // Agent docked in faff's tab (7) -> recovered as the open session.
        let docked = vec![
            pane(7, 1, "faff"),
            pane(7, 42, "#1 x"),
            pane(9, 99, "unrelated other-tab pane"),
        ];
        assert_eq!(e.detect_open_pane(&docked), Some(42));

        // Agent detached to its own tab (8) -> nothing docked.
        let detached = vec![pane(7, 1, "faff"), pane(8, 42, "#1 x")];
        assert_eq!(e.detect_open_pane(&detached), None);

        // A non-agent pane sharing faff's tab is ignored (not a known task pane).
        let stray = vec![pane(7, 1, "faff"), pane(7, 500, "a shell")];
        assert_eq!(e.detect_open_pane(&stray), None);
    }

    #[test]
    fn reconcile_workspaces_removes_only_gone_ones() {
        use std::path::Path;
        let (mut e, _rx) = test_engine();
        // Task A: workspace still present in jj.
        let a = e.store.create_task("a", 0, Autonomy::Inherit).unwrap();
        e.store
            .set_workspace(a.id, "faf-task-1", Path::new("/nope/1"), "c1", "f1")
            .unwrap();
        // Task B: workspace gone (forgotten externally / integrated + cleaned).
        let b = e.store.create_task("b", 0, Autonomy::Inherit).unwrap();
        e.store
            .set_workspace(b.id, "faf-task-2", Path::new("/nope/2"), "c2", "f2")
            .unwrap();
        e.tasks = e.store.list_tasks().unwrap();

        let live = vec![
            jj::Workspace {
                name: "default".into(),
                change_id: "x".into(),
            },
            jj::Workspace {
                name: "faf-task-1".into(),
                change_id: "y".into(),
            },
        ];
        e.reconcile_workspaces(&live);

        // Present workspace stays; vanished workspace's task is dropped entirely.
        assert!(e.store.try_get_task(a.id).unwrap().is_some());
        assert!(e.store.try_get_task(b.id).unwrap().is_none());
    }

    #[test]
    fn orphaned_workspaces_flags_only_untracked_faff_workspaces() {
        use std::path::Path;
        let (mut e, _rx) = test_engine();
        // A live task; its workspace is `faf-task-<id>` (the invariant prepare_workspace holds).
        let t = e.store.create_task("a", 0, Autonomy::Inherit).unwrap();
        let tracked = format!("faf-task-{}", t.id.0);
        e.store
            .set_workspace(t.id, &tracked, Path::new("/nope"), "c", "f")
            .unwrap();
        e.tasks = e.store.list_tasks().unwrap();

        // A ghost id with no task row (well past the allocated id).
        let ghost = format!("faf-task-{}", t.id.0 + 1000);
        let live = vec![
            // The default workspace is never faff-owned — must be left alone.
            jj::Workspace {
                name: "default".into(),
                change_id: "x".into(),
            },
            // Tracked by task `t` — must be kept.
            jj::Workspace {
                name: tracked.clone(),
                change_id: "y".into(),
            },
            // A faff workspace with no DB row — a ghost to forget.
            jj::Workspace {
                name: ghost.clone(),
                change_id: "z".into(),
            },
        ];

        assert_eq!(e.orphaned_workspaces(&live), vec![ghost]);
    }

    #[test]
    fn orphaned_workspaces_keeps_row_created_before_ws_name_set() {
        // Regression (concurrent-instance race): `create_task` commits the task row
        // *before* `jj workspace add` registers the workspace and *before* `set_workspace`
        // records its `ws_name`. A sibling faff instance whose refresh lands in that window
        // must NOT forget the in-flight workspace. Keying on row existence (not `ws_name`)
        // is what makes the reaper safe here.
        let (e, _rx) = test_engine();
        let t = e
            .store
            .create_task("mid-create", 0, Autonomy::Inherit)
            .unwrap();
        // ws_name deliberately left unset, mirroring the window inside prepare_workspace.
        let live = vec![jj::Workspace {
            name: format!("faf-task-{}", t.id.0),
            change_id: "z".into(),
        }];
        assert!(e.orphaned_workspaces(&live).is_empty());
    }

    // ---- recreating lost instances ----

    /// An engine whose Claude project root is a scratch dir, so transcript presence
    /// (the thing that decides fresh-vs-resume) is controllable in a test.
    fn engine_with_projects(dir: &std::path::Path) -> Engine {
        let (mut e, _rx) = test_engine();
        e.claude_projects = dir.to_path_buf();
        e
    }

    /// A task with a workspace but no pane — the shape faff is left with after an
    /// agent's pane dies. Returns the task and its minted session id.
    fn lost_task(e: &mut Engine, ws: &std::path::Path) -> (Task, String) {
        lost_task_with(e, ws, "do the thing")
    }

    fn lost_task_with(e: &mut Engine, ws: &std::path::Path, prompt: &str) -> (Task, String) {
        let t = e.store.create_task(prompt, 0, Autonomy::Inherit).unwrap();
        e.store
            .set_workspace(t.id, "faf-task-1", ws, "c1", "f1")
            .unwrap();
        let t = e.store.get_task(t.id).unwrap();
        let sid = t.session_id.clone().expect("minted at creation");
        e.tasks = vec![t.clone()];
        (t, sid)
    }

    fn write_transcript(projects: &std::path::Path, ws: &std::path::Path, sid: &str) {
        let dir = projects.join(config::encode_repo_path(ws));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{sid}.jsonl")), "{}\n").unwrap();
    }

    #[test]
    fn a_task_that_never_ran_launches_fresh_and_pins_its_id() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let mut e = engine_with_projects(tmp.path());
        let (t, sid) = lost_task(&mut e, &ws);

        assert_eq!(
            e.task_argv(&t),
            vec!["claude", "--session-id", &sid, "do the thing"]
        );
    }

    #[test]
    fn a_task_with_a_transcript_relaunches_as_a_resume() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let mut e = engine_with_projects(tmp.path());
        let (t, sid) = lost_task(&mut e, &ws);
        write_transcript(tmp.path(), &ws, &sid);

        // No prompt re-sent, and --session-id would be rejected as already in use.
        assert_eq!(e.task_argv(&t), vec!["claude", "--resume", &sid]);
    }

    #[test]
    fn a_task_is_only_resumable_once_it_has_a_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let mut e = engine_with_projects(tmp.path());
        let (t, sid) = lost_task(&mut e, &ws);

        assert!(!e.task_resumable(&t), "nothing written yet");
        write_transcript(tmp.path(), &ws, &sid);
        assert!(e.task_resumable(&t), "pane gone, conversation on disk");
    }

    #[test]
    fn a_running_agent_is_not_treated_as_lost() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let mut e = engine_with_projects(tmp.path());
        let (t, sid) = lost_task(&mut e, &ws);
        write_transcript(tmp.path(), &ws, &sid);
        e.store.set_pane(t.id, Some(42)).unwrap();
        let t = e.store.get_task(t.id).unwrap();

        assert!(!e.task_resumable(&t));
    }

    #[test]
    fn enter_on_a_lost_task_routes_to_revive_instead_of_reporting_no_session() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let mut e = engine_with_projects(tmp.path());
        let (t, sid) = lost_task(&mut e, &ws);
        write_transcript(tmp.path(), &ws, &sid);

        assert_eq!(
            session::decide(e.open_pane, t.pane_id, e.task_resumable(&t)),
            session::Toggle::Revive
        );
    }

    #[test]
    fn a_legacy_task_without_a_minted_id_still_launches() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let mut e = engine_with_projects(tmp.path());
        let (t, _) = lost_task(&mut e, &ws);
        let legacy = Task {
            session_id: None,
            ..t
        };

        assert_eq!(e.task_argv(&legacy), vec!["claude", "do the thing"]);
        assert!(!e.task_resumable(&legacy), "no id, nothing to resume");
    }

    #[test]
    fn view_reports_which_lost_tasks_can_be_revived_or_resumed() {
        // The toolbar reads these sets instead of statting transcripts at render time.
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let mut e = engine_with_projects(tmp.path());
        let (t, sid) = lost_task(&mut e, &ws);

        let v = e.view();
        assert!(v.revivable.contains(&t.id), "workspace intact");
        assert!(!v.resumable.contains(&t.id), "no conversation yet");

        write_transcript(tmp.path(), &ws, &sid);
        assert!(e.view().resumable.contains(&t.id), "conversation kept");

        e.store.set_pane(t.id, Some(42)).unwrap();
        e.tasks = e.store.list_tasks().unwrap();
        let v = e.view();
        assert!(!v.revivable.contains(&t.id) && !v.resumable.contains(&t.id), "alive again");
    }

    #[test]
    fn a_task_that_died_before_its_first_prompt_gets_a_fresh_agent() {
        // `n` spawns an agent sitting at an empty prompt, and claude writes no transcript
        // until the first message — so faff's commonest lost instance has a live
        // workspace and nothing to resume. It still deserves an agent back.
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let mut e = engine_with_projects(tmp.path());
        let (t, sid) = lost_task_with(&mut e, &ws, "");

        assert!(e.task_revivable(&t), "workspace is intact");
        assert!(!e.task_resumable(&t), "nothing written to resume from");
        assert_eq!(e.task_argv(&t), vec!["claude", "--session-id", &sid]);
    }

    #[test]
    fn a_task_with_no_workspace_left_cannot_be_revived() {
        let tmp = tempfile::tempdir().unwrap();
        let e = engine_with_projects(tmp.path());
        let t = e
            .store
            .create_task("orphan", 0, Autonomy::Inherit)
            .unwrap();

        assert!(!e.task_revivable(&t));
    }
}
