//! The faff dashboard TUI: unified revision view, browse/session modes, and the
//! orchestration actions. See spec §11. Pure logic lives in the submodules
//! (input, session, model); every side effect runs on the engine thread
//! (`engine`); this file is the UI state, event loop, and rendering glue.

mod describe;
mod engine;
mod input;
mod model;
mod notify;
mod session;
mod train;

use crate::domain::{Task, TaskId, TaskStatus, truncate_first_line};
use crate::graph::GraphRow;
use anyhow::{Context, Result, bail};
use engine::{Base, Cmd, Engine, Msg, View};
use input::Action;
use ratatui::crossterm::event::{self, Event as CtEvent, KeyEventKind};
use ratatui::crossterm::{execute, terminal};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::collections::VecDeque;
use std::io::{Stdout, stdout};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

type Term = Terminal<ratatui::backend::CrosstermBackend<Stdout>>;

/// Fixed display width of the change-id column (jj pads its shortest id to 8).
const ID_W: usize = 8;

/// Most rows the notice line may occupy. A jj error can be long; three wrapped lines
/// is enough to read one in full without letting chrome crowd out the graph.
const NOTICE_MAX_LINES: u16 = 3;

/// Frames of the spinner on a revision being described — the `desc` script's.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// How long each spinner frame shows.
const SPIN_FRAME: Duration = Duration::from_millis(100);

/// Entry point for the `Tui` command.
pub fn run(repo: Option<PathBuf>) -> Result<()> {
    let repo = resolve_repo(repo)?;
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (msg_tx, msg_rx) = mpsc::channel();
    let engine = Engine::open(repo.clone(), msg_tx, cmd_tx.clone())?;
    std::thread::Builder::new()
        .name("faff-engine".into())
        .spawn(move || engine.run(cmd_rx))
        .context("spawning the engine thread")?;
    let mut app = App::new(repo, cmd_tx, msg_rx);
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

/// Which overlay, if any, is drawn over the graph. Modal: while one is open the next
/// key closes it and does nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Overlay {
    None,
    /// `!` — notice scrollback.
    Log,
    /// `?` — the full keymap.
    Help,
}

/// What the body shows. Tab switches between the two; the engine builds both from the
/// same `jj log` on every refresh, so switching never waits on jj.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Body {
    /// The curated graph: agents lifted onto their fork points, empty commits folded out,
    /// selection per task.
    Agents,
    /// Every loaded revision in jj's order, nothing collapsed or lifted, selection per
    /// revision. Task keys don't apply here.
    Log,
}

struct App {
    repo: PathBuf,
    /// The engine's latest snapshot. Everything rendered comes from here.
    view: View,
    cmd_tx: Sender<Cmd>,
    msg_rx: Receiver<Msg>,
    /// What each command sent to the engine is doing, oldest first. The engine runs them in
    /// order and answers each with one `Msg::Done`, so the front is always the one running.
    in_flight: VecDeque<String>,

    // UI state (owned by this thread alone).
    selected: usize,
    /// Levelled notification log. The most recent note is shown on the notice line;
    /// `!` opens the scrollback. Replaces the old untyped `status: String`.
    notices: notify::Notices,
    /// The open overlay, if any. See [`Overlay`].
    overlay: Overlay,
    /// Which graph the body shows. See [`Body`].
    body: Body,
    /// The log view's selected revision, kept by change id so it survives a new view.
    log_selected: Option<String>,
    /// When the spinner started, so its frame follows the clock rather than redraws.
    spin_epoch: Instant,
    /// `q` pressed: quit as soon as the in-flight commands have finished.
    should_quit: bool,
    /// `q` pressed twice: quit now, abandoning whatever is still in flight.
    quit_now: bool,
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

/// Toolbar groups. Order and membership are fixed: keys never move between widths, so
/// muscle memory holds when faff is docked and the labels shrink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Grp {
    New,
    Open,
    Rev,
    Train,
    Del,
    App,
}

impl Grp {
    /// Prefix shown in the compact form. The app group has none — `q ? !` reads fine bare.
    fn label(self) -> &'static str {
        match self {
            Grp::New => "new",
            Grp::Open => "open",
            Grp::Rev => "rev",
            Grp::Train => "train",
            Grp::Del => "del",
            Grp::App => "",
        }
    }

    fn color(self) -> Color {
        match self {
            Grp::New => Color::Green,
            Grp::Open => Color::Blue,
            Grp::Rev => Color::Cyan,
            Grp::Train => Color::Magenta,
            Grp::Del => Color::Red,
            Grp::App => Color::DarkGray,
        }
    }
}

struct Hint {
    key: &'static str,
    /// Label used in the verbose form and in the `?` overlay.
    verbose: &'static str,
    grp: Grp,
}

/// Every binding the toolbar advertises, in display order. `↵`'s label is overridden at
/// render time by [`App::enter_hint`] (open / detach / revive / start). Keep in sync
/// with `input::map_key` — the test
/// `every_action_key_appears_in_the_toolbar_data` enforces it.
const HINTS: &[Hint] = &[
    Hint {
        key: "n",
        verbose: "new",
        grp: Grp::New,
    },
    Hint {
        key: "N",
        verbose: "handoff",
        grp: Grp::New,
    },
    Hint {
        key: "↵",
        verbose: "open",
        grp: Grp::Open,
    },
    Hint {
        key: "s",
        verbose: "swap",
        grp: Grp::Rev,
    },
    Hint {
        key: "S",
        verbose: "snap",
        grp: Grp::Rev,
    },
    Hint {
        key: "r",
        verbose: "rebase",
        grp: Grp::Rev,
    },
    Hint {
        key: "R",
        verbose: "onto",
        grp: Grp::Rev,
    },
    Hint {
        key: "d",
        verbose: "desc",
        grp: Grp::Rev,
    },
    Hint {
        key: "a",
        verbose: "accept",
        grp: Grp::Train,
    },
    Hint {
        key: "A",
        verbose: "abort",
        grp: Grp::Train,
    },
    Hint {
        key: "x",
        verbose: "rm",
        grp: Grp::Del,
    },
    Hint {
        key: "X",
        verbose: "drop",
        grp: Grp::Del,
    },
    Hint {
        key: "⇥",
        verbose: "view",
        grp: Grp::App,
    },
    Hint {
        key: "q",
        verbose: "quit",
        grp: Grp::App,
    },
    Hint {
        key: "?",
        verbose: "keys",
        grp: Grp::App,
    },
    Hint {
        key: "!",
        verbose: "log",
        grp: Grp::App,
    },
];

/// Cheap, render-time facts about the selected row, used only to dim unavailable keys.
///
/// Deliberately an approximation of the real guards: `a`'s emptiness check
/// (`jj::any_revision`) and its clear-`@` check shell out to jj, and render must stay
/// pure and cheap. The authoritative guards live in the action functions, where they
/// produce the Warn notices. Dimming is a hint, not enforcement — `a` can render
/// undimmed and still refuse.
struct Caps {
    has_ws: bool,
    has_pane: bool,
    train: bool,
    /// A pane-less row Enter can still act on: the workspace outlived the agent, so `↵`
    /// starts or revives one in it rather than reporting "no session".
    revivable: bool,
    /// The log view is showing: nothing task-shaped is selected.
    log: bool,
}

/// Clip a line to `width` columns, replacing the final column with `…` so it reads as
/// cut rather than as a complete row. Used only below ~56 columns, where even the
/// compact key row overflows.
fn truncate_line(line: Line<'static>, width: usize) -> Line<'static> {
    if line.width() <= width || width == 0 {
        return line;
    }
    let budget = width.saturating_sub(1);
    let mut used = 0usize;
    let mut out: Vec<Span<'static>> = Vec::new();
    for span in line.spans {
        let w = span.content.chars().count();
        if used + w <= budget {
            used += w;
            out.push(span);
            continue;
        }
        let take = budget - used;
        if take > 0 {
            let cut: String = span.content.chars().take(take).collect();
            out.push(Span::styled(cut, span.style));
        }
        break;
    }
    out.push(Span::styled("…", Style::default().fg(Color::DarkGray)));
    Line::from(out)
}

/// Rows kept visible below the selection when the graph scrolls to reach it, so the
/// selected task isn't jammed against the bottom marker.
const SCROLL_CONTEXT: usize = 2;

/// Which slice of the graph's lines fits in the pane. `skip` lines are hidden above,
/// then `body` lines are shown. `up`/`down` say whether a `↑ N more` / `↓ N more`
/// marker takes the pane's first/last row; hidden counts are `skip` and
/// `total - skip - body`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Viewport {
    skip: usize,
    body: usize,
    up: bool,
    down: bool,
}

/// Fit `total` lines into `height` rows, keeping `sel` (the selected task's first..=last
/// line) on screen. Stateless — derived from the selection alone on every frame — so a
/// refresh or resize can't leave a stale offset behind: the graph stays pinned to the top
/// until the selection would fall below the cut, then scrolls just far enough to show it
/// plus [`SCROLL_CONTEXT`] rows beneath.
fn graph_viewport(total: usize, height: usize, sel: Option<(usize, usize)>) -> Viewport {
    if total <= height {
        return Viewport { skip: 0, body: total, up: false, down: false };
    }
    // Too short for markers to leave room for anything: show a bare window.
    if height < 3 {
        let skip = sel.map_or(0, |(a, _)| a.min(total - height));
        return Viewport { skip, body: height, up: false, down: false };
    }
    let end = sel.map_or(0, |(_, b)| (b + 1 + SCROLL_CONTEXT).min(total));
    // Pinned to the top: everything up to `end` fits above the `↓` marker.
    if end < height {
        return Viewport { skip: 0, body: height - 1, up: false, down: true };
    }
    let first = sel.map_or(0, |(a, _)| a);
    if end == total {
        // Scrolled to the very bottom: only the `↑` marker.
        let body = height - 1;
        let skip = (total - body).min(first).max(1);
        let down = skip + body < total;
        let body = if down { body - 1 } else { body };
        return Viewport { skip, body, up: true, down };
    }
    // Both markers. A selection taller than the body shows its top.
    let body = height - 2;
    let skip = (end - body).min(first).max(1);
    Viewport { skip, body, up: true, down: true }
}

/// A change id cut to the id column's width, for notices and busy labels.
fn short_id(change_id: &str) -> String {
    change_id.chars().take(ID_W).collect()
}

/// Extend a selection's first..=last line range to take in line `at`.
fn mark_line(sel: &mut Option<(usize, usize)>, at: usize) {
    *sel = Some(sel.map_or((at, at), |(a, _)| (a, at)));
}

/// Draw graph `lines` into `block`: the graph can outgrow the pane (every workspace's
/// ancestors, plus the detached list), so show the slice around the selection and mark
/// what's hidden. Shared by both views, so they scroll alike.
fn render_viewport(
    f: &mut Frame,
    area: Rect,
    block: Block,
    lines: Vec<Line<'static>>,
    sel_lines: Option<(usize, usize)>,
) {
    let height = block.inner(area).height as usize;
    let total = lines.len();
    let vp = graph_viewport(total, height, sel_lines);
    let more = |glyph: char, n: usize| {
        Line::from(Span::styled(
            format!("{glyph} {n} more"),
            Style::default().fg(Color::DarkGray),
        ))
    };
    let mut shown: Vec<Line> = Vec::with_capacity(height);
    if vp.up {
        shown.push(more('↑', vp.skip));
    }
    shown.extend(lines.into_iter().skip(vp.skip).take(vp.body));
    if vp.down {
        shown.push(more('↓', total - vp.skip - vp.body));
    }
    f.render_widget(Paragraph::new(shown).block(block), area);
}

impl App {
    fn new(repo: PathBuf, cmd_tx: Sender<Cmd>, msg_rx: Receiver<Msg>) -> App {
        let mut app = App {
            repo,
            view: View::default(),
            cmd_tx,
            msg_rx,
            in_flight: VecDeque::new(),
            selected: 0,
            notices: notify::Notices::default(),
            overlay: Overlay::None,
            body: Body::Agents,
            log_selected: None,
            spin_epoch: Instant::now(),
            should_quit: false,
            quit_now: false,
            pending_swap: None,
            pending_rebase: None,
            pending_describe: None,
        };
        app.notices.info("ready");
        // The opening "ready" is not news; don't start the session with a badge.
        app.notices.mark_seen();
        app
    }

    /// The UI loop: draw, apply whatever the engine sent, and handle keys. Nothing here
    /// shells out or touches the store, so a slow jj or wezterm call can delay a view
    /// update but never a keypress or a redraw.
    fn event_loop(&mut self, term: &mut Term) -> Result<()> {
        let mut dirty = true;
        let mut last_draw = Instant::now();
        loop {
            dirty |= self.drain_messages()?;
            if self.quit_now || (self.should_quit && self.in_flight.is_empty()) {
                return Ok(());
            }
            // A revision being described spins, so keep drawing while one is.
            dirty |= !self.view.describing.is_empty() && last_draw.elapsed() >= SPIN_FRAME;
            if dirty {
                term.draw(|f| self.render(f))?;
                last_draw = Instant::now();
                dirty = false;
            }
            if event::poll(Duration::from_millis(50))? {
                if let CtEvent::Key(key) = event::read()?
                    && key.kind == KeyEventKind::Press
                {
                    self.handle_key(key);
                }
                // A resize (or any other terminal event) needs a redraw too.
                dirty = true;
            }
        }
    }

    /// Apply everything the engine has sent since the last pass. Returns whether anything
    /// arrived. The engine only hangs up by panicking, which leaves faff with no way to act
    /// — surface that as an error rather than sit there looking alive.
    fn drain_messages(&mut self) -> Result<bool> {
        let mut any = false;
        loop {
            match self.msg_rx.try_recv() {
                Ok(Msg::Notice(level, text)) => self.notices.push(level, text),
                Ok(Msg::View(view)) => self.apply_view(*view),
                Ok(Msg::Done) => {
                    self.in_flight.pop_front();
                }
                Err(TryRecvError::Empty) => return Ok(any),
                Err(TryRecvError::Disconnected) => bail!("faff's worker thread stopped"),
            }
            any = true;
        }
    }

