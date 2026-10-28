# Live, searchable log view — design

Date: 2026-10-27
Status: draft, awaiting approval

## Why

Jezza keeps a terminal pane open on the repo's history while working: faff's TUI fills the
top half of the screen, an interactive shell sits bottom-left, and `watch -n 2 -- jj peek`
(`jj log --color always --ignore-working-copy --at-op @`) runs bottom-right. He uses that
pane to:

- see what the agents have changed in their jj workspaces, and get a revision id to refer to;
- see his own bookmarks before pushing;
- see where other bookmarks sit: `master`, and contributors' branches.

That last one recently failed outright. He knew a contributor's branch name from a GitHub PR
and could not find it in the log. Once found, he wanted to see where it forked off master
and what it changed, and then review it with his `jj sq` alias (squash the whole branch
into one revision and `jj edit` into it).

What gets in the way today:

- `watch` redraws every 2 s, can't scroll, and can't search.
- `jj peek` deliberately doesn't snapshot, so edits made in the shell don't show up until
  some other jj command runs. Snapshotting from a poll caused divergence before (see
  commit `31b848d0` "Poll jj without snapshotting the working copy": a snapshot landing
  mid `jj git fetch` left a divergent op log).
- faff's own graph only loads `ancestors(<each workspace @> | @, 25)`
  (`jj::workspace_ancestors_revset`, `src/jj.rs:184`), so anything off those lines is
  invisible to it.
- faff's graph rendering is muted: a cyan id prefix and little else besides grey. It shows
  no bookmarks, author or time.
- His repos now merge with a fork/merge strategy, so history is full of
  "fork from main, merge back" pairs that take up most of the screen.

## Goals

1. `faff logs`: a standalone, live log view that replaces `watch jj peek` in that pane.
2. It stays live without polling jank and snapshots the checkout it was started in when files
   change, without racing a concurrent `jj git fetch` into divergence.
3. Fuzzy search (nucleo) over bookmarks, descriptions, authors and ids, with
   `grep -A/-B`-style adjustable context around each hit.
4. Search can find bookmarks, including untracked remote ones, that aren't in the loaded revset.
5. A clearer look: magenta id prefix, plus bookmark, author and age columns, with the
   description given as much width as the terminal allows.
6. Collapse simple fork/merge pairs to one row, and expand them when a search hit is inside.
7. Act on what you find: copy the id, squash a branch for review, view its squashed
   diff, and `jj edit` into a revision.
8. faff's existing log view and `faff logs` share one implementation.

## Non-goals

- Changing the revset faff's TUI loads. It stays `workspace_ancestors_revset(.., 25)`.
- A detail pane, or expanding the selected row to show its full description. (Considered
  and rejected: merges already have "expand", and a second kind of expand would confuse.)
- Row-by-row navigation while a filter is active. Movement jumps between matches only.
- Snapshotting any checkout other than the one `faff logs` was started in. Agent
  workspaces are faff's business.
- Splitting `src/tui/mod.rs` into modules beyond what sharing the log view requires.

## Anti-goals (it fails if…)

- `faff logs` causes divergent changes or a divergent op log when used alongside
  `jj git fetch`, rebases, or a running faff.
- `faff logs` and a running `faff` fight over faff's SQLite database, sockets or wezterm
  panes. `faff logs` must not open the DB at all.
- Any key in the log view moves the working copy, except the explicit `e`.
- The `sq` action moves `@`.

## Design

### 1. `faff logs` command

New `Command::Logs` in `src/cli.rs`:

```
faff logs [-r/--revisions REVSET] [--repo PATH]
```

- `--repo` defaults to the current directory, like `faff tui`.
- Default revset: the user's configured `revsets.log` (read with `jj config get revsets.log`;
  jj's own `builtin_log()` if unset). For Jezza that is
  `present(@) | bookmarks() | present(fork_point(trunk() | @)::(@ | trunk())) | ancestors(immutable_heads().., 5)`.
  `-r` replaces it.
- No database, no scheduler, no agents, no wezterm, no hook injection, no socket. It runs
  only the log view (section 4) plus a small engine that loads revisions, watches and
  snapshots (sections 2 and 3).
- `q` quits.

### 2. Live updates

- Reload whenever jj records an operation. Watch the op-heads directory
  (`<repo>/.jj/repo/op_heads/heads`; in a secondary workspace `.jj/repo` is a file holding
  the real repo path, so resolve it). That catches the user's commands, faff's and the
  agents', and redraws within moments instead of every 2 s.
- Keep a slow fallback poll (*my call:* 10 s) in case a watch event is missed.
- All reads use `--ignore-working-copy` (`jj::log_peek` / `peek_jj`, `src/jj.rs:76`), so a
  read never records an operation.
