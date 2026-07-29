//! The faf dashboard TUI: unified revision view, browse/session modes, and the
//! orchestration actions. See spec §11. Pure logic lives in the submodules
//! (input, session, model); this file is the app state, event loop, and
//! rendering glue.

mod input;
mod model;
mod session;

use crate::domain::{Autonomy, Task, TaskId, TaskStatus, truncate_first_line};
use crate::graph::{self, GraphRow};
use crate::store::Store;
use crate::{config, events, jj, wezterm, workspace};
use anyhow::{Context, Result};
use input::Action;
use ratatui::crossterm::event::{self, Event as CtEvent, KeyEventKind};
use ratatui::crossterm::{execute, terminal};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal};
use std::io::{Stdout, stdout};
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

type Term = Terminal<ratatui::backend::CrosstermBackend<Stdout>>;

/// Fixed display width of the change-id column (jj pads its shortest id to 8).
const ID_W: usize = 8;

/// The prompt `d` injects into an agent's pane: it asks the agent to summarise the end
/// result of its current revision as a short jj description. faff never runs `jj describe`
/// itself — the agent, which holds the live working copy, does.
const DESCRIBE_PROMPT: &str = "Set a short description of what this revision accomplishes. \
     Run: jj describe -m \"<summary>\" where <summary> is a 4-7 word description of the end result.";

/// Entry point for the `Tui` command.
pub fn run(repo: Option<PathBuf>) -> Result<()> {
    let repo = resolve_repo(repo)?;
    let mut app = App::new(repo)?;
    install_panic_hook();
    let mut term = setup_terminal()?;
    let result = app.event_loop(&mut term);
    restore_terminal(&mut term)?;
    result
}

/// Restore the terminal on panic before the default hook prints, so a crash on the
/// render/event hot path never leaves the terminal in raw mode + alternate screen.
fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(stdout(), terminal::LeaveAlternateScreen);
        prev(info);
    }));
}

/// Resolve the HEAD repo root: explicit path, else discover from the cwd via jj.
fn resolve_repo(repo: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(r) = repo {
        return Ok(r);
    }
    // `jj workspace root` from the cwd finds the repo root.
    let out = std::process::Command::new("jj")
        .args(["workspace", "root"])
        .output()
        .context("running jj workspace root")?;
    if out.status.success() {
        let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    std::env::current_dir().context("getting current dir")
}

fn setup_terminal() -> Result<Term> {
    terminal::enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, terminal::EnterAlternateScreen)?;
    let term = Terminal::new(ratatui::backend::CrosstermBackend::new(out))?;
    Ok(term)
}

fn restore_terminal(term: &mut Term) -> Result<()> {
    terminal::disable_raw_mode()?;
    execute!(term.backend_mut(), terminal::LeaveAlternateScreen)?;
    term.show_cursor()?;
    Ok(())
}

struct App {
    repo: PathBuf,
    faf_exe: PathBuf,
    socket: PathBuf,
    db: PathBuf,
    store: Store,
    events_rx: Receiver<events::Event>,
    faf_pane: Option<u64>,

    // View state (owned; recomputed on refresh).
    tasks: Vec<Task>,
    rows: Vec<GraphRow>,
    task_of_node: Vec<Option<TaskId>>,
    task_order: Vec<TaskId>,
    /// Live tasks with no distinct graph node (workspace inlined into HEAD, or a
    /// stale row). Still selectable/removable; shown in a "detached" footer list.
    detached: Vec<TaskId>,
    selected: usize,
    open_pane: Option<u64>,
    /// change_id -> (unique prefix, padding rest) for the id column, from jj.
    id_display: std::collections::HashMap<String, (String, String)>,
    status: String,
    should_quit: bool,
    last_refresh: Instant,
    /// Set when an event arrived; coalesces bursts into a throttled refresh.
    pending_refresh: bool,
    /// Set to a task id after `s` on a *working* agent: the next key confirms the swap
    /// (another `s`) or cancels it (anything else). Guards yanking a live agent's files.
    pending_swap: Option<TaskId>,
    /// Set to `(task, freeze)` after `r`/`R` on a *working* agent: the same key confirms
    /// (a redirect prompt is disruptive mid-turn), anything else cancels. `freeze`
    /// records which of `r` (true) / `R` (false) armed it.
    pending_rebase: Option<(TaskId, bool)>,
    /// Set to a task id after `d` on a *working* agent: a second `d` confirms (injecting a
    /// prompt mid-turn is disruptive, and a description is premature until work settles),
    /// anything else cancels.
    pending_describe: Option<TaskId>,
}

impl App {
    fn new(repo: PathBuf) -> Result<App> {
        let db = config::db_path(&repo)?;
        let store = Store::open(&db)?;
        let socket = events::socket_path(&repo);
        let events_rx = events::spawn_listener(&socket)?;
        let faf_exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("faf"));
        let faf_pane = std::env::var("WEZTERM_PANE")
            .ok()
            .and_then(|s| s.parse().ok());