    /// Swap in a fresh view, keeping the selection on the same task by identity. A task
    /// the engine just created (`n`/`N`) wins, so the new row is highlighted.
    fn apply_view(&mut self, view: View) {
        let prev = self.view.task_order.get(self.selected).copied();
        let prev_log_pos = self.log_selected_pos();
        self.view = view;
        self.selected = self
            .view
            .select
            .or(prev)
            .and_then(|id| self.view.task_order.iter().position(|x| *x == id))
            .unwrap_or_else(|| {
                self.selected
                    .min(self.view.task_order.len().saturating_sub(1))
            });

        // The log view follows its revision by change id. A vanished revision hands the
        // selection to whatever took its row; a first view starts on your working copy.
        let ids: Vec<String> = self
            .log_node_ids()
            .into_iter()
            .map(str::to_string)
            .collect();
        let kept = self.log_selected.take().filter(|id| ids.contains(id));
        let fallback = || match prev_log_pos {
            Some(p) => ids.get(p.min(ids.len().saturating_sub(1))).cloned(),
            None => self
                .view
                .working_copy
                .clone()
                .filter(|id| ids.contains(id))
                .or(ids.first().cloned()),
        };
        self.log_selected = kept.or_else(fallback);
    }

    /// The log view's selectable revisions, top to bottom — one per node row.
    fn log_node_ids(&self) -> Vec<&str> {
        self.view
            .log_rows
            .iter()
            .filter_map(|r| r.change_id.as_deref())
            .collect()
    }

    fn log_selected_pos(&self) -> Option<usize> {
        let sel = self.log_selected.as_deref()?;
        self.log_node_ids().iter().position(|id| *id == sel)
    }

    fn move_log_selection(&mut self, delta: isize) {
        let ids = self.log_node_ids();
        if ids.is_empty() {
            return;
        }
        let n = ids.len() as isize;
        let cur = self.log_selected_pos().unwrap_or(0) as isize;
        let next = ids[(cur + delta).rem_euclid(n) as usize].to_string();
        self.log_selected = Some(next);
    }

    /// The revision `n`/`N` act on: the log view's selection, or `None` in the agent view
    /// (where they act on the fork point / your `@` as always).
    fn log_revision(&self) -> Option<String> {
        match self.body {
            Body::Log => self.log_selected.clone(),
            Body::Agents => None,
        }
    }

    /// The task whose agent sits on the log view's selected revision. Read across the
    /// node's whole block: a combined HEAD+agent node hangs its task on the line below.
    fn log_selected_task(&self) -> Option<Task> {
        let sel = self.log_selected.as_deref()?;
        let rows = &self.view.log_rows;
        let start = rows
            .iter()
            .position(|r| r.change_id.as_deref() == Some(sel))?;
        let in_block = |i: &usize| {
            *i == start || (rows[*i].change_id.is_none() && !rows[*i].content.is_empty())
        };
        let id = (start..rows.len())
            .take_while(in_block)
            .find_map(|i| self.view.log_task_of.get(i).copied().flatten())?;
        self.view.tasks.iter().find(|t| t.id == id).cloned()
    }

    /// `d` in the log view. On an agent's revision, ask that agent (as the agent view
    /// does): describing its working copy from here would leave its workspace stale.
    /// Anywhere else a headless claude writes the description.
    fn describe_log_selection(&mut self) {
        if let Some(t) = self.log_selected_task() {
            self.describe_task(t);
        } else if let Some(rev) = self.log_selected.clone() {
            self.send(
                format!("describing {}", short_id(&rev)),
                Cmd::DescribeRev(rev),
            );
        }
    }

    /// Queue `cmd` on the engine; `doing` labels it in the header until it completes.
    fn send(&mut self, doing: impl Into<String>, cmd: Cmd) {
        if self.cmd_tx.send(cmd).is_ok() {
            self.in_flight.push_back(doing.into());
        } else {
            self.notices.err("faff's worker thread stopped");
        }
    }

    fn handle_key(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        let action = input::map_key(key);
        // An open overlay is modal: the next key dismisses it and nothing else. Safe to
        // check before the `pending_*` blocks because the two cannot coexist — while a
        // confirmation is armed, `!`/`?` are swallowed as "any other key cancels".
        if self.overlay != Overlay::None {
            self.overlay = Overlay::None;
            return;
        }
        // A pending swap-confirmation swallows the next key: `s` confirms, anything
        // else cancels (so a live agent's files are only yanked on a deliberate re-press).
        if let Some(id) = self.pending_swap.take() {
            if action == Action::Swap {
                self.send(format!("swapping #{}", id.0), Cmd::Swap(id));
            } else {
                self.notices.info("swap cancelled");
            }
            return;
        }
        // A pending rebase-confirmation swallows the next key: the *same* key (`r` or
        // `R`) confirms, anything else cancels (a redirect prompt into a live agent is
        // only sent on a deliberate re-press).
        if let Some((id, freeze)) = self.pending_rebase.take() {
            let confirmed =
                (freeze && action == Action::Rebase) || (!freeze && action == Action::RebaseParent);
            if confirmed {
                self.send(format!("rebasing #{}", id.0), Cmd::Rebase { id, freeze });
            } else {
                self.notices.info("rebase cancelled");
            }
            return;
        }
        // A pending describe-confirmation swallows the next key: a second `d` confirms,
        // anything else cancels (the describe prompt is only injected on a deliberate
        // re-press).
        if let Some(id) = self.pending_describe.take() {
            if action == Action::Describe {
                self.send(format!("describing #{}", id.0), Cmd::Describe(id));
            } else {
                self.notices.info("describe cancelled");
            }
            return;
        }
        // The log view selects revisions, not tasks, so the keys that act on the selected
        // task have nothing to act on there. Refuse them rather than reach for the agent
        // view's (hidden) selection.
        let acts_on_task = matches!(
            action,
            Action::ToggleSession
                | Action::Remove
                | Action::RemoveDiscard
                | Action::Swap
                | Action::Snapshot
                | Action::Rebase
                | Action::RebaseParent
                | Action::Accept
        );
        if self.body == Body::Log && acts_on_task {
            self.notices
                .warn("the log view has no task selected — tab back to agents");
            return;
        }
        match action {
            Action::Quit => self.quit(),
            Action::Up if self.body == Body::Log => self.move_log_selection(-1),
            Action::Down if self.body == Body::Log => self.move_log_selection(1),
            Action::Up => self.move_selection(-1),
            Action::Down => self.move_selection(1),
            Action::ToggleView => {
                self.body = match self.body {
                    Body::Agents => Body::Log,
                    Body::Log => Body::Agents,
                };
            }
            Action::NewTask => match self.log_revision() {
                Some(rev) => {
                    let doing = format!("creating a task on {}", short_id(&rev));
                    self.send(doing, Cmd::New(Base::At(rev)));
                }
                None => self.send("creating a task", Cmd::New(Base::ForkPoint)),
            },
            Action::Handoff => match self.log_revision() {
                Some(rev) => {
                    let doing = format!("handing off {}", short_id(&rev));
                    self.send(doing, Cmd::New(Base::Continue(rev)));
                }
                None => self.send("handing off", Cmd::New(Base::Handoff)),
            },
            Action::ToggleSession => {
                let doing = match self.enter_hint() {
                    "detach" => "detaching",
                    "revive" => "reviving",
                    "start" => "starting",
                    _ => "opening",
                };
                let id = self.selected_task().map(|t| t.id);
                let what = id.map(|id| format!(" #{}", id.0)).unwrap_or_default();
                self.send(format!("{doing}{what}"), Cmd::Toggle(id));
            }
            Action::Remove => self.remove_selected(false),
            Action::RemoveDiscard => self.remove_selected(true),
            Action::Swap => self.swap_selected(),
            Action::Snapshot => self.snapshot_selected(),
            Action::Rebase => self.rebase_selected(true),
            Action::RebaseParent => self.rebase_selected(false),
            Action::Describe if self.body == Body::Log => self.describe_log_selection(),
            Action::Describe => self.describe_selected(),
            Action::Accept => self.accept_selected(),
            Action::AbortTrain => self.abort_train(),
            Action::ShowLog => {
                self.overlay = Overlay::Log;
                self.notices.mark_seen();
            }
            Action::Help => self.overlay = Overlay::Help,
            Action::None => {}
        }
    }

    /// `q`: quit once the engine is idle, so a half-finished create or remove is never
    /// cut off mid-way. A second `q` quits regardless.
    fn quit(&mut self) {
        if self.in_flight.is_empty() || self.should_quit {
            self.quit_now = true;
            return;
        }
        self.should_quit = true;
        self.notices.prompt(format!(
            "quitting once {} finishes — press q again to quit now",
            self.in_flight.len()
        ));
    }

    fn selected_task(&self) -> Option<Task> {
        let id = self.view.task_order.get(self.selected)?;
        self.view.tasks.iter().find(|t| &t.id == id).cloned()
    }

    /// The task whose agent pane is currently docked beside faff (the focused session).
    fn open_task_id(&self) -> Option<TaskId> {
        let open = self.view.open_pane?;
        self.view
            .tasks
            .iter()
            .find(|t| t.pane_id == Some(open))
            .map(|t| t.id)
    }

    fn move_selection(&mut self, delta: isize) {
        if self.view.task_order.is_empty() {
            return;
        }
        let n = self.view.task_order.len() as isize;
        let cur = self.selected as isize;
        self.selected = ((cur + delta).rem_euclid(n)) as usize;
    }

    // ---- actions ----
    //
    // Each one checks only what the view already knows (a workspace, a pane, whether the
    // agent is working) to refuse or arm a confirmation, then hands the real work to the
    // engine, which re-checks against live state.