- File watching: the `notify` crate (*my call*, builder may choose an equivalent).

### 3. Snapshotting the checkout

- Watch the files of the workspace `faff logs` was started in. Ignore `.jj/`, `.git/`, and
  paths matched by the repo's gitignore rules (the `ignore` crate's gitignore matcher, *my
  call*), so build output under `target/` doesn't trigger anything.
- After changes, wait for a quiet period (*my call:* 1 s), then run `jj util snapshot`
  (jj 0.45 has it; it records an operation only if something changed).
- **Never snapshot while another jj command that can rewrite commits is running in this
  checkout.** That is what made the old poll diverge. Before each snapshot, scan for live
  `jj` processes (Linux: `/proc/*/cwd`, `/proc/*/cmdline`) whose working directory is
  inside this checkout, and defer until none of the mutating kind remain. The check is
  racy at the edges (a fetch could start right after it), but it closes the realistic
  window: fetches and rebases take seconds, and the snapshot itself takes milliseconds.
  Rules (*my call*):
  - **Exempt:** `util exec` (the jj process
    only waits while `jj ui`/jayjay runs, and jayjay's own jj calls are separate processes
    that get judged on their own); and read-only subcommands (`log`, `show`, `diff`,
    `st`/`status`, `op log`, `evolog`, `config`, `file show`, `bookmark list`, plus aliases
    that expand to these, such as `peek`). These snapshot only at start-up, before any
    pager, so a paged `jj log` doesn't hold anything up.
  - `--ignore-working-copy` is **not** an exemption (revised after testing): it skips the
    working-copy lock, so a writer run with it races a snapshot straight into a divergent
    `@` (30 of 30 rounds in a stress test), where an ordinary writer serializes on the lock.
    Only the subcommand decides.
  - **Defer for everything else**, including `describe` waiting on an editor, which really
    would rewrite `@` after our snapshot.
  - No time limit on a deferral. The status line says what it is waiting on
    (`snapshot deferred: jj git fetch`).
  - Only processes in this checkout count. Agent workspaces live outside the repo and
    rewrite only their own `@`, and concurrent operations on different commits merge
    cleanly in jj.
- *My call:* show a dim `snapshotting…` / `snapshot deferred (jj running)` in the status
  line so it's never mysterious.
- Snapshot failures show as a notice and don't stop the view.

### 4. The log view (shared by faff's Tab log body and `faff logs`)

faff already has a log body (`Body::Log`, `src/tui/mod.rs:135`; Tab toggles it): every
loaded revision in jj's order, selection per revision. That is the view to extend and
share. Factor it so `faff logs` can host it without the rest of `App` (tasks, sessions,
train, DB). Inside faff it keeps its existing task keys (`n`, `N`, `d`) and its revset.

**Which faff views change.** The new look (magenta prefix, bookmark, author and age
columns) applies to **both** faff bodies (the agents view and the log body) as well as
`faff logs`, through the shared painter (`graph_lines`). Tests that assert the cyan prefix
(for example `graph_renders_id_column_with_prefix` and the docked-id test) get updated to
magenta.