        let mut app = App {
            repo,
            faf_exe,
            socket,
            db,
            store,
            events_rx,
            faf_pane,
            tasks: Vec::new(),
            rows: Vec::new(),
            task_of_node: Vec::new(),
            task_order: Vec::new(),
            detached: Vec::new(),
            selected: 0,
            open_pane: None,
            id_display: std::collections::HashMap::new(),
            status: "ready".to_string(),
            should_quit: false,
            last_refresh: Instant::now(),
            pending_refresh: false,
            pending_swap: None,
            pending_rebase: None,
            pending_describe: None,
        };
        app.refresh();
        Ok(app)
    }

    fn event_loop(&mut self, term: &mut Term) -> Result<()> {
        // Rebuilding the graph shells out to jj/wezterm; throttle it. Idle cadence is
        // ~1s; a pending event refreshes sooner but no more often than min_gap, so a
        // chatty agent can't drive unbounded subprocess churn.
        let idle = Duration::from_millis(1000);
        let min_gap = Duration::from_millis(400);
        while !self.should_quit {
            term.draw(|f| self.render(f))?;
            if self.drain_channels() {
                self.pending_refresh = true;
            }
            if event::poll(Duration::from_millis(250))?
                && let CtEvent::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                self.handle_key(key);
            }
            let elapsed = self.last_refresh.elapsed();
            if elapsed >= idle || (self.pending_refresh && elapsed >= min_gap) {
                self.refresh();
            }
        }
        Ok(())
    }

    /// Drain the socket nudge channel. Hook events are persisted to the store by
    /// `report-event` itself, so socket messages are only refresh nudges here (not
    /// re-applied). Returns whether anything arrived.
    fn drain_channels(&mut self) -> bool {
        let mut any = false;
        while self.events_rx.try_recv().is_ok() {
            any = true; // report-event already wrote the store; just nudge a refresh
        }
        any
    }

    fn handle_key(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        let action = input::map_key(key);
        // A pending swap-confirmation swallows the next key: `s` confirms, anything
        // else cancels (so a live agent's files are only yanked on a deliberate re-press).
        if let Some(id) = self.pending_swap.take() {
            if action == Action::Swap {
                self.perform_swap(id);
            } else {
                self.status = "swap cancelled".to_string();
            }
            return;
        }
        // A pending rebase-confirmation swallows the next key: the *same* key (`r` or
        // `R`) confirms, anything else cancels (a redirect prompt into a live agent is
        // only sent on a deliberate re-press).
        if let Some((id, freeze)) = self.pending_rebase.take() {
            let confirmed = (freeze && action == Action::Rebase)
                || (!freeze && action == Action::RebaseParent);
            if confirmed {
                self.perform_rebase(id, freeze);
            } else {
                self.status = "rebase cancelled".to_string();
            }
            return;
        }
        // A pending describe-confirmation swallows the next key: a second `d` confirms,
        // anything else cancels (the describe prompt is only injected on a deliberate
        // re-press).
        if let Some(id) = self.pending_describe.take() {
            if action == Action::Describe {
                self.perform_describe(id);
            } else {
                self.status = "describe cancelled".to_string();
            }
            return;
        }
        match action {
            Action::Quit => self.should_quit = true,
            Action::Up => self.move_selection(-1),
            Action::Down => self.move_selection(1),
            Action::NewTask => self.new_task(),
            Action::ToggleSession => self.toggle_session(),
            Action::Remove => self.remove_selected(false),
            Action::RemoveDiscard => self.remove_selected(true),
            Action::Swap => self.swap_selected(),
            Action::Snapshot => self.snapshot_selected(),
            Action::Rebase => self.rebase_selected(true),
            Action::RebaseParent => self.rebase_selected(false),
            Action::Describe => self.describe_selected(),
            Action::None => {}
        }
    }

    fn selected_task(&self) -> Option<Task> {
        let id = self.task_order.get(self.selected)?;
        self.tasks.iter().find(|t| &t.id == id).cloned()
    }

    /// The task whose agent pane is currently docked beside faf (the focused session).
    fn open_task_id(&self) -> Option<TaskId> {
        let open = self.open_pane?;
        self.tasks
            .iter()
            .find(|t| t.pane_id == Some(open))
            .map(|t| t.id)
    }

    fn move_selection(&mut self, delta: isize) {
        if self.task_order.is_empty() {
            return;
        }
        let n = self.task_order.len() as isize;
        let cur = self.selected as isize;
        self.selected = ((cur + delta).rem_euclid(n)) as usize;
    }

    // ---- orchestration actions ----

    /// `n`: create an empty task, fork its workspace, spawn a bare `claude`, and open
    /// it beside faf so the user types their task straight into the session. The task
    /// prompt (and title) are captured later from the first UserPromptSubmit hook.
    fn new_task(&mut self) {
        match self.try_new_task() {
            Ok(id) => {
                self.status = format!("new task #{id} — type your task in the pane");
            }
            Err(e) => self.status = format!("new task failed: {e}"),
        }
        self.refresh();
    }

    fn try_new_task(&mut self) -> Result<TaskId> {
        let faf = self
            .faf_pane
            .context("run faf inside WezTerm to spawn agents")?;
        let task = self.store.create_task("", 0, Autonomy::Inherit)?;
        // Roll the row back if workspace prep fails (no invisible zombie).
        if let Err(e) = self.prepare_workspace(task.id, "") {
            let _ = self.store.delete_task(task.id);
            return Err(e);
        }
        let t = self.store.get_task(task.id)?;
        match self.spawn_claude(&t) {
            Ok(pane) => {
                let _ = self.store.set_pane(task.id, Some(pane));
                // Awaiting the user's first prompt — literally needs their input.
                let _ = self.store.update_status(task.id, TaskStatus::NeedsInput);
                // Dock it beside faf (detaching any already-open session first) and
                // focus it — a new task lands the user in the agent to type the task.
                self.open_session(faf, pane, true);
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

    fn prepare_workspace(&self, id: TaskId, prompt: &str) -> Result<()> {
        let slug = config::slugify(prompt, 4);
        let name = format!("faf-task-{}", id.0);
        let path = config::task_workspace_dir(&self.repo, id.0, &slug)?;
        let ws = workspace::create(&self.repo, &name, &path)?;
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
        let _ = workspace::seed_memory(&workspace::claude_projects_dir(), &self.repo, &ws.path);
        let _ = workspace::write_hooks(&ws.path, id.0, &self.faf_exe, &self.socket, &self.db);
        let _ = workspace::trust_workspace(&ws.path);
        Ok(())
    }

    /// Launch a `claude` pane in the task's workspace and title its tab. A task created
    /// via `n` spawns bare so the user types the task live; the prompt is passed as the
    /// initial message only if the task already has one. `--permission-mode` is passed
    /// only for an explicit override — otherwise the user's own default (e.g.
    /// `permissions.defaultMode=auto`) is inherited.
    fn spawn_claude(&self, t: &Task) -> Result<u64> {
        let ws_path = t.ws_path.clone().context("task has no workspace")?;
        let mut prog: Vec<&str> = vec!["claude"];
        if let Some(mode) = t.autonomy.permission_mode() {
            prog.push("--permission-mode");
            prog.push(mode);
        }
        if !t.prompt.is_empty() {
            prog.push(&t.prompt);
        }
        let pane = wezterm::spawn(&ws_path, &prog)?;
        let _ = wezterm::set_tab_title(pane, &format!("#{}", t.id.0));
        // `wezterm cli spawn` activates the new tab, stealing focus from faf. Return
        // focus to faf so spawning an agent never yanks the user out of the TUI (a new
        // task re-focuses its own pane afterward — see open_session).
        if let Some(faf) = self.faf_pane {
            let _ = wezterm::activate_pane(faf);
        }
        Ok(pane)
    }

    /// Dock `pane` beside faf, ensuring only one session is ever docked: any
    /// currently-open session is detached first. Idempotent if `pane` is already open.
    /// `focus` decides where the cursor lands: a new task focuses its fresh agent so
    /// the user can type the task straight away; every other dock leaves focus on faf
    /// (the user swaps with their own WezTerm keybinds).
    fn open_session(&mut self, faf: u64, pane: u64, focus: bool) {
        if let Some(prev) = self.open_pane
            && prev != pane
        {
            let _ = wezterm::detach(prev);
        }
        if wezterm::open_beside(faf, pane).is_ok() {
            self.open_pane = Some(pane);
            // open_beside activates the moved pane; set focus explicitly either way.
            let _ = wezterm::activate_pane(if focus { pane } else { faf });
        }
    }

    fn toggle_session(&mut self) {
        let selected_pane = self.selected_task().and_then(|t| t.pane_id);
        let Some(faf) = self.faf_pane else {
            self.status = "no WEZTERM_PANE; run faf inside WezTerm".to_string();
            return;
        };
        match session::decide(self.open_pane, selected_pane) {
            session::Toggle::Nothing => self.status = "no session for this task".to_string(),
            // Open and Retarget both route through open_session (which detaches any
            // currently-docked session first).
            session::Toggle::Open(p) | session::Toggle::Retarget { open: p, .. } => {
                // Docking to view an existing agent keeps focus on faf.
                self.open_session(faf, p, false)
            }
            session::Toggle::Detach(p) => {
                if wezterm::detach(p).is_ok() {
                    self.open_pane = None;
                    // Ejecting the pane to a new tab activates it; stay on faf.
                    let _ = wezterm::activate_pane(faf);
                }
            }
        }
    }

    /// Remove the selected task: kill its pane, tear down its jj workspace, and drop the
    /// row from the store. There is no archive/history — a removed task is gone.
    ///
    /// `x` (`discard_revision = false`) preserves the task's real work as ordinary jj
    /// history; `X`/Shift+x (`discard_revision = true`) abandons the revision too,
    /// throwing the work away (see `workspace::teardown_discarding_revision`).
    fn remove_selected(&mut self, discard_revision: bool) {
        let Some(t) = self.selected_task() else {
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
        let _ = self.store.delete_task(t.id);
        self.status = if discard_revision {
            format!("removed #{} and discarded its revision", t.id.0)
        } else {
            format!("removed #{}", t.id.0)
        };
        self.refresh();
    }

    /// `s`: swap the default workspace's `@` with the selected agent's revision (a
    /// literal trade of checkouts — see `workspace::swap`). If the agent is actively
    /// working, the first `s` only arms a confirmation (its files are about to change
    /// underneath it); a second `s` (via `handle_key`) goes through.
    fn swap_selected(&mut self) {
        let Some(t) = self.selected_task() else {
            return;
        };
        if t.ws_name.is_none() || t.ws_path.is_none() {
            self.status = "no workspace to swap for this task".to_string();
            return;
        }
        if t.status == TaskStatus::Working {
            self.pending_swap = Some(t.id);
            self.status = format!(
                "#{} is working — press s to confirm swap, any other key cancels",
                t.id.0
            );
            return;
        }
        self.perform_swap(t.id);
    }

    /// Run the swap for task `id` (already validated / confirmed) and refresh.
    fn perform_swap(&mut self, id: TaskId) {
        let Some(t) = self.tasks.iter().find(|t| t.id == id).cloned() else {
            return;
        };
        let (Some(name), Some(path)) = (t.ws_name.clone(), t.ws_path.clone()) else {
            self.status = "no workspace to swap for this task".to_string();
            return;
        };
        match workspace::swap(&self.repo, &name, &path) {
            Ok(new_rev) => {
                // Keep the recorded revision honest (the graph rebuilds from jj anyway).
                let _ = self.store.set_ws_change_id(id, &new_rev);
                self.status = format!("swapped @ ⇄ #{}", id.0);
            }
            Err(e) => self.status = format!("swap failed: {e}"),
        }
        self.refresh();
    }

    /// `S`: snapshot the selected agent's workspace so edits from an agent that hasn't
    /// run a jj command land in its `@` and show up in the graph.
    fn snapshot_selected(&mut self) {
        let Some(t) = self.selected_task() else {
            return;
        };
        let Some(path) = t.ws_path.clone() else {
            self.status = "no workspace to snapshot for this task".to_string();
            return;
        };
        match workspace::snapshot(&path) {
            Ok(()) => self.status = format!("snapshotted #{}", t.id.0),
            Err(e) => self.status = format!("snapshot failed: {e}"),
        }
        self.refresh();
    }

    /// `r` / `R`: refresh the selected agent onto a newer base. faff computes the base
    /// (the same fork-point recipe `create` uses) and injects a prompt telling the agent
    /// to rebase *itself* — faff never runs `jj rebase`. `freeze == true` (`r`) freezes
    /// your WIP first like `create`; `false` (`R`) uses your parent line, read-only. On a
    /// working agent the first press only arms a confirmation (a redirect mid-turn is
    /// disruptive); a second, matching press goes through (via `handle_key`).
    fn rebase_selected(&mut self, freeze: bool) {
        let Some(t) = self.selected_task() else {
            return;
        };
        if t.ws_name.is_none() {
            self.status = "no workspace to rebase for this task".to_string();
            return;
        }
        if t.pane_id.is_none() {
            self.status = "no live pane to send the rebase prompt to".to_string();
            return;
        }
        // The UserPromptSubmit hook captures the *first* prompt as the task title. Before
        // the user has sent one, an injected rebase prompt would become that title — so
        // hold off until the task has a real prompt of its own.
        if t.prompt.is_empty() {
            self.status = "send the task its first prompt before rebasing".to_string();
            return;
        }
        if t.status == TaskStatus::Working {
            self.pending_rebase = Some((t.id, freeze));
            let key = if freeze { "r" } else { "R" };
            self.status = format!(
                "#{} is working — press {key} to confirm rebase, any other key cancels",
                t.id.0
            );
            return;
        }
        self.perform_rebase(t.id, freeze);
    }

    /// Compute the new base and inject the rebase prompt into the agent's pane (already
    /// validated / confirmed). No-op when the agent is already on the latest base.
    fn perform_rebase(&mut self, id: TaskId, freeze: bool) {
        let Some(t) = self.tasks.iter().find(|t| t.id == id).cloned() else {
            return;
        };
        let (Some(name), Some(pane)) = (t.ws_name.clone(), t.pane_id) else {
            self.status = "no workspace/pane to rebase for this task".to_string();
            return;
        };
        match workspace::refresh(&self.repo, &name, freeze) {
            Ok(workspace::Refresh::AlreadyFresh) => {
                self.status = format!("#{} is already on the latest base", id.0);
            }
            Ok(workspace::Refresh::Rebase { prompt, .. }) => match wezterm::send_text(pane, &prompt)
            {
                Ok(()) => self.status = format!("sent rebase to #{}", id.0),
                Err(e) => self.status = format!("rebase send failed: {e}"),
            },
            Err(e) => self.status = format!("rebase failed: {e}"),
        }
        self.refresh();
    }

    /// `d`: ask the selected agent to describe its own revision. faff never runs `jj
    /// describe` itself — it injects a prompt telling the agent to set a short 4-7 word
    /// description of the end result, so the log row shows what the revision actually did
    /// (not just the prompt-derived label). On a working agent the first
    /// press only arms a confirmation (a mid-turn prompt is disruptive, and a description
    /// is premature before the work settles); a second `d` goes through (via `handle_key`).
    fn describe_selected(&mut self) {
        let Some(t) = self.selected_task() else {
            return;
        };
        if t.ws_name.is_none() {
            self.status = "no workspace to describe for this task".to_string();
            return;
        }
        if t.pane_id.is_none() {
            self.status = "no live pane to send the describe prompt to".to_string();
            return;
        }
        // Like rebase: the first prompt is captured as the task title, so an injected
        // describe prompt sent before the user's own first prompt would become that title.
        // Hold off until the task has a real prompt (and thus some work to describe).
        if t.prompt.is_empty() {
            self.status = "send the task its first prompt before describing".to_string();
            return;
        }
        if t.status == TaskStatus::Working {
            self.pending_describe = Some(t.id);
            self.status = format!(
                "#{} is working — press d to confirm describe, any other key cancels",
                t.id.0
            );
            return;
        }
        self.perform_describe(t.id);
    }

    /// Inject the describe prompt into the agent's pane (already validated / confirmed).
    fn perform_describe(&mut self, id: TaskId) {
        let Some(t) = self.tasks.iter().find(|t| t.id == id).cloned() else {
            return;
        };
        let Some(pane) = t.pane_id else {
            self.status = "no pane to describe for this task".to_string();
            return;
        };
        match wezterm::send_text(pane, DESCRIBE_PROMPT) {
            Ok(()) => self.status = format!("sent describe to #{}", id.0),
            Err(e) => self.status = format!("describe send failed: {e}"),
        }
        self.refresh();
    }

    // ---- refresh (recompute view from store + jj) ----

    fn refresh(&mut self) {
        self.pending_refresh = false;
        // Preserve the selected task by identity across the rebuild.
        let prev_selected = self.task_order.get(self.selected).copied();

        self.tasks = self.store.list_tasks().unwrap_or_default();

        // Fetch the authoritative jj workspace list once (shared by reconcile + graph).
        let mut ws_list = jj::workspace_list(&self.repo).ok();

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

        // Recover which agent (if any) is already docked beside faf. Derived from the
        // live layout each refresh, so a restart — where `open_pane` starts `None` —
        // doesn't leave faf blind to an already-docked session and re-open (double) it.
        if let Some(p) = &panes {
            self.open_pane = self.detect_open_pane(p);
        }

        // Build the revision graph from the (already-fetched) workspace list.
        if let Some(ws) = &ws_list {
            match self.build_rows(ws) {
                Ok((rows, task_of, id_display)) => {
                    self.rows = rows;
                    self.task_of_node = task_of;
                    self.id_display = id_display;
                }
                Err(e) => self.status = format!("jj error: {e}"),
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

        self.selected = prev_selected
            .and_then(|id| self.task_order.iter().position(|x| *x == id))
            .unwrap_or_else(|| self.selected.min(self.task_order.len().saturating_sub(1)));
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

    /// Derive which agent pane (if any) is currently docked beside faf: a known task
    /// pane that shares faf's WezTerm tab (that is how `open_beside` docks it). This
    /// lets faf recover the docked session after a restart — when `open_pane` starts
    /// `None` — instead of treating Enter as a fresh open and spawning a duplicate
    /// split. An agent detached to its own tab, or a non-agent pane, is not "open".
    fn detect_open_pane(&self, panes: &[wezterm::Pane]) -> Option<u64> {
        let faf = self.faf_pane?;
        let faf_tab = panes.iter().find(|p| p.pane_id == faf)?.tab_id;
        let agent_panes: std::collections::HashSet<u64> =
            self.tasks.iter().filter_map(|t| t.pane_id).collect();
        panes
            .iter()
            .find(|p| p.tab_id == faf_tab && p.pane_id != faf && agent_panes.contains(&p.pane_id))
            .map(|p| p.pane_id)
    }

    /// Drop tasks whose jj workspace has vanished (integrated + cleaned, or forgotten
    /// outside faf): the workspace is gone, so the task is done — remove it. Keeps the
    /// active list honest. Only runs with an authoritative workspace list.
    fn reconcile_workspaces(&self, workspaces: &[jj::Workspace]) {
        let live: std::collections::HashSet<&str> =
            workspaces.iter().map(|w| w.name.as_str()).collect();
        for t in &self.tasks {
            if let Some(name) = &t.ws_name
                && !live.contains(name.as_str())
            {
                if let Some(p) = &t.ws_path {
                    let _ = std::fs::remove_dir_all(p); // best-effort dir cleanup
                }
                let _ = self.store.delete_task(t.id);
            }
        }
    }

    /// faff-owned jj workspaces (`faf-task-*`) that no live task tracks — ghosts left by
    /// a remove or swap that didn't fully unregister. Pure: returns the names to forget,
    /// with no side effects (the effectful wrapper does the forgetting).
    fn orphaned_workspaces(&self, workspaces: &[jj::Workspace]) -> Vec<String> {
        let tracked: std::collections::HashSet<&str> =
            self.tasks.iter().filter_map(|t| t.ws_name.as_deref()).collect();
        workspaces
            .iter()
            .map(|w| w.name.as_str())
            .filter(|name| name.starts_with("faf-task-") && !tracked.contains(name))
            .map(str::to_string)
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
    )> {
        let mut heads: Vec<String> = workspaces.iter().map(|w| w.change_id.clone()).collect();
        heads.push("@".to_string());
        let revset = format!("ancestors({}, 25)", heads.join(" | "));
        let mut revs = jj::log(&self.repo, &revset)?;
        // HEAD's `@` leads the log on lane 0; every agent is lifted to sit directly above
        // the trunk revision it forked from, so each folds to a one-row `├─●` stub.
        model::order_by_fork_point(&mut revs);
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
        Ok((rows, task_of, id_display))
    }

    // ---- rendering ----

    fn render(&self, f: &mut Frame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(f.area());
        self.render_header(f, chunks[0]);
        self.render_body(f, chunks[1]);
        self.render_footer(f, chunks[2]);
    }

    fn render_header(&self, f: &mut Frame, area: Rect) {
        let working = self
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Working)
            .count();
        let session = match self.open_task_id() {
            Some(id) => format!(" · ▶ #{}", id.0),
            None => String::new(),
        };
        let text = format!(
            " faf · {} · {working} working{session} ",
            self.repo
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
        );
        f.render_widget(
            Paragraph::new(text).style(Style::default().add_modifier(Modifier::REVERSED)),
            area,
        );
    }

    fn render_body(&self, f: &mut Frame, area: Rect) {
        // The revision graph is the whole body: when a session is docked WezTerm owns
        // the right half (the real claude pane) beside faf's narrowed area, and when
        // browsing the graph simply uses the full width.
        self.render_graph(f, area);
    }

    fn render_graph(&self, f: &mut Frame, area: Rect) {
        let selected = self.task_order.get(self.selected).copied();
        let open = self.open_task_id();
        // Text width inside the block (its RIGHT border takes one column). Labels are
        // clipped to fit this — and only when they overflow — so the log fills the pane
        // and re-fits whenever docking/detaching a session resizes faff.
        let text_w = area.width.saturating_sub(1) as usize;
        let mut lines: Vec<Line> = Vec::with_capacity(self.rows.len());
        // Width of the "[abcdefgh] " id column, so continuation lines align under content.
        let id_col = ID_W + 3;
        // Pad every gutter to one uniform width so the [id] column, block indicator, and
        // description line up into straight columns regardless of branch depth — a folded
        // agent stub `├─●` is wider than a trunk glyph `○`, and without this the whole
        // right-hand block shifts sideways by the difference. The 2-space separator that
        // was previously appended per-row is folded into this width. (Gutter cells are all
        // single-column box-drawing chars, so char count is the display width.)
        let gutter_w = self
            .rows
            .iter()
            .map(|r| r.gutter.chars().count())
            .max()
            .unwrap_or(0)
            + 2;
        for (i, row) in self.rows.iter().enumerate() {
            let row_task = self.task_of_node.get(i).copied().flatten();
            let is_sel = selected.is_some() && row_task == selected;
            let base = if is_sel {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            // Reserve columns for the trailing docked-session marker on rows that show it,
            // so truncating the content never pushes the `▶` off the pane.
            let show_marker = row_task.is_some() && row_task == open;
            let marker_w = if show_marker { 3 } else { 0 };
            let mut spans: Vec<Span> = Vec::new();
            match &row.change_id {
                // Node row: gutter + [id] + content, with the unique prefix highlighted.
                Some(cid) => {
                    // Color the working-copy glyph `@` green (bold), like jj log; every
                    // other gutter character keeps the base style. Only the `@` node's
                    // commit row ever carries `@`, and there is at most one.
                    let gutter = format!("{:<gutter_w$}", row.gutter);
                    match gutter.find('@') {
                        Some(at) => {
                            spans.push(Span::styled(gutter[..at].to_string(), base));
                            spans.push(Span::styled(
                                "@",
                                base.fg(Color::Green).add_modifier(Modifier::BOLD),
                            ));
                            spans.push(Span::styled(gutter[at + 1..].to_string(), base));
                        }
                        None => spans.push(Span::styled(gutter, base)),
                    }
                    let (prefix, rest) = self
                        .id_display
                        .get(cid)
                        .cloned()
                        .unwrap_or_else(|| (cid.chars().take(ID_W).collect(), String::new()));
                    let id_w = 1 + prefix.chars().count() + rest.chars().count() + 2;
                    spans.push(Span::styled("[", base.fg(Color::DarkGray)));
                    spans.push(Span::styled(
                        prefix,
                        base.fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    ));
                    spans.push(Span::styled(rest, base.fg(Color::DarkGray)));
                    spans.push(Span::styled("] ", base.fg(Color::DarkGray)));
                    let avail = text_w.saturating_sub(gutter_w + id_w + marker_w);
                    spans.push(Span::styled(truncate_first_line(&row.content, avail), base));
                    // Marker for the currently-docked (focused) session.
                    if show_marker {
                        spans.push(Span::styled(
                            "  ▶",
                            base.fg(Color::Green).add_modifier(Modifier::BOLD),
                        ));
                    }
                }
                // Link row (no content): gutter only.
                None if row.content.is_empty() => {
                    spans.push(Span::styled(row.gutter.clone(), base));
                }
                // Continuation row: gutter + id-column padding + content (aligned).
                None => {
                    let pad = format!("{:<gutter_w$}{}", row.gutter, " ".repeat(id_col));
                    let avail = text_w.saturating_sub(pad.chars().count() + marker_w);
                    spans.push(Span::styled(pad, base));
                    spans.push(Span::styled(truncate_first_line(&row.content, avail), base));
                    // The combined HEAD+agent node hangs its task (and so its docked
                    // marker) on the agent's continuation line, not the HEAD header row.
                    if show_marker {
                        spans.push(Span::styled(
                            "  ▶",
                            base.fg(Color::Green).add_modifier(Modifier::BOLD),
                        ));
                    }
                }
            }
            lines.push(Line::from(spans));
        }
        // Detached tasks (workspace inlined into HEAD, or a stale row) have no
        // distinct node — list them below so they stay visible and selectable.
        if !self.detached.is_empty() {
            lines.push(Line::from(Span::styled(
                "── detached (integrated / no node) ──",
                Style::default().fg(Color::DarkGray),
            )));
            for id in &self.detached {
                if let Some(t) = self.tasks.iter().find(|t| &t.id == id) {
                    let (icon, _) = model::status_label(t.status);
                    let show_marker = open == Some(*id);
                    let marker_w = if show_marker { 3 } else { 0 };
                    let text = format!("· #{} {} {icon}", t.id.0, t.label());
                    let text = truncate_first_line(&text, text_w.saturating_sub(marker_w));
                    let style = if selected == Some(*id) {
                        Style::default().add_modifier(Modifier::REVERSED)
                    } else {
                        Style::default()
                    };
                    let mut spans = vec![Span::styled(text, style)];
                    if show_marker {
                        spans.push(Span::styled(
                            "  ▶",
                            style.fg(Color::Green).add_modifier(Modifier::BOLD),
                        ));
                    }
                    lines.push(Line::from(spans));
                }
            }
        }
        let block = Block::default().borders(Borders::RIGHT).title("revisions");
        f.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn render_footer(&self, f: &mut Frame, area: Rect) {
        let selected_pane = self.selected_task().and_then(|t| t.pane_id);
        let enter = if self.open_pane.is_some() && self.open_pane == selected_pane {
            "[↵]detach"
        } else {
            "[↵]open"
        };
        let keys = format!(
            " [n]ew {enter} [s]wap [S]napshot [r]ebase [d]escribe [x]remove [X]remove+drop [q]uit   {}",
            self.status
        );
        f.render_widget(
            Paragraph::new(keys).style(Style::default().fg(Color::DarkGray)),
            area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    // Build a minimal App with an in-memory store (no jj/wezterm needed).
    fn test_app() -> App {
        let (_ev_tx, events_rx) = std::sync::mpsc::channel();
        App {
            repo: PathBuf::from("/tmp/repo"),
            faf_exe: PathBuf::from("/bin/faf"),
            socket: PathBuf::from("/tmp/faf.sock"),
            db: PathBuf::from("/tmp/faf.db"),
            store: Store::open_memory().unwrap(),
            events_rx,
            faf_pane: Some(1),
            tasks: Vec::new(),
            rows: Vec::new(),
            task_of_node: Vec::new(),
            task_order: Vec::new(),
            detached: Vec::new(),
            selected: 0,
            open_pane: None,
            id_display: std::collections::HashMap::new(),
            status: "ready".into(),
            should_quit: false,
            pending_refresh: false,
            pending_swap: None,
            pending_rebase: None,
            pending_describe: None,
            last_refresh: Instant::now(),
        }
    }

    use ratatui::crossterm::event::KeyCode;
    fn key(code: KeyCode) -> ratatui::crossterm::event::KeyEvent {
        ratatui::crossterm::event::KeyEvent {
            code,
            modifiers: ratatui::crossterm::event::KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: ratatui::crossterm::event::KeyEventState::NONE,
        }
    }

    #[test]
    fn swap_on_working_agent_arms_then_cancels() {
        // `s` on a working agent must not shell out to jj; it only arms a confirmation.
        // A non-`s` key then cancels and clears the pending state.
        let mut app = test_app();
        let t = app.store.create_task("x", 0, Autonomy::Inherit).unwrap();
        app.store
            .set_workspace(t.id, "faf-task-1", std::path::Path::new("/nope/ws"), "c1", "f1")
            .unwrap();
        app.store.update_status(t.id, TaskStatus::Working).unwrap();
        app.tasks = app.store.list_tasks().unwrap();
        app.task_order = vec![t.id];
        app.selected = 0;

        app.swap_selected();
        assert_eq!(app.pending_swap, Some(t.id), "first s arms the confirmation");
        assert!(app.status.contains("press s to confirm"));

        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.pending_swap, None, "a non-s key cancels");
        assert_eq!(app.status, "swap cancelled");
    }

    #[test]
    fn rebase_on_working_agent_arms_then_cancels() {
        // `r` on a working agent must not send anything; it only arms a confirmation.
        // A non-matching key then cancels and clears the pending state.
        let mut app = test_app();
        let t = app.store.create_task("x", 0, Autonomy::Inherit).unwrap();
        app.store
            .set_workspace(t.id, "faf-task-1", std::path::Path::new("/nope/ws"), "c1", "f1")
            .unwrap();
        app.store.set_pane(t.id, Some(42)).unwrap();
        app.store.update_status(t.id, TaskStatus::Working).unwrap();
        app.tasks = app.store.list_tasks().unwrap();
        app.task_order = vec![t.id];
        app.selected = 0;

        app.rebase_selected(true);
        assert_eq!(
            app.pending_rebase,
            Some((t.id, true)),
            "first r arms the confirmation"
        );
        assert!(app.status.contains("press r to confirm"));

        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.pending_rebase, None, "a non-r key cancels");
        assert_eq!(app.status, "rebase cancelled");
    }

    #[test]
    fn rebase_before_first_prompt_is_blocked() {
        // Before the task has a prompt of its own, an injected rebase prompt would be
        // captured as the task's title — so `r`/`R` refuses until a real prompt exists.
        let mut app = test_app();
        let t = app.store.create_task("", 0, Autonomy::Inherit).unwrap();
        app.store
            .set_workspace(t.id, "faf-task-1", std::path::Path::new("/nope/ws"), "c1", "f1")
            .unwrap();
        app.store.set_pane(t.id, Some(42)).unwrap();
        app.tasks = app.store.list_tasks().unwrap();
        app.task_order = vec![t.id];
        app.selected = 0;

        app.rebase_selected(true);
        assert_eq!(app.pending_rebase, None, "must not arm without a prompt");
        assert!(app.status.contains("first prompt"), "status: {}", app.status);
    }

    #[test]
    fn describe_on_working_agent_arms_then_cancels() {
        // `d` on a working agent must not send anything; it only arms a confirmation.
        // A non-`d` key then cancels and clears the pending state.
        let mut app = test_app();
        let t = app.store.create_task("x", 0, Autonomy::Inherit).unwrap();
        app.store
            .set_workspace(t.id, "faf-task-1", std::path::Path::new("/nope/ws"), "c1", "f1")
            .unwrap();
        app.store.set_pane(t.id, Some(42)).unwrap();
        app.store.update_status(t.id, TaskStatus::Working).unwrap();
        app.tasks = app.store.list_tasks().unwrap();
        app.task_order = vec![t.id];
        app.selected = 0;

        app.describe_selected();
        assert_eq!(
            app.pending_describe,
            Some(t.id),
            "first d arms the confirmation"
        );
        assert!(app.status.contains("press d to confirm"));

        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.pending_describe, None, "a non-d key cancels");
        assert_eq!(app.status, "describe cancelled");
    }

    #[test]
    fn describe_before_first_prompt_is_blocked() {
        // Before the task has a prompt of its own, an injected describe prompt would be
        // captured as the task's title — so `d` refuses until a real prompt exists.
        let mut app = test_app();
        let t = app.store.create_task("", 0, Autonomy::Inherit).unwrap();
        app.store
            .set_workspace(t.id, "faf-task-1", std::path::Path::new("/nope/ws"), "c1", "f1")
            .unwrap();
        app.store.set_pane(t.id, Some(42)).unwrap();
        app.tasks = app.store.list_tasks().unwrap();
        app.task_order = vec![t.id];
        app.selected = 0;

        app.describe_selected();
        assert_eq!(app.pending_describe, None, "must not arm without a prompt");
        assert!(app.status.contains("first prompt"), "status: {}", app.status);
    }

    #[test]
    fn header_and_footer_render_without_panicking() {
        let mut app = test_app();
        app.tasks = vec![
            app.store
                .create_task("do a thing", 0, Autonomy::AcceptEdits)
                .unwrap(),
        ];
        let backend = TestBackend::new(80, 20);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| app.render(f)).unwrap();
        // If we got here, layout + widgets rendered without panic.
    }

    #[test]
    fn reconcile_heals_orphaned_panes_to_idle() {
        let app = test_app();
        // Task A: Working with a pane that is still alive.
        let a = app
            .store
            .create_task("a", 0, Autonomy::AcceptEdits)
            .unwrap();
        app.store.set_pane(a.id, Some(10)).unwrap();
        app.store.update_status(a.id, TaskStatus::Working).unwrap();
        // Task B: Working with a pane that has vanished.
        let b = app
            .store
            .create_task("b", 0, Autonomy::AcceptEdits)
            .unwrap();
        app.store.set_pane(b.id, Some(99)).unwrap();
        app.store.update_status(b.id, TaskStatus::Working).unwrap();
        // Task C: Working but never got a pane (a failed spawn).
        let c = app
            .store
            .create_task("c", 0, Autonomy::AcceptEdits)
            .unwrap();
        app.store.update_status(c.id, TaskStatus::Working).unwrap();

        let mut app = app;
        app.tasks = app.store.list_tasks().unwrap();

        // Only pane 10 is alive.
        let panes = vec![wezterm::Pane {
            window_id: 1,
            tab_id: 1,
            pane_id: 10,
            workspace: String::new(),
            title: String::new(),
            cwd: String::new(),
        }];
        app.reconcile_panes(&panes);

        // A stays Working (pane alive); B healed (dead pane); C healed (no pane).
        assert_eq!(
            app.store.get_task(a.id).unwrap().status,
            TaskStatus::Working
        );
        let b2 = app.store.get_task(b.id).unwrap();
        assert_eq!(b2.status, TaskStatus::Idle);
        assert_eq!(b2.pane_id, None);
        assert_eq!(app.store.get_task(c.id).unwrap().status, TaskStatus::Idle);
    }

    #[test]
    fn graph_renders_id_column_with_prefix() {
        let mut app = test_app();
        app.rows = vec![graph::GraphRow {
            gutter: "@".into(),
            content: "(no description set)".into(),
            node_index: Some(0),
            change_id: Some("abcd1234efgh".into()),
        }];
        app.task_of_node = vec![None];
        app.id_display = std::collections::HashMap::from([(
            "abcd1234efgh".to_string(),
            ("ab".to_string(), "cd1234".to_string()),
        )]);

        let backend = TestBackend::new(80, 10);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| app.render(f)).unwrap();
        let text: String = term
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            text.contains("[abcd1234]"),
            "id column with padded id: {text:?}"
        );
        assert!(text.contains("(no description set)"));
        // The working-copy glyph `@` is rendered green, like jj log.
        let buf = term.backend().buffer();
        let at = buf
            .content
            .iter()
            .find(|c| c.symbol() == "@")
            .expect("@ glyph rendered");
        assert_eq!(at.fg, Color::Green, "working-copy `@` is green");
    }

    #[test]
    fn graph_aligns_id_and_indicator_columns_across_gutter_widths() {
        // A trunk node (`○`, a 1-column gutter) and a folded agent stub (`├─●`, a
        // 3-column gutter) must line their `[id]` column, block indicator, and
        // description into one straight vertical column: the gutter is padded to a
        // uniform width so branch depth never pushes the `[id] ░ …` block sideways.
        let mut app = test_app();
        app.rows = vec![
            graph::GraphRow {
                gutter: "○".into(),
                content: "█ trunk work".into(),
                node_index: Some(0),
                change_id: Some("aaaaaaaa".into()),
            },
            graph::GraphRow {
                gutter: "├─●".into(),
                content: "░ #11 :: agent work".into(),
                node_index: Some(1),
                change_id: Some("bbbbbbbb".into()),
            },
        ];
        app.task_of_node = vec![None, None];
        app.id_display = std::collections::HashMap::from([
            ("aaaaaaaa".to_string(), ("aaaaaaaa".to_string(), String::new())),
            ("bbbbbbbb".to_string(), ("bbbbbbbb".to_string(), String::new())),
        ]);

        let width = 80usize;
        let backend = TestBackend::new(width as u16, 10);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| app.render(f)).unwrap();

        let buf = term.backend().buffer();
        // (x, y) of every cell carrying `sym`.
        let find = |sym: &str| -> Vec<(usize, usize)> {
            buf.content
                .iter()
                .enumerate()
                .filter(|(_, c)| c.symbol() == sym)
                .map(|(i, _)| (i % width, i / width))
                .collect()
        };

        // The block indicators are unique to the graph rows (█ on the trunk row, ░ on
        // the agent stub) — they must share one x column.
        let full = find("█");
        let empty = find("░");
        assert_eq!(full.len(), 1, "one █ indicator: {full:?}");
        assert_eq!(empty.len(), 1, "one ░ indicator: {empty:?}");
        assert_eq!(
            full[0].0, empty[0].0,
            "block indicators aligned: █@{full:?} vs ░@{empty:?}"
        );

        // The `[` opening each id column (found on the indicator's own row, so footer/
        // header brackets don't interfere) must likewise sit at one x.
        let bracket_x = |y: usize| -> usize {
            buf.content
                .iter()
                .enumerate()
                .find(|(i, c)| i / width == y && c.symbol() == "[")
                .map(|(i, _)| i % width)
                .expect("id column `[` on the indicator's row")
        };
        assert_eq!(
            bracket_x(full[0].1),
            bracket_x(empty[0].1),
            "id columns aligned"
        );
    }

    #[test]
    fn focused_session_is_marked_in_graph_and_header() {
        let mut app = test_app();
        let t = app.store.create_task("x", 0, Autonomy::Inherit).unwrap();
        app.store.set_pane(t.id, Some(77)).unwrap();
        app.tasks = app.store.list_tasks().unwrap();
        app.open_pane = Some(77); // this agent is docked beside faf
        app.rows = vec![graph::GraphRow {
            gutter: "@".into(),
            content: format!("#{} x", t.id.0),
            node_index: Some(0),
            change_id: Some("abcd1234".into()),
        }];
        app.task_of_node = vec![Some(t.id)];

        let backend = TestBackend::new(90, 10);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| app.render(f)).unwrap();
        let text: String = term
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains('▶'), "focused marker on the row: {text:?}");
        assert!(text.contains(&format!("#{}", t.id.0)));
    }

    #[test]
    fn combined_node_marks_and_highlights_the_agent_line_not_head() {
        // HEAD parked on the agent's revision: the `@` commit row is the HEAD header
        // (its description) and the agent hangs beneath. row_tasks hangs the task on the
        // agent line, so the docked `▶` marker lands there — never on the HEAD header.
        let mut app = test_app();
        let t = app.store.create_task("x", 0, Autonomy::Inherit).unwrap();
        app.store.set_pane(t.id, Some(55)).unwrap();
        app.tasks = app.store.list_tasks().unwrap();
        app.open_pane = Some(55); // the agent is docked beside faf
        app.rows = vec![
            graph::GraphRow {
                gutter: "@".into(),
                content: "(no description set)".into(),
                node_index: Some(0),
                change_id: Some("x".into()),
            },
            graph::GraphRow {
                gutter: "│".into(),
                content: format!("↳ #{} x", t.id.0),
                node_index: None,
                change_id: None,
            },
            graph::GraphRow {
                gutter: "│".into(),
                content: "  ⚙ working · %55".into(),
                node_index: None,
                change_id: None,
            },
        ];
        // What model::row_tasks produces for a combined node: task on the agent line.
        app.task_of_node = vec![None, Some(t.id), None];
        app.task_order = vec![t.id];
        app.selected = 0;

        let (w, h) = (60usize, 12usize);
        let backend = TestBackend::new(w as u16, h as u16);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| app.render(f)).unwrap();
        let buf = term.backend().buffer();
        let row_text = |y: usize| -> String {
            (0..w).map(|x| buf.content[y * w + x].symbol()).collect()
        };
        let head_row = (0..h)
            .find(|&y| row_text(y).contains("(no description set)"))
            .expect("HEAD header row rendered");
        let agent_row = (0..h)
            .find(|&y| row_text(y).contains("↳ #"))
            .expect("agent line rendered");
        assert!(
            row_text(agent_row).contains('▶'),
            "docked marker rides the agent line"
        );
        assert!(
            !row_text(head_row).contains('▶'),
            "no marker on the HEAD header row"
        );
        // The agent line is the selected/highlighted one (reverse video), not HEAD.
        let reversed = |y: usize| {
            (0..w).any(|x| buf.content[y * w + x].modifier.contains(Modifier::REVERSED))
        };
        assert!(reversed(agent_row), "agent line is highlighted when selected");
        assert!(!reversed(head_row), "HEAD header row is not highlighted");
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

        let mut app = test_app(); // faf_pane = Some(1), open_pane = None (fresh start)
        let t = app.store.create_task("x", 0, Autonomy::Inherit).unwrap();
        app.store.set_pane(t.id, Some(42)).unwrap(); // persisted across the restart
        app.tasks = app.store.list_tasks().unwrap();

        // Agent docked in faf's tab (7) -> recovered as the open session.
        let docked = vec![
            pane(7, 1, "faf"),
            pane(7, 42, "#1 x"),
            pane(9, 99, "unrelated other-tab pane"),
        ];
        assert_eq!(app.detect_open_pane(&docked), Some(42));

        // Agent detached to its own tab (8) -> nothing docked.
        let detached = vec![pane(7, 1, "faf"), pane(8, 42, "#1 x")];
        assert_eq!(app.detect_open_pane(&detached), None);

        // A non-agent pane sharing faf's tab is ignored (not a known task pane).
        let stray = vec![pane(7, 1, "faf"), pane(7, 500, "a shell")];
        assert_eq!(app.detect_open_pane(&stray), None);
    }

    #[test]
    fn reconcile_workspaces_removes_only_gone_ones() {
        use std::path::Path;
        let app = test_app();
        // Task A: workspace still present in jj.
        let a = app.store.create_task("a", 0, Autonomy::Inherit).unwrap();
        app.store
            .set_workspace(a.id, "faf-task-1", Path::new("/nope/1"), "c1", "f1")
            .unwrap();
        // Task B: workspace gone (forgotten externally / integrated + cleaned).
        let b = app.store.create_task("b", 0, Autonomy::Inherit).unwrap();
        app.store
            .set_workspace(b.id, "faf-task-2", Path::new("/nope/2"), "c2", "f2")
            .unwrap();
        let mut app = app;
        app.tasks = app.store.list_tasks().unwrap();

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
        app.reconcile_workspaces(&live);

        // Present workspace stays; vanished workspace's task is dropped entirely.
        assert!(app.store.try_get_task(a.id).unwrap().is_some());
        assert!(app.store.try_get_task(b.id).unwrap().is_none());
    }

    #[test]
    fn orphaned_workspaces_flags_only_untracked_faff_workspaces() {
        use std::path::Path;
        let app = test_app();
        // A live task whose workspace is still tracked.
        let t = app.store.create_task("a", 0, Autonomy::Inherit).unwrap();
        app.store
            .set_workspace(t.id, "faf-task-3", Path::new("/nope/3"), "c3", "f3")
            .unwrap();
        let mut app = app;
        app.tasks = app.store.list_tasks().unwrap();

        let live = vec![
            // The default workspace is never faff-owned — must be left alone.
            jj::Workspace { name: "default".into(), change_id: "x".into() },
            // Tracked by task `t` — must be kept.
            jj::Workspace { name: "faf-task-3".into(), change_id: "y".into() },
            // A faff workspace with no DB row — a ghost to forget.
            jj::Workspace { name: "faf-task-4".into(), change_id: "z".into() },
        ];

        assert_eq!(app.orphaned_workspaces(&live), vec!["faf-task-4".to_string()]);
    }
}