    /// `x` / `X`: remove the selected task. `x` keeps its real work as ordinary jj history;
    /// `X` discards the revision too (see `workspace::teardown_discarding_revision`).
    fn remove_selected(&mut self, discard: bool) {
        let Some(t) = self.selected_task() else {
            return;
        };
        self.send(
            format!("removing #{}", t.id.0),
            Cmd::Remove { id: t.id, discard },
        );
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
            self.notices.warn("no workspace to swap for this task");
            return;
        }
        if t.status == TaskStatus::Working {
            self.pending_swap = Some(t.id);
            self.notices.prompt(format!(
                "#{} is working — press s to confirm swap, any other key cancels",
                t.id.0
            ));
            return;
        }
        self.send(format!("swapping #{}", t.id.0), Cmd::Swap(t.id));
    }

    /// `S`: snapshot the selected agent's workspace so edits from an agent that hasn't
    /// run a jj command land in its `@` and show up in the graph.
    fn snapshot_selected(&mut self) {
        let Some(t) = self.selected_task() else {
            return;
        };
        if t.ws_path.is_none() {
            self.notices.warn("no workspace to snapshot for this task");
            return;
        }
        self.send(format!("snapshotting #{}", t.id.0), Cmd::Snapshot(t.id));
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
            self.notices.warn("no workspace to rebase for this task");
            return;
        }
        if t.pane_id.is_none() {
            self.notices
                .warn("no live pane to send the rebase prompt to");
            return;
        }
        if t.status == TaskStatus::Working {
            self.pending_rebase = Some((t.id, freeze));
            let key = if freeze { "r" } else { "R" };
            self.notices.prompt(format!(
                "#{} is working — press {key} to confirm rebase, any other key cancels",
                t.id.0
            ));
            return;
        }
        self.send(
            format!("rebasing #{}", t.id.0),
            Cmd::Rebase { id: t.id, freeze },
        );
    }

    /// `d`: ask the selected agent to describe its own revision. faff never runs `jj
    /// describe` itself — it injects a prompt telling the agent to set a short 4-7 word
    /// description of the end result, so the log row shows what the revision actually did
    /// (not just the prompt-derived label). On a working agent the first
    /// press only arms a confirmation (a mid-turn prompt is disruptive, and a description
    /// is premature before the work settles); a second `d` goes through (via `handle_key`).
    fn describe_selected(&mut self) {
        if let Some(t) = self.selected_task() {
            self.describe_task(t);
        }
    }

    fn describe_task(&mut self, t: Task) {
        if t.ws_name.is_none() {
            self.notices.warn("no workspace to describe for this task");
            return;
        }
        if t.pane_id.is_none() {
            self.notices
                .warn("no live pane to send the describe prompt to");
            return;
        }
        if t.status == TaskStatus::Working {
            self.pending_describe = Some(t.id);
            self.notices.prompt(format!(
                "#{} is working — press d to confirm describe, any other key cancels",
                t.id.0
            ));
            return;
        }
        self.send(format!("describing #{}", t.id.0), Cmd::Describe(t.id));
    }

    /// `a`: toggle the selected agent's revision into/out of the merge train. A queued task
    /// always toggles back out; otherwise one with no workspace or live pane is refused here,
    /// and the engine makes the jj checks (content to merge, a clear `@`).
    fn accept_selected(&mut self) {
        let Some(t) = self.selected_task() else {
            return;
        };
        if !self.view.merge_train.contains(t.id) {
            if t.ws_name.is_none() {
                self.notices.warn("no workspace to accept for this task");
                return;
            }
            if t.pane_id.is_none() {
                self.notices.warn(format!(
                    "no live pane to drive #{} through the merge train",
                    t.id.0
                ));
                return;
            }
        }
        self.send(format!("accepting #{}", t.id.0), Cmd::Accept(t.id));
    }

    /// `A`: abort the merge train — dequeue everything still pending. Revisions already
    /// taken over stay merged; nothing is rolled back.
    fn abort_train(&mut self) {
        if self.view.merge_train.is_empty() {
            self.notices.warn("no merge train to abort");
            return;
        }
        self.send("aborting the merge train", Cmd::AbortTrain);
    }

    // ---- rendering ----

    fn render(&self, f: &mut Frame) {
        let train_h = self.merge_train_height();
        let notice_h = self.notice_height(f.area().width);
        let mut constraints = vec![
            Constraint::Length(1),        // header
            Constraint::Min(1),           // revision graph
            Constraint::Length(notice_h), // notice line
            Constraint::Length(1),        // key row
        ];
        if train_h > 0 {
            constraints.push(Constraint::Length(train_h)); // merge-train panel below the keys
        }
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(f.area());
        self.render_header(f, chunks[0]);
        self.render_body(f, chunks[1]);
        if self.overlay != Overlay::None {
            self.render_overlay(f, chunks[1]);
        }
        self.render_notice(f, chunks[2]);
        self.render_keys(f, chunks[3]);
        if train_h > 0 {
            self.render_merge_train(f, chunks[4]);
        }
    }

    /// Height of the merge-train panel: a title/border row plus one per member (and a note
    /// row when present), capped so it never dominates the screen. Zero — panel not drawn —
    /// when the train is empty.
    fn merge_train_height(&self) -> u16 {
        if self.view.merge_train.is_empty() {
            return 0;
        }
        let note = u16::from(!self.view.merge_train.note.is_empty());
        (self.view.merge_train.members.len() as u16 + note + 1).min(12)
    }

    fn render_merge_train(&self, f: &mut Frame, area: Rect) {
        let mut lines: Vec<Line> = Vec::new();
        if !self.view.merge_train.note.is_empty() {
            lines.push(Line::from(Span::styled(
                format!(" {}", self.view.merge_train.note),
                Style::default().fg(Color::Yellow),
            )));
        }
        for m in &self.view.merge_train.members {
            let label = self
                .view
                .tasks
                .iter()
                .find(|t| t.id == m.task)
                .map(|t| t.label())
                .unwrap_or_default();
            let stage = m.stage;
            let color = match stage {
                train::Stage::Ready => Color::Green,
                train::Stage::Merging => Color::Cyan,
                train::Stage::Resolving => Color::Red,
                _ => Color::Gray,
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" #{:<3} ", m.task.0),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(format!("{:<42} ", truncate_first_line(&label, 40))),
                Span::styled(stage.label().to_string(), Style::default().fg(color)),
            ]));
        }
        let block = Block::default().borders(Borders::TOP).title("merge train");
        f.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn render_header(&self, f: &mut Frame, area: Rect) {
        let working = self
            .view
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Working)
            .count();
        let session = match self.open_task_id() {
            Some(id) => format!(" · ▶ #{}", id.0),
            None => String::new(),
        };
        // What the engine is busy with, so a slow create or remove reads as progress
        // rather than as a dead key.
        let busy = match (self.in_flight.front(), self.in_flight.len()) {
            (None, _) => String::new(),
            (Some(doing), 1) => format!(" · ⋯ {doing}"),
            (Some(doing), n) => format!(" · ⋯ {doing} (+{})", n - 1),
        };
        let text = format!(
            " faff · {} · {working} working{session}{busy} ",
            self.repo
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
        );
        let style = Style::default().add_modifier(Modifier::REVERSED);
        f.render_widget(Paragraph::new(text).style(style), area);
        // Which body Tab has switched to, pinned to the far right; drawn second so a long
        // status on the left is clipped under it rather than pushing it off-screen.
        let body = match self.body {
            Body::Agents => " agents ",
            Body::Log => " log ",
        };
        f.render_widget(
            Paragraph::new(Line::from(body).right_aligned()).style(style),
            area,
        );
    }

    fn render_body(&self, f: &mut Frame, area: Rect) {
        // The revision graph is the whole body: when a session is docked WezTerm owns
        // the right half (the real claude pane) beside faff's narrowed area, and when
        // browsing the graph simply uses the full width.
        self.render_graph(f, area);
    }

    fn render_graph(&self, f: &mut Frame, area: Rect) {
        match self.body {
            Body::Agents => self.render_agents(f, area),
            Body::Log => self.render_log(f, area),
        }
    }

    fn render_agents(&self, f: &mut Frame, area: Rect) {
        let selected = self.view.task_order.get(self.selected).copied();
        let open = self.open_task_id();
        // Text width inside the block (its RIGHT border takes one column). Labels are
        // clipped to fit this — and only when they overflow — so the log fills the pane
        // and re-fits whenever docking/detaching a session resizes faff.
        let text_w = area.width.saturating_sub(1) as usize;
        let task_of = &self.view.task_of_node;
        let is_sel = |i: usize| selected.is_some() && task_of.get(i).copied().flatten() == selected;
        let (mut lines, mut sel_lines) =
            self.graph_lines(&self.view.rows, task_of, &is_sel, text_w);
        // Detached tasks (workspace inlined into HEAD, or a stale row) have no
        // distinct node — list them below so they stay visible and selectable.
        if !self.view.detached.is_empty() {
            lines.push(Line::from(Span::styled(
                "── detached (integrated / no node) ──",
                Style::default().fg(Color::DarkGray),
            )));
            for id in &self.view.detached {
                if let Some(t) = self.view.tasks.iter().find(|t| &t.id == id) {
                    let (icon, _) = model::status_label(t.status);
                    let docked = open == Some(*id);
                    let style = if selected == Some(*id) {
                        Style::default().add_modifier(Modifier::REVERSED)
                    } else {
                        Style::default()
                    };
                    // A detached row has no revision brackets, so the docked session is
                    // shown by greening its `· #id` prefix instead — the same fixed
                    // left-column indicator, never crowded out by a truncated description.
                    // Start from `Style::default()`, not `style`: on the highlighted row
                    // `style` carries REVERSED, which would swap fg/bg into a green cell
                    // background rather than green glyphs.
                    let prefix = format!("· #{}", t.id.0);
                    let rest = format!(" {} {icon}", t.label());
                    let prefix_style = if docked {
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        style
                    };
                    let avail = text_w.saturating_sub(prefix.chars().count());
                    let text = truncate_first_line(&rest, avail);
                    // Here the glyph trails the description rather than leading it, so
                    // tint the tail — and only when the clip left it in place; a title
                    // long enough to be cut takes the glyph with it, as it always has.
                    let mut spans = vec![Span::styled(prefix, prefix_style)];
                    match text.strip_suffix(icon) {
                        Some(head) => {
                            spans.push(Span::styled(head.to_string(), style));
                            spans.push(Span::styled(
                                icon.to_string(),
                                status_style(style, t.status),
                            ));
                        }
                        None => spans.push(Span::styled(text, style)),
                    }
                    if selected == Some(*id) {
                        mark_line(&mut sel_lines, lines.len());
                    }
                    lines.push(Line::from(spans));
                }
            }
        }
        let block = Block::default().borders(Borders::RIGHT);
        render_viewport(f, area, block, lines, sel_lines);
    }

    fn render_log(&self, f: &mut Frame, area: Rect) {
        let text_w = area.width.saturating_sub(1) as usize;
        // Highlight the selected revision's node row and any continuation rows under it
        // (a combined HEAD+agent node is two lines); link rows between nodes stay plain.
        let rows = &self.view.log_rows;
        let sel = self.log_selected.as_deref();
        let mut sel_rows = vec![false; rows.len()];
        let mut in_sel = false;
        for (i, row) in rows.iter().enumerate() {
            match &row.change_id {
                Some(cid) => in_sel = Some(cid.as_str()) == sel,
                None if row.content.is_empty() => in_sel = false,
                None => {}
            }
            sel_rows[i] = in_sel;
        }
        let is_sel = |i: usize| sel_rows[i];
        let (lines, sel_lines) = self.graph_lines(rows, &self.view.log_task_of, &is_sel, text_w);
        let block = Block::default().borders(Borders::RIGHT);
        render_viewport(f, area, block, lines, sel_lines);
    }

    /// The spinner frame for now, shown on a row a headless claude is describing.
    fn spinner_frame(&self) -> &'static str {
        let n = self.spin_epoch.elapsed().as_millis() / SPIN_FRAME.as_millis();
        SPINNER[n as usize % SPINNER.len()]
    }

    /// Paint laid-out graph rows: tinted gutter glyphs, the `[id]` column, the clipped
    /// label. Shared by both views; `is_sel` says which rows are highlighted, and the
    /// highlighted lines' first..=last index comes back with the lines for the viewport.
    fn graph_lines(
        &self,
        rows: &[GraphRow],
        task_of: &[Option<TaskId>],
        is_sel: &dyn Fn(usize) -> bool,
        text_w: usize,
    ) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
        let open = self.open_task_id();
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(rows.len());
        // Width of the "[abcdefgh] " id column, so continuation lines align under content.
        let id_col = ID_W + 3;
        // Pad every gutter to one uniform width so the [id] column and description line up
        // into straight columns regardless of branch depth — a folded
        // agent stub `├─●` is wider than a trunk glyph `◻`, and without this the whole
        // right-hand block shifts sideways by the difference. The 2-space separator that
        // was previously appended per-row is folded into this width. (Gutter cells are all
        // single-column box-drawing chars, so char count is the display width.)
        let gutter_w = rows
            .iter()
            .map(|r| r.gutter.chars().count())
            .max()
            .unwrap_or(0)
            + 2;
        // Which node rows carry the docked (focused) session, so their revision id
        // can be greened. A combined HEAD+agent node hangs its task on a continuation line
        // below the bracketed HEAD row, so attribute each row to the nearest node row at
        // or above it — the one whose id is shown.
        let mut node_is_docked = vec![false; rows.len()];
        if let Some(open_id) = open {
            let mut last_node: Option<usize> = None;
            for (i, row) in rows.iter().enumerate() {
                if row.change_id.is_some() {
                    last_node = Some(i);
                }
                if task_of.get(i).copied().flatten() == Some(open_id)
                    && let Some(n) = last_node
                {
                    node_is_docked[n] = true;
                }
            }
        }
        // The selected rows' first..=last line, so the viewport can keep them on screen.
        let mut sel_lines: Option<(usize, usize)> = None;
        for (i, row) in rows.iter().enumerate() {
            let row_task = task_of.get(i).copied().flatten();
            let row_status =
                row_task.and_then(|id| self.view.tasks.iter().find(|t| t.id == id).map(|t| t.status));
            let is_sel = is_sel(i);
            let base = if is_sel {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            let mut spans: Vec<Span> = Vec::new();
            match &row.change_id {
                // Node row: gutter + [id] + content, with the unique prefix highlighted.
                Some(cid) => {
                    // Tint one node glyph in the gutter (bold), leaving every other gutter
                    // character at the base style. The current fork point — where new agents
                    // branch from — is a cyan `◆`, or a cyan `@` when it coincides with the
                    // working copy; a non-fork working-copy `@` is green, like jj log.
                    let gutter = format!("{:<gutter_w$}", row.gutter);
                    let is_fork = self.view.fork_point.as_deref() == Some(cid.as_str());
                    let highlight = if is_fork {
                        gutter
                            .find('◆')
                            .map(|at| (at, '◆', Color::Cyan))
                            .or_else(|| gutter.find('@').map(|at| (at, '@', Color::Cyan)))
                    } else {
                        gutter.find('@').map(|at| (at, '@', Color::Green))
                    };
                    match highlight {
                        Some((at, ch, color)) => {
                            spans.push(Span::styled(gutter[..at].to_string(), base));
                            spans.push(Span::styled(
                                ch.to_string(),
                                base.fg(color).add_modifier(Modifier::BOLD),
                            ));
                            spans
                                .push(Span::styled(gutter[at + ch.len_utf8()..].to_string(), base));
                        }
                        None => spans.push(Span::styled(gutter, base)),
                    }
                    let (prefix, rest) = self
                        .view
                        .id_display
                        .get(cid)
                        .cloned()
                        .unwrap_or_else(|| (cid.chars().take(ID_W).collect(), String::new()));
                    let id_w = 1 + prefix.chars().count() + rest.chars().count() + 2;
                    // The docked (focused) session shows through its revision id: the
                    // brackets *and* the id's non-unique remainder go green + bold, else
                    // both stay the muted dark-gray frame. Two thin bracket glyphs alone
                    // are easy to miss; the remainder gives the green some width. The
                    // unique prefix keeps its cyan either way — green and blue inside one
                    // id would read as noise. A fixed left-column indicator survives
                    // description truncation that a trailing marker would not. The docked
                    // style starts from `Style::default()`, NOT `base`: on the highlighted
                    // row `base` carries REVERSED, which would swap fg/bg and paint the
                    // cell background green instead of the glyphs.
                    let frame = if node_is_docked[i] {
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        base.fg(Color::DarkGray)
                    };
                    spans.push(Span::styled("[", frame));
                    spans.push(Span::styled(
                        prefix,
                        base.fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    ));
                    spans.push(Span::styled(rest, frame));
                    spans.push(Span::styled("] ", frame));
                    let mut avail = text_w.saturating_sub(gutter_w + id_w);
                    if self.view.describing.contains(cid) {
                        spans.push(Span::styled(
                            format!("{} ", self.spinner_frame()),
                            base.fg(Color::Yellow).add_modifier(Modifier::BOLD),
                        ));
                        avail = avail.saturating_sub(2);
                    }
                    push_row_content(
                        &mut spans,
                        truncate_first_line(&row.content, avail),
                        base,
                        row_status,
                    );
                }
                // Link row (no content): gutter only.
                None if row.content.is_empty() => {
                    spans.push(Span::styled(row.gutter.clone(), base));
                }
                // Continuation row: gutter + id-column padding + content (aligned).
                None => {
                    let pad = format!("{:<gutter_w$}{}", row.gutter, " ".repeat(id_col));
                    let avail = text_w.saturating_sub(pad.chars().count());
                    spans.push(Span::styled(pad, base));
                    push_row_content(
                        &mut spans,
                        truncate_first_line(&row.content, avail),
                        base,
                        row_status,
                    );
                }
            }
            if is_sel {
                mark_line(&mut sel_lines, lines.len());
            }
            lines.push(Line::from(spans));
        }
        (lines, sel_lines)
    }

    /// Rows the current notice needs, clamped to [`NOTICE_MAX_LINES`]. At least 1 even
    /// with no notice, so the graph does not jitter when the first one arrives.
    fn notice_height(&self, width: u16) -> u16 {
        let Some(n) = self.notices.latest() else {
            return 1;
        };
        let avail = usize::from(width.saturating_sub(3)).max(1);
        (notify::wrap_words(&n.text, avail).len() as u16).clamp(1, NOTICE_MAX_LINES)
    }

    /// The current notice, coloured by level and wrapped. Sticky: it stays until the
    /// next notice replaces it.
    fn render_notice(&self, f: &mut Frame, area: Rect) {
        let Some(n) = self.notices.latest() else {
            return;
        };
        let avail = usize::from(area.width.saturating_sub(3)).max(1);
        let wrapped = notify::wrap_words(&n.text, avail);
        let clipped = wrapped.len() > NOTICE_MAX_LINES as usize;
        let style = Style::default().fg(n.level.color());
        let last = NOTICE_MAX_LINES as usize - 1;
        let lines: Vec<Line> = wrapped
            .iter()
            .take(NOTICE_MAX_LINES as usize)
            .enumerate()
            .map(|(i, w)| {
                // Anything past the cap is still in the `!` log; mark the cut.
                let text = if clipped && i == last {
                    format!("{w}…")
                } else {
                    w.clone()
                };
                if i == 0 {
                    Line::from(vec![
                        Span::styled(
                            format!(" {} ", n.level.glyph()),
                            style.add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(text, style),
                    ])
                } else {
                    // Continuation lines indent clear of the glyph column.
                    Line::from(Span::styled(format!("   {text}"), style))
                }
            })
            .collect();
        f.render_widget(Paragraph::new(lines), area);
    }

    fn caps(&self) -> Caps {
        let t = self.selected_task();
        Caps {
            has_ws: t.as_ref().is_some_and(|t| t.ws_path.is_some()),
            has_pane: t.as_ref().is_some_and(|t| t.pane_id.is_some()),
            train: !self.view.merge_train.is_empty(),
            revivable: t.as_ref().is_some_and(|t| self.view.revivable.contains(&t.id)),
            log: self.body == Body::Log,
        }
    }

    /// Whether `key` is usable on the selected row. See [`Caps`] on why this is an
    /// approximation.
    fn hint_available(key: &str, c: &Caps) -> bool {
        match key {
            "↵" | "s" | "S" | "r" | "R" | "a" | "x" | "X" if c.log => false,
            // Every revision can be described: by its agent, else by a headless claude.
            "d" if c.log => true,
            // A lost instance has no pane, but Enter still has something to do with it.
            "↵" => c.has_pane || c.revivable,
            "s" | "S" => c.has_ws,
            "r" | "R" | "d" | "a" => c.has_ws && c.has_pane,
            "A" => c.train,
            _ => true,
        }
    }

    /// The `↵` label for the current selection — the only place a *lost* instance
    /// announces that it can be brought back.
    fn enter_hint(&self) -> &'static str {
        let selected = self.selected_task();
        let selected_pane = selected.as_ref().and_then(|t| t.pane_id);
        if self.view.open_pane.is_some() && self.view.open_pane == selected_pane {
            "detach"
        } else if selected
            .as_ref()
            .is_some_and(|t| self.view.resumable.contains(&t.id))
        {
            "revive"
        } else if selected
            .as_ref()
            .is_some_and(|t| self.view.revivable.contains(&t.id))
        {
            "start"
        } else {
            "open"
        }
    }

    /// Build the key row. `verbose` picks `n new  N handoff …`; otherwise the compact
    /// `new nN │ open ↵ │ …`. Both come from [`HINTS`], so their widths are derived and
    /// the two forms cannot drift apart.
    fn keys_line(&self, verbose: bool) -> Line<'static> {
        let caps = self.caps();
        let enter = self.enter_hint();
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut cur: Option<Grp> = None;
        // The `!` key doubles as the unseen-notice badge, coloured by the worst unseen
        // level so a red `!3` is visible without opening anything.
        let unseen = self.notices.unseen();
        let badge = self.notices.max_unseen_level().map(|l| l.color());
        for h in HINTS {
            let label = match h.key {
                "↵" => enter,
                // In the log view `N` edits the selected revision rather than handing off
                // your own.
                "N" if self.body == Body::Log => "edit",
                _ => h.verbose,
            };
            if cur != Some(h.grp) {
                if cur.is_some() {
                    spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
                } else {
                    spans.push(Span::raw(" "));
                }
                if !verbose && !h.grp.label().is_empty() {
                    spans.push(Span::styled(
                        format!("{} ", h.grp.label()),
                        Style::default().fg(h.grp.color()),
                    ));
                }
                cur = Some(h.grp);
            } else if verbose {
                spans.push(Span::raw("  "));
            } else if h.grp == Grp::App {
                // The app group's keys are unrelated to each other, unlike the `nN`/`xX`
                // pairs, and the badge would otherwise jam into `?` as `q?!3`.
                spans.push(Span::raw(" "));
            }
            let key_text = if h.key == "!" && unseen > 0 {
                format!("!{unseen}")
            } else {
                h.key.to_string()
            };
            let badge_color = if h.key == "!" && unseen > 0 {
                badge
            } else {
                None
            };
            // Unavailable keys stay in place and dim, so nothing ever shifts sideways.
            let mut style = Style::default().fg(badge_color.unwrap_or(h.grp.color()));
            if !Self::hint_available(h.key, &caps) {
                style = style.add_modifier(Modifier::DIM);
            }
            if verbose {
                spans.push(Span::styled(
                    format!("{key_text} "),
                    style.add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(label.to_string(), style));
            } else {
                spans.push(Span::styled(key_text, style));
            }
        }
        Line::from(spans)
    }

    /// Draw the open overlay over the graph area. `Clear` wipes the graph underneath so
    /// the two do not interleave.
    fn render_overlay(&self, f: &mut Frame, area: Rect) {
        let avail = usize::from(area.width.saturating_sub(4)).max(1);
        let (title, lines) = match self.overlay {
            Overlay::Log => ("notices  ·  any key closes", self.log_lines(avail)),
            Overlay::Help => ("keys  ·  any key closes", self.help_lines()),
            Overlay::None => return,
        };
        f.render_widget(Clear, area);
        let block = Block::default().borders(Borders::ALL).title(title);
        let para = Paragraph::new(lines).block(block);
        // The log wraps itself (it needs the line count); the keymap's group rows are
        // built whole, so let Paragraph fold them rather than clip a binding off the
        // edge in a narrow pane.
        let para = match self.overlay {
            Overlay::Help => para.wrap(Wrap { trim: false }),
            _ => para,
        };
        f.render_widget(para, area);
    }

    /// Notice scrollback, newest first: `HH:MM:SS ✓ text`, wrapped under the glyph.
    fn log_lines(&self, avail: usize) -> Vec<Line<'static>> {
        if self.notices.latest().is_none() {
            return vec![Line::from(Span::styled(
                " nothing reported yet",
                Style::default().fg(Color::DarkGray),
            ))];
        }
        // "HH:MM:SS " + glyph + " " — continuation lines indent past it.
        let indent = 11usize;
        let text_w = avail.saturating_sub(indent).max(1);
        let mut lines = Vec::new();
        for n in self.notices.iter() {
            let style = Style::default().fg(n.level.color());
            for (i, w) in notify::wrap_words(&n.text, text_w).into_iter().enumerate() {
                if i == 0 {
                    lines.push(Line::from(vec![
                        Span::styled(
                            n.at.format("%H:%M:%S ").to_string(),
                            Style::default().fg(Color::DarkGray),
                        ),
                        Span::styled(
                            format!("{} ", n.level.glyph()),
                            style.add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(w, style),
                    ]));
                } else {
                    lines.push(Line::from(Span::styled(
                        format!("{:indent$}{w}", "", indent = indent),
                        style,
                    )));
                }
            }
        }
        lines
    }

    /// Every binding, including the ones the toolbar has no room for. Laid out one row
    /// per toolbar group so the whole keymap fits a short pane without scrolling — a
    /// reference that can be clipped is worse than none.
    fn help_lines(&self) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from(vec![
            Span::styled(
                format!(" {:<8}", "move"),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled("↑/↓ j/k ", Style::default().add_modifier(Modifier::BOLD)),
            Span::styled("select", Style::default().fg(Color::DarkGray)),
        ])];
        let mut cur: Option<Grp> = None;
        for h in HINTS {
            if cur != Some(h.grp) {
                lines.push(Line::from(vec![Span::styled(
                    format!(" {:<8}", h.grp.label()),
                    Style::default().fg(h.grp.color()),
                )]));
                cur = Some(h.grp);
            }
            let row = lines.last_mut().expect("a group row was pushed");
            row.spans.push(Span::styled(
                format!("{} ", h.key),
                Style::default()
                    .fg(h.grp.color())
                    .add_modifier(Modifier::BOLD),
            ));
            row.spans.push(Span::styled(
                format!("{:<9}", h.verbose),
                Style::default().fg(h.grp.color()),
            ));
        }
        lines
    }

    /// Render the key row at whichever verbosity fits, truncating if even the compact
    /// form overflows.
    fn render_keys(&self, f: &mut Frame, area: Rect) {
        let verbose = self.keys_line(true);
        let line = if verbose.width() as u16 <= area.width {
            verbose
        } else {
            let compact = self.keys_line(false);
            if compact.width() as u16 <= area.width {
                compact
            } else {
                truncate_line(compact, area.width as usize)
            }
        };
        f.render_widget(Paragraph::new(line), area);
    }
}