Merge collapsing and search are **log-body only** (the user's call). The agents view
selects per task, so it couldn't expand a merge or land on a search hit. Pressing `/` in
the agents view switches to the log body with the search line open. Esc clears the filter
and stays in the log body, and Tab goes back (*my call*).

**Task decoration** (*my call*). The shared painter takes optional per-row decoration from
its host. In faff, task labels and status (`#3 ⚙ :: title`) stay where they are today, at the
start of the description column. Multi-line nodes (an agent's continuation line under a
HEAD node) stay as they are, so `log_selected_task` and `d` keep working. "One row per
revision" describes plain revisions; `faff logs` has no tasks, so it is always one row.

**Glyphs** (*my call*). The log body and `faff logs` replace `jj peek`, so they use jj's
meanings: `@` working copy, `◆` immutable, `○` mutable, `×` conflicted. This adds
`immutable` to the template. faff's fork point keeps its cyan tint on whichever glyph it
has. The agents view keeps faff's existing glyphs (◻, ●/○, cyan ◆ fork point).

#### 4.1 Data

Extend `LOG_TEMPLATE` (`src/jj.rs:19`), or add a log-view template, to also carry:

- the full description (newlines escaped), searched in full and shown as the first line;
- author name and email;
- the committer timestamp (*my call:* committer, matching jj's default log);
- local bookmarks, remote bookmarks (`name@remote`), and which workspaces have this as
  their `@`.

#### 4.2 Row layout

One row per revision:

```
gutter  [id]  description …………………  bookmarks  author  age
```

- **Id:** unchanged shape, with the unique prefix **magenta** bold (was cyan) and the rest
  dark grey. Every other existing colour stays: green `@`, cyan fork-point `◆`, green
  docked-session id.
- **Description:** takes all remaining width and is truncated with `…`. It's the column that
  grows with the terminal.
- **Bookmarks:** a column after the description, sized to its widest content and blue bold.
  A remote bookmark at the same revision as its local one is shown as `master @origin`
  (remote part dimmer). A remote-only bookmark is `name@origin`, dim blue. Workspace
  markers (`agent-3@`) also go here, dim (*my call*).
- **Author, age:** right-aligned, grey (not bold). Author is the name, falling back to the
  email's local part. Age is compact relative time: `2m`, `1h`, `3d`, `5w`, then a date.
- Layout uses the real terminal width. When space runs short, columns give way in this
  order: age disappears, then author, then bookmarks shrink to the first name plus `+N`.
  The description keeps a sensible minimum (*my call:* 20 cols) before any of that kicks in.

Mockups from the session: `.superpowers/brainstorm/*/content/log-look-v2.html` and
`log-look-v3.html` (local only, not committed).

#### 4.3 Collapsing fork/merge pairs

A merge `M` with two parents, mainline `P1` (first parent) and side `P2`, is a **simple
pair** when the side commits `S = P1..P2` form a single linear chain (each has one parent)
whose root's parent is `P1` **or any ancestor of `P1`** (*my call*: a PR branch that forked
before main moved on is still a fork/merge pair to the user; about 1 in 5 real merges
fork from an older mainline commit). Mainline commits between the fork and `P1` render
normally. The collapsed side lane simply isn't drawn.

- Simple pairs render as one row: the merge, with a dim `(+N)` after the description
  (N = |S|). This is option A from the mockups, chosen over a folded side-lane stub.
- *My call:* a pair does **not** collapse if `S` contains `@`, any workspace's `@`, or a
  local bookmark. Remote-only bookmarks don't block collapsing.
- **→** on a collapsed merge expands it; **←** collapses it again (the user's choice). With
  the selection on a side commit of an expanded pair, ← collapses that pair and selects
  the merge.
- A search hit inside `S` expands that pair for as long as the filter is active.
- Collapsing happens in the graph model so lanes are laid out for what's visible. The
  existing `collapse` / `splice_collapsed` machinery in `src/graph.rs` is the natural
  place.

#### 4.4 Search

- `/` opens a search line at the bottom. Typing filters live.
- Matching uses nucleo (`nucleo-matcher` crate) against one haystack per revision: change id,
  commit id, full description, author name and email, local and remote bookmark names,
  and workspace names. Matched characters are highlighted where they're visible.
- Non-matching revisions fold away except for **±N context** revisions around each hit, in
  display order (like `grep -B N -A N`). Runs of hidden revisions show as one dim
  `~ 212 hidden ~` row. Default N = 1 (*my call*).
- **Enter** locks the filter and leaves the search line. Then:
  - `+` / `-` widen or narrow the context (N ≥ 0);
  - **↑/↓ and `j`/`k`** jump to the previous or next hit (no row-by-row movement while
    filtered);
  - `/` edits the query again;
  - **Esc** clears the filter and restores the full view, keeping the selection on the
    revision that was selected.
- ↑/↓ also jump between hits while still typing.
- The status line shows `2 hits · 1/2 · ±1` plus key hints.

#### 4.5 Search reaches outside the revset (`faff logs` only)

If the query matches a bookmark (local or remote, from `jj bookmark list --all-remotes`,
cached and refreshed on each operation) whose target isn't loaded, pull that branch in:
add `fork_point(trunk() | <bookmark>)::<bookmark>` to the loaded revset and reload, so it
appears in place in the graph with its fork point. This is what would have found the
contributor's PR branch.

- *My call:* do this for at most the 5 best-scoring unloaded bookmarks, and only after
  typing pauses (~300 ms), so a half-typed query doesn't fan out jj calls.
- Pulled-in branches stay loaded until the filter is cleared.
- In faff's own log body, search filters only what's loaded (*my call*: the user wants
  faff's revset left alone).

#### 4.6 Actions (on the selected revision)

| Key | Action | jj |
| --- | --- | --- |
| `c` | Copy the change id to the clipboard | — (OSC 52, *my call*, so it works over SSH and in wezterm) |
| `e` | Edit: move this checkout's `@` to it | `jj edit <rev>` |
| `z` | sq: squash the branch into one new revision *without moving `@`*, then select it | `jj new --no-edit 'fork_point(<rev> \| trunk())' -m "squashed: <name>"` then `jj restore --from <rev> --into <new>` |
| `v` | sq-diff: view the branch as one diff in the pager, return to the log on exit | `jj diff --from 'fork_point(<rev> \| trunk())' --to <rev>` with the TUI suspended |

- `z` and `v` cover the fork point up to and including the **selected** revision, not the
  branch tip (the user's call). Selecting a mid-branch commit gives a partial squash.
- `<name>` is the selected revision's first local bookmark, else its first remote one, else
  its short change id.
- The `z` and `v` letters are *my call*: `s`, `d`, `n`, `N` are taken in faff's log body.
  `c` and `e` are the user's choices.
- **Revisions already in trunk** (*my call*). There the fork point is the revision itself,
  which would give an empty diff or squash. Instead:
  - On a merge (collapsed or not), `v` shows the side branch as one diff,
    `jj diff --from <P1> --to <M>`, which is "what did this PR change". `z` squashes that
    same range into a new revision on `P1`.
  - On a non-merge trunk revision, `v` falls back to `jj diff -r <rev>`, and `z` refuses with
    a notice.
- **Where `z`'s revision shows up.** It's a mutable child of an immutable fork point, so the
  default `revsets.log` (`ancestors(immutable_heads().., 5)`) already includes it. If a
  custom `-r` doesn't, `faff logs` adds it to the loaded set the way 4.5 pulls in
  bookmarks, until the view exits. Repeating `z` creates another squashed revision each
  time, as the `jj sq` alias does (*my call*).
- The new revision's id comes from `jj new`'s output or a follow-up
  `jj log -r 'latest(…)'`. The builder can pick whichever is reliable.
- `z` is **not** offered in faff's bodies, because faff's revset is fixed at workspace
  ancestors and the new revision would be created out of sight (*my call*).

#### 4.7 Keys per context

Each context's toolbar shows **only the keys that exist in that context**. Dimming is
kept for keys that exist there but don't apply to the selected row (for example `↵` on a
task with no pane). This replaces today's behaviour, where the log body shows every task
key dimmed (`hint_available`'s `c.log` arm, `src/tui/mod.rs:1391`). Blank cells mean the
key does nothing there and isn't shown.

| Key | Agents view | faff log body | `faff logs` |
|---|---|---|---|
| `n` new task | empty child of fork point | empty child of selected revision | |
| `N` | handoff | **spawn**: an agent takes over the selected revision itself (`Base::Continue`; relabelled from "edit") | |
| `↵` | open/detach/revive/start | | |
| `→` / `←` | | expand / collapse merge | expand / collapse merge |
| `s` swap, `S` snap, `r`/`R` rebase, `a` accept, `x`/`X` rm/drop | ✓ | | |
| `A` abort train | when a train is running | when a train is running | |
| `d` describe | via the task's agent | via its agent, else headless claude | headless claude (`describe.rs`; no DB needed) |
| `c` copy change id | the task's revision | ✓ | ✓ |
| `e` `jj edit` in your checkout | | ✓ | ✓ |
| `v` sq-diff | | ✓ | ✓ |
| `z` sq | | | ✓ |
| `/` search | switches to log body | ✓ | ✓ |
| `⇥` toggle view | ✓ | ✓ | |
| `q` `?` `!` | ✓ | ✓ | ✓ |

Search has two more contexts, each with its own toolbar:

- **Typing:** printable keys go into the query, `↑`/`↓` jump between hits, `↵` locks the
  filter, and `Esc` clears it.
- **Locked:** `↑`/`↓`/`j`/`k` jump between hits, `+`/`-` change the context, `→`/`←`
  expand/collapse, `/` edits the query, `Esc` clears. The host view's selected-row keys
  (`c`, `e`, `v`, `d`, `n`, `N`, `z`) act on the selected hit.

`input::map_key` becomes context-aware (or gains per-context maps), and the
`every_action_key_appears_in_the_toolbar_data` test extends to every context.
- Mutating actions (`e`, `z`) run through the same "no other jj running" guard as snapshots,
  and snapshot first (`jj edit` does that itself).

### 5. Constraints

- Rust, ratatui 0.29, existing crate layout. New dependencies: `nucleo-matcher`,
  `notify` (or equivalent), and `ignore` (or equivalent gitignore matching).
- Version control is jj. Don't run tree-wide `cargo fmt` (the tree isn't rustfmt-clean).
  Format only touched code.
- Tests follow the existing style: unit tests beside the code in each module, TUI tests
  driving `App` with key presses and checking rendered spans/colours (for example
  `graph_renders_id_column_with_prefix`, `src/tui/mod.rs:2239`).

### 6. Left to the builder

- How to factor the log body out of `App` so both hosts share it.
- The exact template, escaping and parsing.
- Watcher plumbing, debounce implementation and process detection details.
- Exact colour constants within the stated intent (magenta prefix, blue bookmarks, grey
  author/age).
