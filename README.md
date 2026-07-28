# faff

A TUI for running several Claude Code agents in parallel on one repo. Each task gets its
own [jj](https://jj-vcs.github.io/jj/) workspace forked off your current work, and its own
`claude` running in a WezTerm pane.

faff does not review, rebase, or merge. Integration is yours, in your own jj.

## Requirements

- Rust 1.85+ (edition 2024)
- `jj` on `PATH`
- [WezTerm](https://wezfurlong.org/wezterm/). faff runs as a pane inside it and drives agent
  panes via `wezterm cli`. Needs `WEZTERM_PANE` set; task creation fails without it.
- `claude` on `PATH`

## Build

```sh
cargo build
cargo test              # 75 tests; the workspace integration tests shell out to jj
cargo clippy --all-targets
```

## Usage

From your repo root, inside WezTerm:

```sh
faff
faff tui --repo /path/to/repo    # explicit repo instead of discovery from cwd
```

| Key | Action |
|---|---|
| `n` | new task |
| `↑`/`↓` or `k`/`j` | move selection |
| `Enter` | dock the selected task's claude pane beside faff, or detach it back to its own tab |
| `s` | swap: trade your `@` with the selected agent's revision |
| `S` | snapshot the selected agent's workspace |
| `x` | remove the selected task |
| `q` | quit |

One session is docked at a time. Docking another detaches the current one.

With task #7's session docked, faff on the left and the real `claude` pane on the right:

```
 faf · faff · 1 working · ▶ #7                   ┃ ⏺ Convert the HTTP and MQTT bridges
revisions                                      │ ┃   from postcard to JSON
@  [wvrsmsyk] (no description set)             │ ┃
│ ●  [kmkxwzqr] #7 Convert bridges to JSON  ▶  │ ┃ ● Read src/bridge/http.rs
├─╯             ⚙ working · %12                │ ┃ ● Edit src/bridge/http.rs
│ ●  [rzqlvksp] #8 Fix flaky store tests       │ ┃ ● Bash cargo test -p bridge
├─╯             🔔 needs you · %14             │ ┃
○  [yuvnmxxo] initial code commit              │ ┃ ✻ Thinking…
○  [ntlpqxos] import                           │ ┃
── detached (integrated / no node) ──          │ ┃ >
· #5 Add OAuth login ✓                         │ ┃
 [n]ew [↵]detach [s]wap [S]napshot [x]remove [q]uit   ready ┃
```

`┃` is the WezTerm pane split; faff only draws the left side. The header bar is reverse
video, the selected row is highlighted, and `▶` marks the docked session. Change ids are
padded to 8 columns with the unique prefix highlighted.

### Creating a task

`n`:

1. `jj workspace add` at the newest non-empty ancestor of `@`. If `@` is that revision,
   `jj new` runs first, advancing your working copy onto a fresh empty commit. Uncommitted
   work is included in the fork.
2. Copies `~/.claude/projects/<HEAD-key>/memory/` and `MEMORY.md` to the new workspace's
   project key.
3. Writes `<workspace>/.claude/settings.local.json` with hooks that call
   `faff report-event`.
4. Sets `hasTrustDialogAccepted` for the workspace path in `~/.claude.json`.
5. Spawns `claude` in a WezTerm pane at the workspace, docks it beside faff, focuses it.

The task starts with no prompt. You type it into the pane. The `UserPromptSubmit` hook
captures the first prompt only, and a background `claude -p --model haiku` turns it into a
short title for the row and the tab.

Steps 2 to 4 are best-effort; a failure there doesn't abort the task. A failed workspace
add or pane spawn rolls the whole thing back.

### Swapping (`s`)

`s` trades your working copy with the selected agent's revision: your `@` ends up where the
agent's revision was, and the agent's workspace ends up on your old line. Your repo now
holds the agent's work (review or build on it in your own pane), and the agent, next time it
runs, is based on your current line instead of an ever-staler fork — which is the point:
it keeps agent workspaces from going stale as you move ahead.

Mechanically it snapshots both workspaces (so an agent that never ran a jj command doesn't
lose its edits), then two `jj edit`s reorder around jj's auto-abandon of empty commits so an
empty `@` survives the trade. It bails if `@` already sits on the agent's revision, or if the
agent's revision is empty (nothing to adopt). If the agent is actively working, the first `s`
asks you to confirm (the swap changes files under a live agent); a second `s` goes through.

### Snapshotting (`S`)

`S` runs `jj util snapshot` on the selected agent's workspace, folding its uncommitted edits
into its revision so they show up in the graph. Useful for watching an agent that doesn't
snapshot on its own. `s` does this for you before a swap, too.

### Removing a task

`x` kills the pane, forgets the workspace, deletes its directory, and drops the row (no
archive). The task's commits — `(fork_point..head) ~ ::@`, its own work minus anything already
integrated into your `@` — are abandoned **only if they're all empty** (a bare fork or an
empty tip). If any carry real content, faff leaves them in place as ordinary history for you
to integrate or `jj abandon` yourself: faff never discards real work on removal. (This is also
why removing a swapped task keeps your old line, which the agent's workspace now holds.)

### The revision view

The body is one graph, built from `jj log` over `ancestors(<all workspace heads> | @, 25)`.
HEAD's line is pinned to the top lane, agent branches below it. Glyphs:

- `@` your working copy — drawn green (like jj log), labelled with its description
  (or `(no description set)`)
- `●` a faff agent's revision, with a status line under it (`⚙ working`, `🔔 needs you`,
  `✓ review-ready`) and its pane id
- `○` ordinary history, or another workspace's working copy
- `×` a conflict

Empty description-less single-parent commits collapse out. Merges and conflicts never
collapse.

A task whose change no longer has a node of its own, which is the usual result of
integrating it, moves to a "detached" list under the graph. It stays selectable and
removable there.

## How state moves

```
injected hooks → faff report-event → SQLite + socket nudge → TUI refresh
```

`report-event` writes the database, then nudges the socket
(`$XDG_RUNTIME_DIR/faf-<hash>.sock`) so a running TUI refreshes sooner. Events still land
with the TUI closed. Refresh is throttled to ~1s idle, 400ms floor while events arrive.

Hooks injected per workspace:

| Hook | Effect |
|---|---|
| `UserPromptSubmit` | status → working; first prompt captured as the task |
| `Stop` | status → idle |
| `Notification` | status → needs input |
| `PostToolUse` | appends an activity row; clears a stale needs-input |
| `SessionStart` | records the claude session id |

Each refresh also reconciles: a task whose pane has died goes back to idle, and a task whose
jj workspace has vanished is dropped.

Per-repo state lives under `~/.local/share/faf/<encoded-repo-path>/`: `faf.db`, and
`ws/<nnnn>-<slug>/` for the workspaces. The path encoding matches Claude Code's project key
scheme.

## Modules

| Module | Responsibility |
|---|---|
| `domain` | `Task`, `TaskStatus`, `Autonomy`, label truncation |
| `config` | data-dir paths, repo-path encoding, slugs |
| `store` | SQLite (tasks, activity, config) |
| `graph` | DAG to text lanes, multi-line nodes, collapsing |
| `jj` | `jj log`/`workspace list` via templates; `edit`/`snapshot` per workspace |
| `workspace` | fork, memory seed, hook injection, trust, teardown, swap, snapshot |
| `wezterm` | `wezterm cli` argv, exec, list parsing |
| `events` | event enum and Unix-socket transport |
| `scheduler` | applies events to the store |
| `title` | background title generation |
| `cli` | argument parsing and the `report-event` subcommand |
| `tui` | ratatui app: state, event loop, rendering, actions |