/// Colour for a status glyph, layered onto the row's base style — which on the selected
/// row carries REVERSED, so the tint lands on the cell background instead of the glyph,
/// the same way the docked bracket's green does.
///
/// The glyphs are deliberately plain single-column marks (`⚙ ! ✓`), so colour is what
/// tells the three modes apart at a glance: blue for the quiet steady state, bold magenta
/// for the one that wants you — magenta means nothing else in faff, so it can't be
/// misread as the cyan fork point or a green docked session — and green for done and
/// waiting on your review.
fn status_style(base: Style, status: TaskStatus) -> Style {
    match status {
        TaskStatus::Working => base.fg(Color::Blue),
        TaskStatus::NeedsInput => base.fg(Color::Magenta).add_modifier(Modifier::BOLD),
        TaskStatus::Idle => base.fg(Color::Green),
    }
}

/// Push an agent row's text with its status glyph tinted by the task's mode. A row with
/// no task behind it — a trunk description — or one the clip cut before the ` :: `
/// separator has no glyph to find, and goes in as the single plain span it always was.
fn push_row_content(
    spans: &mut Vec<Span<'static>>,
    text: String,
    base: Style,
    status: Option<TaskStatus>,
) {
    // Own the three pieces before matching: the split borrows `text`, which the untinted
    // arm needs to move into a span.
    let split = status
        .zip(model::split_status_icon(&text))
        .map(|(s, (before, icon, after))| {
            (s, before.to_string(), icon.to_string(), after.to_string())
        });
    match split {
        Some((status, before, icon, after)) => {
            spans.push(Span::styled(before, base));
            spans.push(Span::styled(icon, status_style(base, status)));
            spans.push(Span::styled(after, base));
        }
        None => spans.push(Span::styled(text, base)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Autonomy;
    use crate::graph;
    use crate::jj;
    use crate::store::Store;
    use ratatui::backend::TestBackend;

    // Build a minimal App with an in-memory store (no jj/wezterm needed).
    // Build a minimal App with no engine behind it (no jj/wezterm needed). The command
    // receiver is dropped; tests that check what was sent use `test_app_with_engine_rx`.
    fn test_app() -> App {
        test_app_with_engine_rx().0
    }

    fn test_app_with_engine_rx() -> (App, Receiver<Cmd>) {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (_msg_tx, msg_rx) = mpsc::channel();
        let app = App {
            repo: PathBuf::from("/tmp/repo"),
            view: View::default(),
            cmd_tx,
            msg_rx,
            in_flight: VecDeque::new(),
            selected: 0,
            notices: notify::Notices::default(),
            overlay: Overlay::None,
            body: Body::Agents,
            log_selected: None,
            spin_epoch: Instant::now(),
            should_quit: false,
            quit_now: false,
            pending_swap: None,
            pending_rebase: None,
            pending_describe: None,
        };
        (app, cmd_rx)
    }

    /// The rendered buffer as newline-joined rows, for substring assertions.
    fn buffer_text(term: &Terminal<TestBackend>) -> String {
        let buf = term.backend().buffer();
        let w = buf.area.width as usize;
        buf.content
            .chunks(w)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn draw(app: &mut App, w: u16, h: u16) -> Terminal<TestBackend> {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| app.render(f)).unwrap();
        term
    }

    #[test]
    fn notice_is_visible_in_a_narrow_docked_pane() {
        // The reported bug: docked beside a Claude pane faff gets ~60 columns, and the
        // notice used to start at column 118. This fails against the pre-Task-3 layout.
        let mut app = test_app();
        app.notices.ok("snapshotted #7");
        let term = draw(&mut app, 60, 12);
        assert!(
            buffer_text(&term).contains("snapshotted #7"),
            "the notice must be on screen at 60 columns:\n{}",
            buffer_text(&term)
        );
    }

    #[test]
    fn notice_carries_its_level_glyph() {
        let mut app = test_app();
        app.notices.err("snapshot failed: boom");
        let term = draw(&mut app, 60, 12);
        let text = buffer_text(&term);
        assert!(text.contains("✗ snapshot failed: boom"), "got:\n{text}");
    }

    #[test]
    fn a_long_error_wraps_instead_of_being_clipped() {
        let mut app = test_app();
        app.notices
            .err("snapshot failed: jj util snapshot: No such file or directory (os error 2)");
        let term = draw(&mut app, 60, 14);
        let text = buffer_text(&term);
        assert!(text.contains("No such file"), "first line present:\n{text}");
        assert!(
            text.contains("(os error 2)"),
            "the tail must wrap onto a second line rather than being clipped:\n{text}"
        );
    }

    #[test]
    fn notice_height_is_one_for_a_short_note() {
        let mut app = test_app();
        app.notices.ok("snapshotted #7");
        assert_eq!(app.notice_height(60), 1);
    }

    #[test]
    fn notice_height_is_capped_at_three_lines() {
        let mut app = test_app();
        app.notices.err("word ".repeat(200));
        assert_eq!(
            app.notice_height(40),
            NOTICE_MAX_LINES,
            "an enormous error must not eat the graph"
        );
    }

    #[test]
    fn notice_line_is_reserved_even_with_no_notice() {
        // Nothing has been pushed: the row still exists, so the graph does not jitter
        // by one line the first time a notice appears.
        let app = test_app();
        assert_eq!(app.notice_height(60), 1);
    }

    #[test]
    fn verbose_key_row_is_used_when_it_fits() {
        let mut app = test_app();
        let w = app.keys_line(true).width() as u16;
        let term = draw(&mut app, w, 12);
        assert!(
            buffer_text(&term).contains("N handoff"),
            "verbose labels shown"
        );
    }

    #[test]
    fn compact_key_row_is_used_when_verbose_does_not_fit() {
        let mut app = test_app();
        let w = app.keys_line(true).width() as u16 - 1;
        let term = draw(&mut app, w, 12);
        let text = buffer_text(&term);
        assert!(
            !text.contains("N handoff"),
            "verbose labels dropped:\n{text}"
        );
        assert!(text.contains("rev sSrRd"), "compact group shown:\n{text}");
    }

    #[test]
    fn compact_key_row_fits_a_docked_pane() {
        let app = test_app();
        assert!(
            app.keys_line(false).width() <= 60,
            "compact form must fit 60 columns, got {}",
            app.keys_line(false).width()
        );
    }

    #[test]
    fn every_action_key_appears_in_the_toolbar_data() {
        // Guards against adding a binding to input.rs that never surfaces in the UI —
        // exactly how `R` went undocumented.
        for key in [
            "n", "N", "↵", "s", "S", "r", "R", "d", "a", "A", "x", "X", "⇥", "q",
        ] {
            assert!(
                HINTS.iter().any(|h| h.key == key),
                "key {key} is bound in input.rs but missing from HINTS"
            );
        }
    }

    #[test]
    fn enter_label_flips_between_open_and_detach() {
        // `docked_node_app` sets up the graph rows but not the selection (its other
        // callers read `task_of_node`), and the ↵ label keys off the *selected* task.
        let select = |app: &mut App, id: TaskId| {
            app.view.task_order = vec![id];
            app.selected = 0;
        };
        let (mut app, id) = docked_node_app(true);
        select(&mut app, id);
        let w = app.keys_line(true).width() as u16;
        assert!(buffer_text(&draw(&mut app, w, 12)).contains("↵ detach"));
        let (mut app, id) = docked_node_app(false);
        select(&mut app, id);
        let w = app.keys_line(true).width() as u16;
        assert!(buffer_text(&draw(&mut app, w, 12)).contains("↵ open"));
    }

    #[test]
    fn keys_are_dimmed_when_unavailable_for_the_selected_row() {
        // A task with no workspace: `s`/`S` are dimmed, `n` is not.
        let store = Store::open_memory().unwrap();
        let mut app = test_app();
        let t = store.create_task("x", 0, Autonomy::Inherit).unwrap();
        app.view.tasks = store.list_tasks().unwrap();
        app.view.task_order = vec![t.id];
        app.selected = 0;
        let c = app.caps();
        assert!(!c.has_ws, "fixture has no workspace");
        assert!(!App::hint_available("s", &c), "swap needs a workspace");
        assert!(!App::hint_available("S", &c), "snapshot needs a workspace");
        assert!(App::hint_available("n", &c), "new task is always available");
        assert!(
            !App::hint_available("A", &c),
            "abort needs a non-empty train"
        );
    }

    #[test]
    fn dimming_renders_as_a_dim_modifier_not_a_missing_key() {
        let mut app = test_app();
        let w = app.keys_line(true).width() as u16;
        let term = draw(&mut app, w, 12);
        // The key is still on screen — dimming must never shift positions.
        assert!(buffer_text(&term).contains("s swap"));
    }

    #[test]
    fn bang_opens_the_log_and_any_key_closes_it() {
        let mut app = test_app();
        app.notices.ok("snapshotted #7");
        app.handle_key(key(KeyCode::Char('!')));
        assert_eq!(app.overlay, Overlay::Log);
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.overlay, Overlay::None, "any key closes the overlay");
    }

    #[test]
    fn opening_the_log_clears_the_unseen_badge() {
        let mut app = test_app();
        app.notices.err("boom");
        assert_eq!(app.notices.unseen(), 1);
        app.handle_key(key(KeyCode::Char('!')));
        assert_eq!(app.notices.unseen(), 0);
    }

    #[test]
    fn a_key_pressed_while_the_overlay_is_open_does_not_also_act() {
        // `q` must close the overlay, not quit faff.
        let mut app = test_app();
        app.handle_key(key(KeyCode::Char('!')));
        app.handle_key(key(KeyCode::Char('q')));
        assert_eq!(app.overlay, Overlay::None);
        assert!(
            !app.should_quit,
            "q closed the overlay rather than quitting"
        );
    }

    #[test]
    fn log_overlay_lists_notices_newest_first() {
        let mut app = test_app();
        app.notices.ok("swapped @ ⇄ #7");
        app.notices.err("snapshot failed: boom");
        app.overlay = Overlay::Log;
        let text = buffer_text(&draw(&mut app, 70, 16));
        let failed = text.find("snapshot failed").expect("error listed");
        let swapped = text.find("swapped").expect("success listed");
        assert!(failed < swapped, "newest first:\n{text}");
    }

    #[test]
    fn help_overlay_lists_bindings_the_toolbar_omits() {
        let mut app = test_app();
        app.overlay = Overlay::Help;
        let text = buffer_text(&draw(&mut app, 70, 20));
        assert!(text.contains("j"), "j/k navigation documented:\n{text}");
        assert!(text.contains("onto"), "R documented:\n{text}");
    }

    #[test]
    fn unseen_badge_shows_the_count_in_the_key_row() {
        let mut app = test_app();
        app.notices.err("boom");
        app.notices.err("boom again");
        let w = app.keys_line(true).width() as u16;
        let text = buffer_text(&draw(&mut app, w, 12));
        assert!(text.contains("!2"), "badge shows the unseen count:\n{text}");
    }

    #[test]
    fn badge_disappears_once_the_log_is_read() {
        let mut app = test_app();
        app.notices.err("boom");
        app.handle_key(key(KeyCode::Char('!')));
        app.overlay = Overlay::None;
        let w = app.keys_line(true).width() as u16;
        let text = buffer_text(&draw(&mut app, w, 12));
        assert!(!text.contains("!1"), "badge cleared:\n{text}");
    }

    #[test]
    fn an_armed_confirmation_still_swallows_the_bang_key() {
        // Pre-existing contract: while a swap is armed, any non-`s` key cancels. `!`
        // must cancel, not open the log over a live confirmation.
        let store = Store::open_memory().unwrap();
        let mut app = test_app();
        let t = store.create_task("x", 0, Autonomy::Inherit).unwrap();
        store
            .set_workspace(
                t.id,
                "faf-task-1",
                std::path::Path::new("/nope/ws"),
                "c1",
                "f1",
            )
            .unwrap();
        store.update_status(t.id, TaskStatus::Working).unwrap();
        app.view.tasks = store.list_tasks().unwrap();
        app.view.task_order = vec![t.id];
        app.selected = 0;

        app.swap_selected();
        assert_eq!(app.pending_swap, Some(t.id));
        app.handle_key(key(KeyCode::Char('!')));
        assert_eq!(app.overlay, Overlay::None, "the log did not open");
        assert_eq!(app.pending_swap, None, "the confirmation was cancelled");
    }

    /// The current notice as `(level, text)` — the shape the migrated assertions want.
    fn latest(app: &App) -> (notify::Level, String) {
        let n = app.notices.latest().expect("a notice was pushed");
        (n.level, n.text.clone())
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
        let store = Store::open_memory().unwrap();
        let mut app = test_app();
        let t = store.create_task("x", 0, Autonomy::Inherit).unwrap();
        store
            .set_workspace(
                t.id,
                "faf-task-1",
                std::path::Path::new("/nope/ws"),
                "c1",
                "f1",
            )
            .unwrap();
        store.update_status(t.id, TaskStatus::Working).unwrap();
        app.view.tasks = store.list_tasks().unwrap();
        app.view.task_order = vec![t.id];
        app.selected = 0;

        app.swap_selected();
        assert_eq!(
            app.pending_swap,
            Some(t.id),
            "first s arms the confirmation"
        );
        let (level, text) = latest(&app);
        assert_eq!(level, notify::Level::Prompt);
        assert!(text.contains("press s to confirm"));

        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.pending_swap, None, "a non-s key cancels");
        assert_eq!(
            latest(&app),
            (notify::Level::Info, "swap cancelled".to_string())
        );
    }

    #[test]
    fn rebase_on_working_agent_arms_then_cancels() {
        // `r` on a working agent must not send anything; it only arms a confirmation.
        // A non-matching key then cancels and clears the pending state.
        let store = Store::open_memory().unwrap();
        let mut app = test_app();
        let t = store.create_task("x", 0, Autonomy::Inherit).unwrap();
        store
            .set_workspace(
                t.id,
                "faf-task-1",
                std::path::Path::new("/nope/ws"),
                "c1",
                "f1",
            )
            .unwrap();
        store.set_pane(t.id, Some(42)).unwrap();
        store.update_status(t.id, TaskStatus::Working).unwrap();
        app.view.tasks = store.list_tasks().unwrap();
        app.view.task_order = vec![t.id];
        app.selected = 0;

        app.rebase_selected(true);
        assert_eq!(
            app.pending_rebase,
            Some((t.id, true)),
            "first r arms the confirmation"
        );
        let (level, text) = latest(&app);
        assert_eq!(level, notify::Level::Prompt);
        assert!(text.contains("press r to confirm"));

        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.pending_rebase, None, "a non-r key cancels");
        assert_eq!(
            latest(&app),
            (notify::Level::Info, "rebase cancelled".to_string())
        );
    }

    #[test]
    fn rebase_before_first_prompt_is_allowed() {
        // `r`/`R` needs no prompt of its own: on a working agent it arms the confirmation.
        let store = Store::open_memory().unwrap();
        let mut app = test_app();
        let t = store.create_task("", 0, Autonomy::Inherit).unwrap();
        store
            .set_workspace(
                t.id,
                "faf-task-1",
                std::path::Path::new("/nope/ws"),
                "c1",
                "f1",
            )
            .unwrap();
        store.set_pane(t.id, Some(42)).unwrap();
        store.update_status(t.id, TaskStatus::Working).unwrap();
        app.view.tasks = store.list_tasks().unwrap();
        app.view.task_order = vec![t.id];
        app.selected = 0;

        app.rebase_selected(true);
        assert_eq!(app.pending_rebase, Some((t.id, true)), "arms without a prompt");
    }

    #[test]
    fn describe_on_working_agent_arms_then_cancels() {
        // `d` on a working agent must not send anything; it only arms a confirmation.
        // A non-`d` key then cancels and clears the pending state.
        let store = Store::open_memory().unwrap();
        let mut app = test_app();
        let t = store.create_task("x", 0, Autonomy::Inherit).unwrap();
        store
            .set_workspace(
                t.id,
                "faf-task-1",
                std::path::Path::new("/nope/ws"),
                "c1",
                "f1",
            )
            .unwrap();
        store.set_pane(t.id, Some(42)).unwrap();
        store.update_status(t.id, TaskStatus::Working).unwrap();
        app.view.tasks = store.list_tasks().unwrap();
        app.view.task_order = vec![t.id];
        app.selected = 0;

        app.describe_selected();
        assert_eq!(
            app.pending_describe,
            Some(t.id),
            "first d arms the confirmation"
        );
        let (level, text) = latest(&app);
        assert_eq!(level, notify::Level::Prompt);
        assert!(text.contains("press d to confirm"));

        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.pending_describe, None, "a non-d key cancels");
        assert_eq!(
            latest(&app),
            (notify::Level::Info, "describe cancelled".to_string())
        );
    }

    #[test]
    fn describe_before_first_prompt_is_allowed() {
        // The scheduler never captures the describe prompt as the task's title, so `d`
        // needs no prompt of its own: on a working agent it arms the confirmation.
        let store = Store::open_memory().unwrap();
        let mut app = test_app();
        let t = store.create_task("", 0, Autonomy::Inherit).unwrap();
        store
            .set_workspace(
                t.id,
                "faf-task-1",
                std::path::Path::new("/nope/ws"),
                "c1",
                "f1",
            )
            .unwrap();
        store.set_pane(t.id, Some(42)).unwrap();
        store.update_status(t.id, TaskStatus::Working).unwrap();
        app.view.tasks = store.list_tasks().unwrap();
        app.view.task_order = vec![t.id];
        app.selected = 0;

        app.describe_selected();
        assert_eq!(app.pending_describe, Some(t.id), "arms without a prompt");
    }

    // Build a selectable task with a workspace + pane + prompt (the accept happy-path
    // prerequisites bar the jj checks, which need a real repo).
    fn task_with_pane(app: &mut App, prompt: &str) -> TaskId {
        let store = Store::open_memory().unwrap();
        let t = store.create_task(prompt, 0, Autonomy::Inherit).unwrap();
        store
            .set_workspace(
                t.id,
                "faf-task-1",
                std::path::Path::new("/nope/ws"),
                "c1",
                "f1",
            )
            .unwrap();
        store.set_pane(t.id, Some(42)).unwrap();
        app.view.tasks = store.list_tasks().unwrap();
        app.view.task_order = vec![t.id];
        app.selected = 0;
        t.id
    }

    #[test]
    fn accept_without_pane_is_blocked() {
        // No live pane means the train can't drive the agent (describe/rebase prompts).
        let store = Store::open_memory().unwrap();
        let mut app = test_app();
        let t = store.create_task("do x", 0, Autonomy::Inherit).unwrap();
        store
            .set_workspace(
                t.id,
                "faf-task-1",
                std::path::Path::new("/nope/ws"),
                "c1",
                "f1",
            )
            .unwrap();
        app.view.tasks = store.list_tasks().unwrap();
        app.view.task_order = vec![t.id];
        app.selected = 0;

        app.accept_selected();
        assert!(app.view.merge_train.is_empty());
        let (level, text) = latest(&app);
        assert_eq!(level, notify::Level::Warn, "refused, not failed: {text}");
        assert!(text.contains("no live pane"), "notice: {text}");
    }

    #[test]
    fn merge_train_panel_sizes_and_renders() {
        let mut app = test_app();
        assert_eq!(
            app.merge_train_height(),
            0,
            "no panel when the train is empty"
        );

        let id = task_with_pane(&mut app, "Convert bridges to JSON");
        app.view.merge_train.add(id);
        if let Some(m) = app.view.merge_train.member_mut(id) {
            m.stage = train::Stage::Rebasing;
        }
        app.view.merge_train.note = "paused — your @ has content".into();
        // title/border + note + one member = 3 rows.
        assert_eq!(app.merge_train_height(), 3);

        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| app.render(f)).unwrap(); // must not panic with the panel present
    }

    #[test]
    fn header_notice_and_keys_render_without_panicking() {
        let store = Store::open_memory().unwrap();
        let mut app = test_app();
        app.view.tasks = vec![
            store
                .create_task("do a thing", 0, Autonomy::AcceptEdits)
                .unwrap(),
        ];
        let backend = TestBackend::new(80, 20);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| app.render(f)).unwrap();
        // If we got here, layout + widgets rendered without panic.
    }

    #[test]
    fn graph_renders_id_column_with_prefix() {
        let mut app = test_app();
        app.view.rows = vec![graph::GraphRow {
            gutter: "@".into(),
            content: "(no description set)".into(),
            node_index: Some(0),
            change_id: Some("abcd1234efgh".into()),
        }];
        app.view.task_of_node = vec![None];
        app.view.id_display = std::collections::HashMap::from([(
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
    fn graph_renders_fork_point_diamond_in_cyan() {
        // The fork-point node carries a ◆; the view tints it cyan. A non-fork `@` on
        // another row stays green.
        let mut app = test_app();
        app.view.rows = vec![
            graph::GraphRow {
                gutter: "@".into(),
                content: "(no description set)".into(),
                node_index: Some(0),
                change_id: Some("wcwcwcwc".into()),
            },
            graph::GraphRow {
                gutter: "◆".into(),
                content: "base".into(),
                node_index: Some(1),
                change_id: Some("basebase".into()),
            },
        ];
        app.view.task_of_node = vec![None, None];
        app.view.id_display = std::collections::HashMap::from([
            (
                "wcwcwcwc".to_string(),
                ("wcwcwcwc".to_string(), String::new()),
            ),
            (
                "basebase".to_string(),
                ("basebase".to_string(), String::new()),
            ),
        ]);
        app.view.fork_point = Some("basebase".to_string());

        let backend = TestBackend::new(80, 10);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| app.render(f)).unwrap();
        let buf = term.backend().buffer();
        let diamond = buf
            .content
            .iter()
            .find(|c| c.symbol() == "◆")
            .expect("◆ glyph rendered");
        assert_eq!(diamond.fg, Color::Cyan, "fork-point ◆ is cyan");
        let at = buf
            .content
            .iter()
            .find(|c| c.symbol() == "@")
            .expect("@ glyph rendered");
        assert_eq!(at.fg, Color::Green, "a non-fork `@` stays green");
    }

    #[test]
    fn graph_tints_working_copy_cyan_when_it_is_the_fork_point() {
        // @ itself is the fork point: keep the @ glyph but tint it cyan, not green.
        let mut app = test_app();
        app.view.rows = vec![graph::GraphRow {
            gutter: "@".into(),
            content: "my work".into(),
            node_index: Some(0),
            change_id: Some("wcwcwcwc".into()),
        }];
        app.view.task_of_node = vec![None];
        app.view.id_display = std::collections::HashMap::from([(
            "wcwcwcwc".to_string(),
            ("wcwcwcwc".to_string(), String::new()),
        )]);
        app.view.fork_point = Some("wcwcwcwc".to_string());

        let backend = TestBackend::new(80, 10);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| app.render(f)).unwrap();
        let buf = term.backend().buffer();
        let at = buf
            .content
            .iter()
            .find(|c| c.symbol() == "@")
            .expect("@ glyph rendered");
        assert_eq!(at.fg, Color::Cyan, "the `@` that is the fork point is cyan");
    }

    #[test]
    fn graph_aligns_id_column_across_gutter_widths() {
        // Rows of differing gutter widths must line their `[id]` column into one straight
        // vertical column: the gutter is padded to a uniform width so branch depth never
        // pushes the block sideways. Three rows of increasing gutter width:
        //   • trunk         `◻`
        //   • folded fork   `├─◻`
        //   • agent stub    `├─●`
        // Every `[id]` must start at the same x regardless.
        let mut app = test_app();
        app.view.rows = vec![
            graph::GraphRow {
                gutter: "◻".into(),
                content: "trunk work".into(),
                node_index: Some(0),
                change_id: Some("aaaaaaaa".into()),
            },
            graph::GraphRow {
                gutter: "├─◻".into(),
                content: "side work".into(),
                node_index: Some(1),
                change_id: Some("bbbbbbbb".into()),
            },
            graph::GraphRow {
                gutter: "├─●".into(),
                content: "#11 ⚙ :: agent work".into(),
                node_index: Some(2),
                change_id: Some("cccccccc".into()),
            },
        ];
        app.view.task_of_node = vec![None, None, None];
        app.view.id_display = std::collections::HashMap::from([
            (
                "aaaaaaaa".to_string(),
                ("aaaaaaaa".to_string(), String::new()),
            ),
            (
                "bbbbbbbb".to_string(),
                ("bbbbbbbb".to_string(), String::new()),
            ),
            (
                "cccccccc".to_string(),
                ("cccccccc".to_string(), String::new()),
            ),
        ]);

        let width = 80usize;
        let backend = TestBackend::new(width as u16, 10);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| app.render(f)).unwrap();

        let buf = term.backend().buffer();
        // For each rendered graph row — the ones carrying the `[abcdefgh]` id column, a
        // `[`…`]` pair spanning the 8-char id — record the x of its `[`.
        let mut bracket_xs = vec![];
        for y in 0..(buf.content.len() / width) {
            let row = &buf.content[y * width..(y + 1) * width];
            let (Some(open), Some(close)) = (
                row.iter().position(|c| c.symbol() == "["),
                row.iter().position(|c| c.symbol() == "]"),
            ) else {
                continue;
            };
            if close - open != 9 {
                continue; // not the id column (footer/header hint brackets)
            }
            bracket_xs.push(open);
        }
        assert_eq!(
            bracket_xs.len(),
            3,
            "three graph rows carry an id column: {bracket_xs:?}"
        );
        assert!(
            bracket_xs.iter().all(|x| *x == bracket_xs[0]),
            "id columns aligned: {bracket_xs:?}"
        );
    }

    // A one-row graph whose single node's session is docked beside faff.
    fn docked_node_app(open: bool) -> (App, TaskId) {
        let store = Store::open_memory().unwrap();
        let mut app = test_app();
        let t = store.create_task("x", 0, Autonomy::Inherit).unwrap();
        store.set_pane(t.id, Some(77)).unwrap();
        app.view.tasks = store.list_tasks().unwrap();
        app.view.open_pane = if open { Some(77) } else { None };
        app.view.rows = vec![graph::GraphRow {
            gutter: "@".into(),
            content: format!("#{} x", t.id.0),
            node_index: Some(0),
            change_id: Some("abcd1234".into()),
        }];
        app.view.task_of_node = vec![Some(t.id)];
        // `ab` is the unique prefix; `cd1234` the rest of the id after it.
        app.view.id_display = std::collections::HashMap::from([(
            "abcd1234".to_string(),
            ("ab".to_string(), "cd1234".to_string()),
        )]);
        (app, t.id)
    }

    // The id column renders `[` + unique prefix + rest + `] `, so relative to the `[`
    // the rest starts two cells past where a prefix-less walk would put it.
    fn id_col_x(x0: usize, dx: usize) -> usize {
        if dx == 0 { x0 } else { x0 + dx + 2 }
    }

    #[test]
    fn focused_session_greens_its_whole_revision_id() {
        let (mut app, tid) = docked_node_app(true);
        // The docked node is also the highlighted (selected) row — the common case (you
        // select a task, then dock it). A naive green foreground on the REVERSED selection
        // style would swap fg/bg and paint the cell background green, not the id glyphs;
        // the id must stay green *glyphs*.
        app.view.task_order = vec![tid];
        app.selected = 0;

        let (w, h) = (90usize, 10usize);
        let mut term = Terminal::new(TestBackend::new(w as u16, h as u16)).unwrap();
        term.draw(|f| app.render(f)).unwrap();
        let buf = term.backend().buffer();
        let row_text =
            |y: usize| -> String { (0..w).map(|x| buf.content[y * w + x].symbol()).collect() };

        // The graph node row, found by its revision id — not the header bar above it,
        // which keeps its own `▶ #id` status chip.
        let gy = (0..h)
            .find(|&y| row_text(y).contains("abcd1234"))
            .expect("graph node row rendered");
        // The docked session shows through its brackets, not a trailing marker that
        // would crowd the description off a narrow pane.
        assert!(!row_text(gy).contains('▶'), "no trailing marker on node row");
        // The brackets *and* the id's non-unique remainder are green — two thin bracket
        // glyphs on their own are too easy to miss.
        let x0 = row_text(gy).find('[').expect("bracket on node row");
        for (dx, needle) in "[cd1234]".chars().enumerate() {
            let cell = &buf.content[gy * w + id_col_x(x0, dx)];
            assert_eq!(cell.symbol(), needle.to_string(), "id column at +{dx}");
            assert_eq!(cell.fg, Color::Green, "docked id char {needle:?} is green");
            assert!(
                cell.modifier.contains(Modifier::BOLD),
                "docked id char {needle:?} is bold"
            );
            assert!(
                !cell.modifier.contains(Modifier::REVERSED),
                "docked id char {needle:?} is a green glyph, not a reversed green cell"
            );
        }
        // The unique prefix stays cyan: green and blue inside one id would read as noise.
        for (dx, needle) in "ab".chars().enumerate() {
            let cell = &buf.content[gy * w + x0 + 1 + dx];
            assert_eq!(cell.symbol(), needle.to_string(), "unique prefix at +{dx}");
            assert_eq!(cell.fg, Color::Cyan, "unique prefix char {needle:?} stays cyan");
        }
    }

    #[test]
    fn undocked_node_keeps_a_gray_revision_id() {
        let (app, _tid) = docked_node_app(false);
        let (w, h) = (90usize, 10usize);
        let mut term = Terminal::new(TestBackend::new(w as u16, h as u16)).unwrap();
        term.draw(|f| app.render(f)).unwrap();
        let buf = term.backend().buffer();
        let row_text =
            |y: usize| -> String { (0..w).map(|x| buf.content[y * w + x].symbol()).collect() };
        let gy = (0..h)
            .find(|&y| row_text(y).contains("abcd1234"))
            .expect("graph node row rendered");
        let x0 = row_text(gy).find('[').expect("bracket on node row");
        for (dx, needle) in "[cd1234]".chars().enumerate() {
            let cell = &buf.content[gy * w + id_col_x(x0, dx)];
            assert_eq!(cell.symbol(), needle.to_string(), "id column at +{dx}");
            assert_eq!(
                cell.fg,
                Color::DarkGray,
                "undocked id char {needle:?} stays gray"
            );
        }
    }

    #[test]
    fn combined_node_greens_the_head_id_not_the_agent_line() {
        // HEAD parked on the agent's revision: the `@` commit row is the HEAD header
        // (its description) and the agent hangs beneath. row_tasks hangs the task on the
        // agent line, so the docked indicator is the HEAD row's green id above it —
        // never a trailing marker on the agent line.
        let store = Store::open_memory().unwrap();
        let mut app = test_app();
        let t = store.create_task("x", 0, Autonomy::Inherit).unwrap();
        store.set_pane(t.id, Some(55)).unwrap();
        app.view.tasks = store.list_tasks().unwrap();
        app.view.open_pane = Some(55); // the agent is docked beside faff
        app.view.rows = vec![
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
        app.view.task_of_node = vec![None, Some(t.id), None];
        app.view.task_order = vec![t.id];
        app.selected = 0;

        let (w, h) = (60usize, 12usize);
        let backend = TestBackend::new(w as u16, h as u16);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| app.render(f)).unwrap();
        let buf = term.backend().buffer();
        let row_text =
            |y: usize| -> String { (0..w).map(|x| buf.content[y * w + x].symbol()).collect() };
        let head_row = (0..h)
            .find(|&y| row_text(y).contains("(no description set)"))
            .expect("HEAD header row rendered");
        let agent_row = (0..h)
            .find(|&y| row_text(y).contains("↳ #"))
            .expect("agent line rendered");
        // No trailing marker rides the graph rows — the indicator moved to HEAD's
        // brackets. (The header bar keeps its own `▶ #id` status chip.)
        assert!(!row_text(head_row).contains('▶'), "no marker on the HEAD row");
        assert!(!row_text(agent_row).contains('▶'), "no marker on the agent line");
        // HEAD's revision id is greened (bold), since its parked agent is docked.
        // The agent line itself carries no id column to colour.
        for needle in ["[", "]"] {
            let x = (0..w)
                .find(|&x| buf.content[head_row * w + x].symbol() == needle)
                .expect("bracket on HEAD row");
            let cell = &buf.content[head_row * w + x];
            assert_eq!(cell.fg, Color::Green, "HEAD bracket {needle} is green");
            assert!(
                cell.modifier.contains(Modifier::BOLD),
                "HEAD bracket {needle} is bold"
            );
        }
        // The agent line is the selected/highlighted one (reverse video), not HEAD.
        let reversed =
            |y: usize| (0..w).any(|x| buf.content[y * w + x].modifier.contains(Modifier::REVERSED));
        assert!(
            reversed(agent_row),
            "agent line is highlighted when selected"
        );
        assert!(!reversed(head_row), "HEAD header row is not highlighted");
    }

    #[test]
    fn agent_rows_tint_the_status_glyph_by_mode() {
        // The status glyphs are plain one-column marks, so colour is what separates the
        // three modes: blue working, bold magenta needs-you, green review-ready. Each must
        // land on the glyph cell alone — the `#id` and the title stay uncoloured.
        let store = Store::open_memory().unwrap();
        let mut app = test_app();
        let mut ids = vec![];
        for (prompt, status) in [
            ("migrate", TaskStatus::Working),
            ("oauth", TaskStatus::NeedsInput),
            ("cleanup", TaskStatus::Idle),
        ] {
            let t = store.create_task(prompt, 0, Autonomy::Inherit).unwrap();
            store.update_status(t.id, status).unwrap();
            ids.push(t.id);
        }
        app.view.tasks = store.list_tasks().unwrap();
        app.view.rows = vec![
            graph::GraphRow {
                gutter: "├─●".into(),
                content: "#1 ⚙ :: migrate".into(),
                node_index: Some(0),
                change_id: Some("aaaaaaaa".into()),
            },
            graph::GraphRow {
                gutter: "├─●".into(),
                content: "#2 ! :: oauth".into(),
                node_index: Some(1),
                change_id: Some("bbbbbbbb".into()),
            },
            graph::GraphRow {
                gutter: "├─●".into(),
                content: "#3 ✓ :: cleanup".into(),
                node_index: Some(2),
                change_id: Some("cccccccc".into()),
            },
        ];
        app.view.task_of_node = ids.iter().copied().map(Some).collect();
        app.view.id_display = std::collections::HashMap::from([
            (
                "aaaaaaaa".to_string(),
                ("aaaaaaaa".to_string(), String::new()),
            ),
            (
                "bbbbbbbb".to_string(),
                ("bbbbbbbb".to_string(), String::new()),
            ),
            (
                "cccccccc".to_string(),
                ("cccccccc".to_string(), String::new()),
            ),
        ]);

        let (w, h) = (80usize, 8usize);
        let mut term = Terminal::new(TestBackend::new(w as u16, h as u16)).unwrap();
        term.draw(|f| app.render(f)).unwrap();
        let buf = term.backend().buffer();
        let find = |glyph: &str| -> (usize, usize) {
            (0..w * h)
                .find(|&i| buf.content[i].symbol() == glyph)
                .map(|i| (i % w, i / w))
                .unwrap_or_else(|| panic!("{glyph} rendered"))
        };

        let (gx, gy) = find("⚙");
        assert_eq!(buf.content[gy * w + gx].fg, Color::Blue, "working is blue");
        let (bx, by) = find("!");
        let bang = &buf.content[by * w + bx];
        assert_eq!(bang.fg, Color::Magenta, "needs-you is magenta");
        assert!(bang.modifier.contains(Modifier::BOLD), "needs-you is bold");
        let (cx, cy) = find("✓");
        assert_eq!(buf.content[cy * w + cx].fg, Color::Green, "idle is green");

        // The tint is the glyph alone: the char either side of it keeps the default fg,
        // so a whole coloured row can never be mistaken for a status.
        for (x, y) in [(gx, gy), (bx, by), (cx, cy)] {
            assert_eq!(
                buf.content[y * w + x - 1].fg,
                Color::Reset,
                "space before the glyph is untinted"
            );
            assert_eq!(
                buf.content[y * w + x + 1].fg,
                Color::Reset,
                "space after the glyph is untinted"
            );
        }
    }

    #[test]
    fn trunk_rows_are_never_tinted() {
        // A plain description has no status field. Anchoring on ` :: ` must not invent one
        // — the row renders exactly as it did before colour existed.
        let mut app = test_app();
        app.view.rows = vec![graph::GraphRow {
            gutter: "◻".into(),
            content: "spawn: declare! child-class refs".into(),
            node_index: Some(0),
            change_id: Some("aaaaaaaa".into()),
        }];
        app.view.task_of_node = vec![None];
        app.view.id_display = std::collections::HashMap::from([(
            "aaaaaaaa".to_string(),
            ("aaaaaaaa".to_string(), String::new()),
        )]);

        let (w, h) = (80usize, 6usize);
        let mut term = Terminal::new(TestBackend::new(w as u16, h as u16)).unwrap();
        term.draw(|f| app.render(f)).unwrap();
        let buf = term.backend().buffer();
        let bang = (0..w * h)
            .find(|&i| buf.content[i].symbol() == "!")
            .expect("the `!` in the description is rendered");
        assert_eq!(
            buf.content[bang].fg,
            Color::Reset,
            "a `!` inside a description is not a status glyph"
        );
    }

    #[test]
    fn docked_detached_task_greens_its_id() {
        // A detached (integrated / no-node) task has no revision brackets, so when its
        // session is docked the `#N` id is greened instead — still a left-anchored
        // indicator that a truncated description can never push off screen.
        let store = Store::open_memory().unwrap();
        let mut app = test_app();
        let t = store
            .create_task("cleanup", 0, Autonomy::Inherit)
            .unwrap();
        store.set_pane(t.id, Some(88)).unwrap();
        app.view.tasks = store.list_tasks().unwrap();
        app.view.open_pane = Some(88); // docked, but this task has no graph node
        app.view.detached = vec![t.id];
        // ...and it is the highlighted (selected) row too, so the green prefix must be a
        // glyph colour, not a REVERSED swap that would paint the cell background green.
        app.view.task_order = vec![t.id];
        app.selected = 0;

        let (w, h) = (60usize, 8usize);
        let mut term = Terminal::new(TestBackend::new(w as u16, h as u16)).unwrap();
        term.draw(|f| app.render(f)).unwrap();
        let buf = term.backend().buffer();
        let row_text =
            |y: usize| -> String { (0..w).map(|x| buf.content[y * w + x].symbol()).collect() };

        // The detached row, by its `· #` bullet — not the header's `▶ #id` chip.
        let dy = (0..h)
            .find(|&y| row_text(y).contains("· #"))
            .expect("detached row rendered");
        assert!(
            !row_text(dy).contains('▶'),
            "no trailing marker on detached row"
        );
        let x = (0..w)
            .find(|&x| buf.content[dy * w + x].symbol() == "#")
            .expect("# on detached row");
        let cell = &buf.content[dy * w + x];
        assert_eq!(cell.fg, Color::Green, "docked detached #id is green");
        assert!(
            cell.modifier.contains(Modifier::BOLD),
            "docked detached #id is bold"
        );
        assert!(
            !cell.modifier.contains(Modifier::REVERSED),
            "docked detached #id is a green glyph, not a reversed green cell"
        );
    }

    #[test]
    fn help_overlay_shows_every_binding_even_in_a_narrow_pane() {
        // The keymap is the authoritative reference; it must never silently clip one.
        let mut app = test_app();
        app.overlay = Overlay::Help;
        let text = buffer_text(&draw(&mut app, 46, 20));
        for verbose in HINTS.iter().map(|h| h.verbose) {
            assert!(
                text.contains(verbose),
                "`{verbose}` clipped at 46 cols:\n{text}"
            );
        }
    }
    fn log_rev(id: &str, parents: &[&str], cwc: bool, empty: bool, desc: &str) -> jj::RevInfo {
        jj::RevInfo {
            change_id: id.into(),
            parents: parents.iter().map(|s| s.to_string()).collect(),
            is_current_wc: cwc,
            empty,
            conflict: false,
            id_prefix: id.chars().take(1).collect(),
            id_rest: id.chars().skip(1).collect(),
            description: desc.into(),
        }
    }

    /// Hand the app the view the engine would publish for `revs`.
    fn apply_revs(app: &mut App, revs: Vec<jj::RevInfo>, ws: &[jj::Workspace]) {
        let g = engine::Graphs::build(revs, ws, &app.view.tasks);
        app.apply_view(View {
            tasks: app.view.tasks.clone(),
            task_order: app.view.task_order.clone(),
            rows: g.rows,
            task_of_node: g.task_of_node,
            log_rows: g.log_rows,
            log_task_of: g.log_task_of,
            working_copy: g.working_copy,
            id_display: g.id_display,
            fork_point: g.fork_point,
            ..View::default()
        });
    }

    fn default_ws(change_id: &str) -> Vec<jj::Workspace> {
        vec![jj::Workspace {
            name: "default".into(),
            change_id: change_id.into(),
        }]
    }

    /// `@` on an empty fork point `fk` (collapsed in the agent view) over a described base.
    fn app_with_log_history() -> App {
        let mut app = test_app();
        let revs = vec![
            log_rev("mp", &["fk"], true, false, "my work"),
            log_rev("fk", &["base"], false, true, ""),
            log_rev("base", &[], false, false, "base commit"),
        ];
        apply_revs(&mut app, revs, &default_ws("mp"));
        app
    }

    fn press(app: &mut App, code: KeyCode) {
        app.handle_key(key(code));
    }

    #[test]
    fn tab_toggles_between_the_agent_and_log_views() {
        let mut app = app_with_log_history();
        // The active view is named at the far right of the header line.
        let header = |app: &mut App| {
            let text = buffer_text(&draw(app, 80, 12));
            text.lines().next().unwrap().trim_end().to_string()
        };
        assert!(header(&mut app).ends_with("agents"), "{}", header(&mut app));
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.body, Body::Log);
        assert!(header(&mut app).ends_with("log"), "{}", header(&mut app));
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.body, Body::Agents);
    }

    #[test]
    fn log_view_shows_the_empty_revisions_the_agent_view_folds_away() {
        let mut app = app_with_log_history();
        let agents = buffer_text(&draw(&mut app, 80, 12));
        assert!(
            !agents.contains("(empty)"),
            "agent view collapses fk:\n{agents}"
        );
        press(&mut app, KeyCode::Tab);
        let log = buffer_text(&draw(&mut app, 80, 12));
        assert!(
            log.contains("(empty) (no description set)"),
            "log shows fk:\n{log}"
        );
        assert!(
            log.contains("my work") && log.contains("base commit"),
            "{log}"
        );
    }

    #[test]
    fn log_selection_starts_on_the_working_copy_and_walks_every_revision() {
        let mut app = app_with_log_history();
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.log_selected.as_deref(), Some("mp"));
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.log_selected.as_deref(), Some("fk"));
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.log_selected.as_deref(), Some("base"));
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(
            app.log_selected.as_deref(),
            Some("mp"),
            "wraps like the agent view"
        );
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(app.log_selected.as_deref(), Some("base"));
    }

    #[test]
    fn log_selection_survives_a_new_view_by_change_id() {
        let mut app = app_with_log_history();
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.log_selected.as_deref(), Some("fk"));
        // A new revision lands on top: the selection follows `fk`, not its old row.
        let revs = vec![
            log_rev("nw", &["mp"], true, false, "newer"),
            log_rev("mp", &["fk"], false, false, "my work"),
            log_rev("fk", &["base"], false, true, ""),
            log_rev("base", &[], false, false, "base commit"),
        ];
        apply_revs(&mut app, revs, &default_ws("nw"));
        assert_eq!(app.log_selected.as_deref(), Some("fk"));
        // …and a vanished one falls back to the row that took its place.
        let revs = vec![
            log_rev("nw", &["mp"], true, false, "newer"),
            log_rev("mp", &["base"], false, false, "my work"),
            log_rev("base", &[], false, false, "base commit"),
        ];
        apply_revs(&mut app, revs, &default_ws("nw"));
        assert_eq!(app.log_selected.as_deref(), Some("base"));
    }

    #[test]
    fn task_keys_are_dimmed_and_refused_in_the_log_view() {
        let (mut app, rx) = test_app_with_engine_rx();
        task_with_pane(&mut app, "do the thing");
        press(&mut app, KeyCode::Tab);
        let caps = app.caps();
        for k in ["↵", "s", "S", "r", "R", "a", "x", "X"] {
            assert!(
                !App::hint_available(k, &caps),
                "{k} should dim in the log view"
            );
        }
        for k in ["n", "N", "d", "⇥", "q", "?", "!"] {
            assert!(
                App::hint_available(k, &caps),
                "{k} still applies in the log view"
            );
        }
        press(&mut app, KeyCode::Char('x'));
        assert!(sent(&rx).is_empty(), "x asked the engine for nothing");
        assert_eq!(latest(&app).0, notify::Level::Warn);
    }

    /// [`app_with_log_history`] in the log view with `fk` (the empty fork point) selected,
    /// plus what the app sends the engine.
    fn log_view_on_fk() -> (App, Receiver<Cmd>) {
        let (mut app, rx) = test_app_with_engine_rx();
        let revs = vec![
            log_rev("mp", &["fk"], true, false, "my work"),
            log_rev("fk", &["base"], false, true, ""),
            log_rev("base", &[], false, false, "base commit"),
        ];
        apply_revs(&mut app, revs, &default_ws("mp"));
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.log_selected.as_deref(), Some("fk"));
        (app, rx)
    }

    #[test]
    fn n_in_the_log_view_forks_a_task_from_the_selected_revision() {
        let (mut app, rx) = log_view_on_fk();
        press(&mut app, KeyCode::Char('n'));
        assert_eq!(sent(&rx), vec![Cmd::New(Base::At("fk".into()))]);
    }

    #[test]
    fn shift_n_in_the_log_view_hands_off_the_selected_revision() {
        let (mut app, rx) = log_view_on_fk();
        press(&mut app, KeyCode::Char('N'));
        assert_eq!(sent(&rx), vec![Cmd::New(Base::Continue("fk".into()))]);
    }

    #[test]
    fn n_in_the_agent_view_still_forks_from_the_fork_point() {
        let (mut app, rx) = log_view_on_fk();
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Char('n'));
        press(&mut app, KeyCode::Char('N'));
        assert_eq!(
            sent(&rx),
            vec![Cmd::New(Base::ForkPoint), Cmd::New(Base::Handoff)]
        );
    }

    #[test]
    fn d_in_the_log_view_describes_a_plain_revision_headless() {
        let (mut app, rx) = log_view_on_fk();
        press(&mut app, KeyCode::Char('k'));
        press(&mut app, KeyCode::Char('d'));
        assert_eq!(sent(&rx), vec![Cmd::DescribeRev("mp".into())]);
    }

    #[test]
    fn d_in_the_log_view_on_an_agents_revision_asks_that_agent() {
        // Rewriting an agent's working copy from outside would leave its workspace stale,
        // so its own agent is asked, exactly as `d` does in the agent view.
        let (mut app, rx) = test_app_with_engine_rx();
        let id = task_with_pane(&mut app, "do the thing");
        let revs = vec![
            log_rev("mp", &["base"], true, false, "my work"),
            log_rev("ag", &["base"], false, false, "agent work"),
            log_rev("base", &[], false, false, "base commit"),
        ];
        let ws = vec![
            jj::Workspace {
                name: "default".into(),
                change_id: "mp".into(),
            },
            jj::Workspace {
                name: "faf-task-1".into(),
                change_id: "ag".into(),
            },
        ];
        apply_revs(&mut app, revs, &ws);
        press(&mut app, KeyCode::Tab);
        while app.log_selected.as_deref() != Some("ag") {
            press(&mut app, KeyCode::Char('j'));
        }
        press(&mut app, KeyCode::Char('d'));
        assert_eq!(sent(&rx), vec![Cmd::Describe(id)]);
    }

    /// The text of the first rendered line containing `needle`.
    fn line_with(text: &str, needle: &str) -> String {
        text.lines()
            .find(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no line with {needle:?} in:\n{text}"))
            .to_string()
    }

    #[test]
    fn a_revision_being_described_spins_in_both_views() {
        let (mut app, _rx) = log_view_on_fk();
        let mut view = app.view.clone();
        view.describing = ["fk".to_string(), "mp".to_string()].into();
        app.apply_view(view);
        let spins = |l: &str| SPINNER.iter().any(|c| l.contains(*c));

        let log = buffer_text(&draw(&mut app, 80, 12));
        assert!(spins(&line_with(&log, "(no description set)")), "{log}");
        assert!(!spins(&line_with(&log, "base commit")), "{log}");
        press(&mut app, KeyCode::Tab);
        let agents = buffer_text(&draw(&mut app, 80, 12));
        assert!(spins(&line_with(&agents, "my work")), "{agents}");

        // The engine reports it done: the spinner goes.
        let mut view = app.view.clone();
        view.describing.clear();
        app.apply_view(view);
        let agents = buffer_text(&draw(&mut app, 80, 12));
        assert!(!spins(&line_with(&agents, "my work")), "{agents}");
    }

    #[test]
    fn a_long_log_scrolls_to_keep_the_selection_in_view() {
        let mut app = test_app();
        let n = 40;
        let ids: Vec<String> = (0..n).map(|i| format!("r{i:02}")).collect();
        let revs: Vec<jj::RevInfo> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                let parents: Vec<&str> = ids.get(i + 1).map(|p| p.as_str()).into_iter().collect();
                log_rev(id, &parents, i == 0, false, &format!("commit {id}"))
            })
            .collect();
        apply_revs(&mut app, revs, &default_ws("r00"));
        press(&mut app, KeyCode::Tab);
        for _ in 0..30 {
            press(&mut app, KeyCode::Char('j'));
        }
        let text = buffer_text(&draw(&mut app, 80, 12));
        assert!(
            text.contains("commit r30"),
            "selection scrolled into view:\n{text}"
        );
        assert!(!text.contains("commit r00"), "top scrolled away:\n{text}");
        // Walking back up scrolls back up.
        for _ in 0..30 {
            press(&mut app, KeyCode::Char('k'));
        }
        let text = buffer_text(&draw(&mut app, 80, 12));
        assert!(text.contains("commit r00"), "{text}");
    }

    #[test]
    fn renders_with_an_active_merge_train() {
        // The notice line added a layout row, pushing the merge-train panel from
        // chunks[3] to chunks[4]. An off-by-one here is an index panic at runtime.
        let store = Store::open_memory().unwrap();
        let mut app = test_app();
        let t = store.create_task("x", 0, Autonomy::Inherit).unwrap();
        app.view.tasks = store.list_tasks().unwrap();
        app.view.task_order = vec![t.id];
        app.view.merge_train.add(t.id);
        app.notices.ok("snapshotted #7");
        assert!(!app.view.merge_train.is_empty(), "fixture has a queued member");
        assert!(app.merge_train_height() > 0, "the panel is drawn");
        let text = buffer_text(&draw(&mut app, 62, 14));
        assert!(
            text.contains("merge train"),
            "train panel rendered:\n{text}"
        );
        assert!(
            text.contains("snapshotted #7"),
            "notice still visible:\n{text}"
        );
    }

    /// Drain what the app sent the engine.
    fn sent(rx: &Receiver<Cmd>) -> Vec<Cmd> {
        rx.try_iter().collect()
    }

    #[test]
    fn remove_goes_to_the_engine_and_shows_as_busy() {
        // `x` must not do the teardown itself — that is what used to freeze the UI while a
        // workspace's `target/` was deleted. It queues the command and says so in the header.
        let store = Store::open_memory().unwrap();
        let (mut app, rx) = test_app_with_engine_rx();
        let t = store.create_task("x", 0, Autonomy::Inherit).unwrap();
        app.view.tasks = store.list_tasks().unwrap();
        app.view.task_order = vec![t.id];

        app.handle_key(key(KeyCode::Char('x')));
        assert_eq!(
            sent(&rx),
            vec![Cmd::Remove {
                id: t.id,
                discard: false
            }]
        );
        let text = buffer_text(&draw(&mut app, 80, 10));
        assert!(text.contains(&format!("⋯ removing #{}", t.id.0)), "got:\n{text}");
    }

    #[test]
    fn done_clears_the_oldest_busy_label() {
        let (msg_tx, msg_rx) = mpsc::channel();
        let (mut app, _rx) = test_app_with_engine_rx();
        app.msg_rx = msg_rx;
        app.handle_key(key(KeyCode::Char('n')));
        app.handle_key(key(KeyCode::Char('N')));
        assert_eq!(app.in_flight, ["creating a task", "handing off"]);

        msg_tx.send(Msg::Done).unwrap();
        assert!(app.drain_messages().unwrap());
        assert_eq!(app.in_flight, ["handing off"]);
    }

    #[test]
    fn a_dead_engine_is_an_error_not_a_silent_ui() {
        let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
        let mut app = test_app();
        app.msg_rx = msg_rx;
        drop(msg_tx);
        assert!(app.drain_messages().is_err());
    }

    #[test]
    fn engine_notices_land_in_the_log() {
        let (msg_tx, msg_rx) = mpsc::channel();
        let mut app = test_app();
        app.msg_rx = msg_rx;
        msg_tx
            .send(Msg::Notice(notify::Level::Error, "swap failed: boom".into()))
            .unwrap();
        app.drain_messages().unwrap();
        assert_eq!(
            latest(&app),
            (notify::Level::Error, "swap failed: boom".to_string())
        );
    }

    #[test]
    fn quit_waits_for_in_flight_work_unless_pressed_twice() {
        let (mut app, _rx) = test_app_with_engine_rx();
        app.handle_key(key(KeyCode::Char('n')));
        app.handle_key(key(KeyCode::Char('q')));
        assert!(app.should_quit && !app.quit_now, "waits for the create");
        assert_eq!(latest(&app).0, notify::Level::Prompt);
        app.handle_key(key(KeyCode::Char('q')));
        assert!(app.quit_now, "a second q quits regardless");
    }

    #[test]
    fn quit_is_immediate_when_idle() {
        let mut app = test_app();
        app.handle_key(key(KeyCode::Char('q')));
        assert!(app.quit_now);
    }

    #[test]
    fn a_created_task_takes_the_selection() {
        // The engine names the task `n`/`N` just made; the next view must move the highlight
        // onto that new row, not keep it on whatever was selected before.
        let mut app = test_app();
        app.apply_view(View {
            task_order: vec![TaskId(1), TaskId(2)],
            ..View::default()
        });
        assert_eq!(app.selected, 0);
        app.apply_view(View {
            task_order: vec![TaskId(1), TaskId(2), TaskId(3)],
            select: Some(TaskId(3)),
            ..View::default()
        });
        assert_eq!(app.view.task_order[app.selected], TaskId(3));
    }

    #[test]
    fn a_new_view_keeps_the_selection_by_identity() {
        // Rows can reorder between views; the highlight follows the task, not the index.
        let mut app = test_app();
        app.apply_view(View {
            task_order: vec![TaskId(1), TaskId(2)],
            ..View::default()
        });
        app.selected = 1;
        app.apply_view(View {
            task_order: vec![TaskId(2), TaskId(1)],
            ..View::default()
        });
        assert_eq!(app.view.task_order[app.selected], TaskId(2));

        // The selected task vanished: clamp rather than point past the end.
        app.apply_view(View {
            task_order: vec![TaskId(1)],
            ..View::default()
        });
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn the_enter_hint_follows_the_views_lost_instance_sets() {
        // The toolbar is where a lost instance becomes discoverable: selecting the row
        // has to say that Enter will bring it back, not just "open".
        let store = Store::open_memory().unwrap();
        let mut app = test_app();
        let t = store.create_task("x", 0, Autonomy::Inherit).unwrap();
        app.view.tasks = store.list_tasks().unwrap();
        app.view.task_order = vec![t.id];
        assert_eq!(app.enter_hint(), "open");

        app.view.revivable.insert(t.id);
        assert_eq!(app.enter_hint(), "start", "workspace intact, no conversation yet");
        app.view.resumable.insert(t.id);
        assert_eq!(app.enter_hint(), "revive", "pane gone, conversation kept");

        app.view.revivable.clear();
        app.view.resumable.clear();
        store.set_pane(t.id, Some(42)).unwrap();
        app.view.tasks = store.list_tasks().unwrap();
        app.view.open_pane = Some(42);
        assert_eq!(app.enter_hint(), "detach");
    }

    #[test]
    fn accept_on_a_queued_task_asks_the_engine_to_dequeue_it() {
        // A queued task toggles out even without a pane, so the UI guards must not refuse it.
        let store = Store::open_memory().unwrap();
        let (mut app, rx) = test_app_with_engine_rx();
        let t = store.create_task("do x", 0, Autonomy::Inherit).unwrap();
        app.view.tasks = store.list_tasks().unwrap();
        app.view.task_order = vec![t.id];
        app.view.merge_train.add(t.id);

        app.accept_selected();
        assert_eq!(sent(&rx), vec![Cmd::Accept(t.id)]);
    }

    #[test]
    fn abort_with_no_train_is_refused_without_asking_the_engine() {
        let (mut app, rx) = test_app_with_engine_rx();
        app.abort_train();
        assert_eq!(
            latest(&app),
            (notify::Level::Warn, "no merge train to abort".to_string())
        );
        assert!(sent(&rx).is_empty());

        app.view.merge_train.add(TaskId(1));
        app.abort_train();
        assert_eq!(sent(&rx), vec![Cmd::AbortTrain]);
    }

    #[test]
    fn a_confirmed_swap_is_sent_once() {
        let store = Store::open_memory().unwrap();
        let (mut app, rx) = test_app_with_engine_rx();
        let t = store.create_task("x", 0, Autonomy::Inherit).unwrap();
        store
            .set_workspace(t.id, "faf-task-1", std::path::Path::new("/nope/ws"), "c1", "f1")
            .unwrap();
        store.update_status(t.id, TaskStatus::Working).unwrap();
        app.view.tasks = store.list_tasks().unwrap();
        app.view.task_order = vec![t.id];

        app.handle_key(key(KeyCode::Char('s')));
        assert!(sent(&rx).is_empty(), "first s only arms");
        app.handle_key(key(KeyCode::Char('s')));
        assert_eq!(sent(&rx), vec![Cmd::Swap(t.id)]);
    }

    #[test]
    fn viewport_shows_everything_that_fits() {
        assert_eq!(
            graph_viewport(5, 10, Some((4, 4))),
            Viewport { skip: 0, body: 5, up: false, down: false }
        );
    }

    #[test]
    fn viewport_stays_pinned_to_the_top_while_the_selection_is_visible() {
        // 30 lines into 10 rows: the last row is the `↓` marker, and a selection on line 6
        // (+2 context rows) still fits above it.
        assert_eq!(
            graph_viewport(30, 10, Some((6, 6))),
            Viewport { skip: 0, body: 9, up: false, down: true }
        );
        assert_eq!(
            graph_viewport(30, 10, None),
            Viewport { skip: 0, body: 9, up: false, down: true }
        );
    }

    #[test]
    fn viewport_scrolls_just_far_enough_to_show_a_selection_below_the_cut() {
        // Line 15 selected: show through line 17 (two context rows) with markers top and
        // bottom, so the body is 8 rows ending at 17.
        let vp = graph_viewport(30, 10, Some((15, 15)));
        assert_eq!(vp, Viewport { skip: 10, body: 8, up: true, down: true });
        assert!((vp.skip..vp.skip + vp.body).contains(&15));
    }

    #[test]
    fn viewport_at_the_bottom_drops_the_down_marker() {
        assert_eq!(
            graph_viewport(30, 10, Some((29, 29))),
            Viewport { skip: 21, body: 9, up: true, down: false }
        );
    }

    #[test]
    fn viewport_shows_the_top_of_a_selection_taller_than_the_pane() {
        let vp = graph_viewport(30, 5, Some((10, 20)));
        assert_eq!(vp.skip, 10);
        assert!(vp.up && vp.down);
    }

    #[test]
    fn a_selected_task_below_the_cut_is_scrolled_into_view() {
        // 40 history rows then one agent: far taller than the pane. The agent must be on
        // screen, with a marker saying how much history scrolled off the top.
        let store = Store::open_memory().unwrap();
        let mut app = test_app();
        let t = store.create_task("deep", 0, Autonomy::Inherit).unwrap();
        app.view.tasks = store.list_tasks().unwrap();
        app.view.task_order = vec![t.id];
        let mut rows: Vec<graph::GraphRow> = (0..40)
            .map(|i| graph::GraphRow {
                gutter: "◻".into(),
                content: format!("history {i}"),
                node_index: Some(i),
                change_id: Some(format!("h{i:07}")),
            })
            .collect();
        rows.push(graph::GraphRow {
            gutter: "├─●".into(),
            content: format!("#{} :: the deep agent", t.id.0),
            node_index: Some(40),
            change_id: Some("agentaaa".into()),
        });
        app.view.task_of_node = (0..40).map(|_| None).chain([Some(t.id)]).collect();
        app.view.rows = rows;

        let text = buffer_text(&draw(&mut app, 70, 16));
        assert!(text.contains("the deep agent"), "selection visible:\n{text}");
        assert!(text.contains("↑ ") && text.contains(" more"), "up marker:\n{text}");
        assert!(!text.contains("history 0 "), "top scrolled off:\n{text}");
        assert!(!text.contains("↓ "), "nothing hidden below:\n{text}");
    }

    #[test]
    fn an_unselected_overflow_marks_what_is_hidden_below() {
        let mut app = test_app();
        app.view.rows = (0..40)
            .map(|i| graph::GraphRow {
                gutter: "◻".into(),
                content: format!("history {i}"),
                node_index: Some(i),
                change_id: Some(format!("h{i:07}")),
            })
            .collect();
        app.view.task_of_node = vec![None; 40];
        let text = buffer_text(&draw(&mut app, 70, 16));
        assert!(text.contains("history 0"), "pinned to the top:\n{text}");
        assert!(text.contains("↓ ") && text.contains(" more"), "down marker:\n{text}");
    }
}
