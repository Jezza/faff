# faff

A TUI for running several Claude Code agents in parallel on one repo. Each task gets its
own [jj](https://jj-vcs.github.io/jj/) workspace forked off your current work, and its own
`claude` running in a WezTerm pane.

By default faff does not review, rebase, or merge — integration is yours, in your own jj.
The one exception is the opt-in *merge train* (`a`): it drains a set of finished revisions
into your workspace for you, one at a time. Everything else stays hands-off.

## Requirements

- Rust 1.85+ (edition 2024)
- `jj` on `PATH`
- [WezTerm](https://wezfurlong.org/wezterm/). faff runs as a pane inside it and drives agent
  panes via `wezterm cli`. Needs `WEZTERM_PANE` set; task creation fails without it.
- `claude` on `PATH`

## Build

```sh
cargo build
cargo test              # 115 tests; the workspace integration tests shell out to jj
cargo clippy --all-targets
```

## Nix

A flake is provided. It builds faff and wraps the binary so `jj` and `wezterm` are on
`PATH` at runtime (`claude` still comes from your interactive session, since faff spawns it
inside a WezTerm pane).

```sh
nix run github:Jezza/faff              # run without installing
nix profile install github:Jezza/faff  # install into your profile
nix develop                            # dev shell with the toolchain + jj + wezterm
```

To install declaratively, add the flake as an input and reference
`faff.packages.${system}.default`.

## Usage

From your repo root, inside WezTerm:

```sh
faff
faff tui --repo /path/to/repo    # explicit repo instead of discovery from cwd
faff logs                        # just the live log view (see `faff logs` below)
```

| Key | Action |
|---|---|
| `n` | new task (in the [log view](#the-log-view): forked from the selected revision) |
| `N` | hand off: spawn an agent onto your current revision (it continues your work), and reset your own workspace to the fork point from before your changes (in the log view, *spawn*: an agent that edits the selected revision) |
| `f` | log view: a workspace of your own forked from the selected revision (no agent) |
| `F` | log view: a workspace that takes the selected revision itself (no agent) |
| `↑`/`↓` or `k`/`j` | move selection |
| `Tab` | switch between the agent view and the [log view](#the-log-view) |
| `Enter` | dock the selected task's claude pane beside faff, or detach it back to its own tab; on a task whose pane has died, bring the agent back (see [Reviving a lost agent](#reviving-a-lost-agent)) |
| `s` | swap: trade your `@` with the selected agent's revision |
| `S` | snapshot the selected agent's workspace |
| `r` | refresh: tell the agent to rebase onto the latest fork point (freezes your WIP first) |
| `R` | refresh onto your parent line instead (read-only; your WIP excluded) |
| `d` | describe: tell the agent to set a short 4-7 word jj description of the revision's end result (in the log view, any revision: a background `claude -p` writes it) |
| `a` | accept: toggle the selected revision into/out of the merge train (drained into your `@` one at a time) |
| `A` | abort the merge train: dequeue everything still pending (already-merged revisions stay) |
| `x` | remove the selected task (keeps its revision as history); in the log view, the workspace on the selected revision, else the revision itself if it's empty |
| `X` | remove the selected task *and* abandon its revision (discards the work); in the log view, likewise for the workspace on the selected revision, else the revision itself, changes and all |
| `c` | copy the selected revision's change id to the clipboard |
| `/` | search (in the agent view, switches to the log view first) |
| `e` | log view: `jj edit` the selected revision (moves your `@` onto it) |
| `v` | what the selected agent added, as one diff in your pager: from its task's fork point to its workspace's `@` (in the log view, the selected branch from where it forked) |
| `→`/`←` | log view: open / fold a folded merge |
| `q` | quit |

One session is docked at a time. Docking another detaches the current one.

With task #7's session docked, faff on the left and the real `claude` pane on the right:

```
 faff · faff · 1 working · ▶ #7                            ┃ ⏺ Convert the HTTP and MQTT bridges
revisions                                                │ ┃   from postcard to JSON
@  [wvrsmsyk] (no description set)                       │ ┃
├─●  [kmkxwzqr] #7 ⚙ :: Convert bridges to JSON  ▶       │ ┃ ● Read src/bridge/http.rs
├─●  [rzqlvksp] #8 ! :: Fix flaky store tests            │ ┃ ● Edit src/bridge/http.rs
◻  [yuvnmxxo] initial code commit                        │ ┃ ● Bash cargo test -p bridge
◻  [ntlpqxos] import                                     │ ┃
── detached (integrated / no node) ──                    │ ┃ ✻ Thinking…
· #5 Add OAuth login ✓                                   │ ┃
                                                         │ ┃ >
 · ready                                                   ┃
 new nN │ open ↵ │ rev sSrRdcv │ train aA │ del xX │ /⇥ q?!┃
```

When the merge train is non-empty a panel appears below the toolbar, one row per queued
revision with its current stage:

```
─ merge train ────────────────────────────────────────────
 #7   Convert bridges to JSON              · ready
 #8   Fix flaky store tests                ✎ describing
 #9   Two-phase plugin startup             ⚙ rebasing
```

`┃` is the WezTerm pane split; faff only draws the left side. The header bar is reverse
video, the selected row has a dark grey background, and `▶` marks the docked session. Change ids are
padded to 8 columns with the unique prefix highlighted.

### Creating a task

`n`:

1. `jj workspace add` at the newest ancestor of `@` with content
   (`heads(::@ ~ (empty() ~ merges()))`). If `@` is that revision, `jj new` runs first,
   advancing your working copy onto a fresh empty commit. Uncommitted work is included in
   the fork.

   A merge counts as content even though jj reports it `(empty)`: a merge that combined its
   parents cleanly modifies no files *relative to those parents*, but it is the only
   revision holding both sides, so it's exactly what an agent should fork from. Only empty
   *non-merge* commits — bare fork-points, fresh working copies — are skipped as noise.
2. Copies `~/.claude/projects/<HEAD-key>/memory/` and `MEMORY.md` to the new workspace's
   project key.
3. Writes `<workspace>/.claude/settings.local.json` with hooks that call
   `faff report-event`.
4. Sets `hasTrustDialogAccepted` for the workspace path in `~/.claude.json`.
5. Spawns `claude` in a WezTerm pane at the workspace, docks it beside faff, focuses it.

The task is born with a session id — a UUID minted in the same statement as the row and
passed as `claude --session-id`, rather than learned afterwards from the agent's
`SessionStart` hook. The id is therefore durable *before* the agent process exists, which
is what makes the agent recoverable if its pane dies. The hook still overwrites the id
afterwards, so faff tracks the conversation that is actually live.

The task starts with no prompt. You type it into the pane. The `UserPromptSubmit` hook
captures the first prompt only, and faff uses its first line as the task's display label in
the log until the change gets a real description of its own (see `d`). The agent's tab is
titled `#<id>`.

Steps 2 to 4 are best-effort; a failure there doesn't abort the task. A failed workspace
add or pane spawn rolls the whole thing back.

### Handing off (`N`)

`N` (Shift + n) hands your in-progress work to an agent. Where `n` forks *beside* your work
and leaves you on it, `N` gives the work *away*: the agent takes over your current revision
`W` — it continues editing that exact commit — and your own `@` retreats to a fresh empty
commit on the fork point from *before* your changes (`heads(::@- ~ (empty() ~ merges()))`,
faff's `R` recipe). The end result:

```
● W   agent @  (your WIP — the agent continues it)
│
│ @   you (fresh empty)
├─┘
◆ P   the fork point, before your changes
```

Mechanically it mirrors `swap`: it snapshots your workspace first (so nothing uncommitted is
lost), forks the agent workspace and moves it onto `W`, then retreats your `@` *last* — so a
failure before that leaves you untouched on your work. Like `n`, it then docks and focuses
the new agent so you type what you want it to finish; the agent already holds your WIP in its
tree for context. It bails if your `@` has no changes (nothing to hand off).

The task's fork point is recorded as `P`, so `W` counts as the agent's own work: `x` keeps it
as history and `X` discards the whole handed-off line (both still shielded from anything you
later re-integrate). If you've stacked several of your own commits, `N` hands off the current
one and retreats to its parent line.

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

### Refreshing an agent (`r` / `R`)

Where `s` keeps an agent fresh by *adopting its work onto your line*, `r` keeps it fresh in
place: it re-bases a running agent forward without moving anything into your repo. faff
computes the new base — the same fork-point recipe `n` uses,
`heads(::@ ~ (empty() ~ merges()))` — and
**injects a prompt into the agent's pane** telling it to run `jj rebase -b @ -d <base>` and
carry on. faff never runs the rebase itself; the agent does, and resolves any conflicts. If
the agent is mid-turn, Claude Code queues the prompt; faff keeps no queue of its own.

`r` freezes your uncommitted WIP first (a `jj new` on *your* `@`, exactly like `n`), so the
agent picks up your latest work. `R` bases it on your parent line instead — read-only, WIP
excluded. Either is a no-op (reported, nothing sent) when the agent already sits on the
newest base. On a working agent the first press arms a confirmation (a redirect mid-turn is
disruptive); the same key again sends it. This needs the send side of the pane, so faff adds
`wezterm cli send-text` alongside the `get-text` it already uses.

### Describing a revision (`d`)

Until its change has a description, a task's log row shows the first line of its prompt — a
fair *intent* label, but not what the work actually turned into. `d` closes that gap: like
`r`, it **injects a prompt into the agent's pane**, here telling the agent to run `jj
describe` with a short 4-7 word summary of the revision's *end result*. faff never runs `jj
describe` itself; the agent does, and the log row picks up the new description on the next
refresh. Run it once the agent has finished — on a working agent the first press arms a
confirmation (a describe mid-turn is premature, and the prompt is disruptive), and a second
`d` sends it. It needs a live pane and a task that already has a prompt of its own (otherwise
the injected prompt would be captured as the first prompt, exactly as with `r`).

### Accepting into the merge train (`a` / `A`)

`a` marks the selected agent's revision to be *accepted* — integrated into your own
workspace. Where `s` and `r` keep agents fresh, `a` is the one action that pulls their work
back into your `@`. It's a *set*, not a rigid pipeline: press `a` on several finished tasks
and faff drains them into your line one at a time.

Accepting needs a clear landing spot — an empty, description-less `@` (commit or hand off
your own WIP first). `a` refuses a revision with nothing to merge, and a task without a live
pane or its own first prompt. Press `a` again on a queued task to drop it back out; `A`
aborts the whole train (revisions already taken over stay; nothing is rolled back).

Each tick, for the set:

1. The revision closest to ready — idle, sitting on the current tip, described — is **taken
   over**: faff snapshots the agent, then runs `jj new <agent_rev>` in your workspace, so
   your `@` becomes a fresh empty child of it. That empty `@` is the landing spot for the
   next one. The agent is then retired (like `x`; the revision is now integrated, so it's
   kept, not abandoned).
2. If the front revision lacks a description, faff **injects the `d` prompt** and waits for
   it (a merge without a summary is premature).
3. After each take-over the tip moves, so every remaining member is **asked to rebase onto
   the new tip** — the same agent-side rebase as `r` (faff injects `jj rebase`; the agent
   runs it). Whichever lands cleanly and idles first is accepted next (ready-first), so a
   slow task never blocks a quick one.

faff never rebases or resolves conflicts in your workspace — that all happens in the agents'
workspaces, exactly as with `r`. Your `@` only ever *takes over* an already-clean revision.
Because `a` is an explicit "merge this", faff sequences on the agent rather than second-guessing
it: a **working** agent is waited on (its revision isn't settled yet); once its turn is over —
idle *or* needs-input — the train drives it (describe, rebase, take over). It only *drops* a
member it genuinely can't merge: a **conflicted** revision, an **empty** one, or a task whose
workspace has vanished — each left as an ordinary task for you to handle by hand, while the
rest carry on. The train is in-memory: quit mid-drain and the revisions already taken over
persist in jj, but the pending set is forgotten.

The panel below the toolbar shows each member and its stage — `rebasing`, `describing`,
`ready`, `merging`, or `resolving conflict` — updated every refresh.

### Removing a task

`x` kills the pane, forgets the workspace, deletes its directory, and drops the row (no
archive). The task's commits — `(fork_point..head) ~ ::@`, its own work minus anything already
integrated into your `@` — are abandoned **only if they're all empty** (a bare fork or an
empty tip). If any carry real content, faff leaves them in place as ordinary history for you
to integrate or `jj abandon` yourself: faff never discards real work on removal. (This is also
why removing a swapped task keeps your old line, which the agent's workspace now holds.) It
goes the same whatever the workspace: one faff made for the task, or one the agent was
spawned into with `N`.

`X` (Shift + x) removes the same way but *also* abandons the revision, real content and all —
for when you've decided the work isn't worth keeping and don't want to `jj abandon` it by hand.
The `~ ::@` guard still applies, so anything already integrated into your `@` is never touched;
`X` only ever discards the task's own unintegrated line. (jj keeps its op log, so an `X` you
regret is recoverable with `jj op undo`.)

In the log view, `x` and `X` remove the workspace on the selected revision the same way, any
workspace but your own: an agent's goes with its task and pane. Its own commits start above
the fork point its task recorded, else the one `f` / `F` noted when they made it, else the
newest revision with changes below its `@`. The workspace the repo lives in (where `jj git
init` ran) is never removed. A revision with no workspace on it is abandoned by itself: `x`
only if it's empty (it tells you `X` discards the changes), `X` regardless.

### Reviving a lost agent

A pane can go away without the work going away: the agent exits, the tab gets closed, the
WezTerm mux restarts, the machine reboots. The jj workspace and the task row survive all of
that, so the agent can be put back. Each refresh flips such a task to idle and clears its
dead pane; selecting it shows what `Enter` will do:

| Toolbar | State | What `Enter` does |
|---|---|---|
| `↵ revive` | pane gone, conversation on disk | `claude --resume <session-id>` — the agent comes back with its full history |
| `↵ start` | pane gone, nothing written yet | `claude --session-id <session-id>` — a blank agent in the same workspace |
| `↵ open` / `↵ detach` | agent running | dock / detach as usual |

The `↵ start` case is the common one for a task created with `n` and never typed into:
claude writes no transcript until the first message, so there is no conversation to
restore — only a workspace waiting for an agent.

Reviving is manual on purpose. A pane usually disappears because you closed it, and an
automatic respawn on sight would be a fight rather than a feature.

What comes back is the *conversation*, not the process. A turn that was in flight when the
pane died is lost, and MCP servers and background tasks start over. The revision is
untouched — the agent picks up the working copy exactly as it left it.

Two constraints worth knowing, both from `claude` itself:

- `--session-id` is create-only. Re-running it against an existing conversation fails with
  `Session ID <uuid> is already in use`, which is why the resume path is a separate branch
  rather than one idempotent command.
- A resumed session adopts the directory it is launched in, not the one it was created in.
  faff always relaunches at the task's own workspace, so the transcript stays under that
  workspace's project key.

### The revision view

The body is one graph of every workspace's working copy (yours and the agents') and their
ancestors, 25 generations deep, whatever your `revsets.log` reaches. HEAD's line is pinned
to the top lane, agent branches below it. Glyphs:

- `@` your working copy — drawn green (like jj log), labelled with its description
  (or `(no description set)`)
- `●` a faff agent's revision (hollow `○` when the revision is still empty), shown on one
  row as `#<id> <status> :: <title>` — the title is the change's jj description (falling back
  to the first line of the prompt until it's described), and `<status>` is a one-column
  glyph, coloured by mode: blue `⚙` working / bold magenta `!` needs you / green `✓`
  review-ready. Every glyph faff draws is a single column wide, so the `#<id>` padding
  keeps the status column straight and label clipping counts characters honestly
- `◻` ordinary history, or another workspace's working copy
- `◆` the current fork point — drawn cyan — the revision new agents branch from
  (`heads(::@ ~ (empty() ~ merges()))`); when it coincides with your working copy the `@`
  itself turns cyan
- `×` a conflict

Empty description-less single-parent commits collapse out. Merges and conflicts never
collapse. A merge (a revision with 2+ parents) draws its fork inline on its own row —
`●─╮` — opening a lane for each extra parent, so both parent lines are visible:

```
◆─╮  [wvrsmsyk] integrate #7
● │  [kmkxwzqr] your work
├─●  [rzqlvksp] #7 :: Convert bridges to JSON
◻  [yuvnmxxo] fork point
```

An agent is always a stub hanging off the line it forked from — it never occupies the
leftmost lane, and never holds a lane open across the commits below it. Because the revset
is bounded, a line can run off the edge of the loaded window; the leftmost lane is then
picked up by the next line down, and an agent forked off *that* line opens the lane for it
with a `╭` corner (nothing above belongs to the new lane) rather than taking it:

```
◻    [qxyvqrtt] update git dep url     ← the line above ends here (its parent is off-window)
╭─●  [rzmqpztu] #21 :: Migrate db host calls
◻    [uzqqxmut] Simplify event publishing
├─●  [qoypxsox] #15 :: Two-phase plugin startup
├─○  [tztqkvsx] #9  :: Implement OIDC support
◻    [pkmzqmnr] deploy time class hashes
```

Row labels are clipped to the current pane width — and only when they overflow — so they
re-fit as docking or detaching a session resizes faff.

When the graph is taller than the pane, it stays pinned to the top until the selection
would fall below the cut, then scrolls just far enough to show the selected task with two
rows of context beneath it. A dim `↑ N more` / `↓ N more` on the first/last row says how
much is hidden.

A task whose change no longer has a node of its own, which is the usual result of
integrating it, moves to a "detached" list under the graph. It stays selectable and
removable there.

### The log view

`Tab` switches the body between the agent view above and the log view; the header names
the active one (`agents` or `log`) at its far right. faff loads both views' revisions
together, side by side: the log view's are your own `revsets.log` plus every workspace's
working copy (`working_copies()`), so agents always show. Scrolling the log view to within a screen of
the bottom loads the next 50 generations of older history before you get there; `↓` on the
last row waits for it rather than wrapping to the top, until the root is loaded. faff reloads whenever jj records an operation — yours, an agent's,
anyone's — keeping whatever you've scrolled into, so switching views is instant. The log isn't curated the way the
agent view is: empty commits get a row of their own (`(empty) (no description set)`,
as jj labels them), agent branches stay in jj's order instead of being lifted onto their
fork points, and the glyphs are jj's (`◆` immutable, `○` mutable, `@`, `×` conflicted).
Agent revisions keep their `#<id> <status> :: <title>` label, so they're still easy to spot.

Both views show each revision's bookmarks, author and age in columns right of the
description. Local bookmarks are bold blue, with the remotes that agree with them after
them (`main @origin`); remote-only bookmarks are dim blue, and other workspaces' working
copies dim green (`agent-3@`). The description takes whatever width the terminal has left;
when that gets tight, the age goes first, then the author, then each row's bookmarks
shrink to the first plus a count (`main +2`).

```
@    [wwkvpqku] (no description set)                                     Jezza  1w
○    [zuwtnnpt] Set faff's own terminal title                            Jezza  1w
◆    [sqlqskkp] Order revisions as jj's graph does         main @origin  Jezza  1w
⊕    [srsmunsk] Merge #41: nest graph lanes (+3)                         Jezza  2w
```

**Merges fold.** A merge whose side branch is a plain run of commits that forked off the
mainline (and carries no bookmark, working copy or agent) shows as the merge alone, drawn
`⊕` instead of its usual glyph, with `(+N)` for the commits it hides. `→` on it opens it, `←` folds it again (from the merge or
any of its side commits).

**Search.** `/` opens a search line (in the agent view too, which switches to the log view
first: only the log view can land on a hit). What you type is fuzzy-matched (nucleo, so `'word`
asks for it verbatim) against every loaded revision's change and commit ids, description,
author, email, bookmarks and workspaces. Each word of the query matches within one of those
fields, never strung across two, and what it matched is picked out in yellow. When that's
something the row doesn't show (an email, the description's body, a commit id), a dimmed
snippet after the description shows it, as in `email: …row@gmail.com`. Everything else folds away except the revisions either
side of each hit (`~ 12 hidden ~` marks the gaps), merges holding a hit open up, and the
selection lands on the best match. `↑`/`↓` jump between hits, even while typing. `↵` locks
the query; then `+`/`-` widen or narrow the context around each hit (like `grep -C`), `/`
edits the query again, and the other keys act on the selected hit. `Esc` clears it.

Search reaches past the revset: when the query matches bookmarks (local or remote) that
aren't loaded, the best few are pulled in once typing pauses, with their branch from its
fork point off trunk, so a contributor's PR branch shows up where it sits. They drop out
when the search does.

Search reaches into files too. Once typing pauses on a query of three or more characters,
jj looks through all of history for revisions that changed a path containing it, or added
or removed a line containing it, ignoring case (up to 50 of each). Those are hits as well,
pulled in where they sit if they weren't loaded: the row carries `▤` for a path, `±` for a
line, and the snippet shows the path or the line (`line: fn holds_repo(path: &Path) …`). `v`
on one shows its diff. The fuzzy matcher's own syntax is dropped for this (`'`, `^`, `$`),
and a negated query (`!word`) doesn't search the files.

`j`/`k` move over revisions rather than tasks (over hits while searching), and the log
scrolls exactly like the agent view (same `↑ N more` / `↓ N more` markers). Each view keeps
its own selection, and the log's follows its revision across refreshes.

These keys act on the selected revision:

- `n` starts a new task forked from it: the agent's workspace is an empty child of the
  selected revision rather than of the fork point.
- `N` (*spawn*) starts a new task that edits it: the agent's workspace sits on the revision
  itself and continues that commit. Your own `@` is left alone, unless the revision *is*
  your `@`, in which case this is the usual hand-off. On a revision another workspace is
  already on, the agent spawns in that workspace, which stays where it is; faff refuses
  only when that workspace is already an agent workspace. It refuses an immutable
  revision. If the revision is below your `@`, the agent's edits rebase your `@` as they
  land, as any rewrite in jj would.
- `f` makes a workspace of your own forked from it, by `n`'s recipe: the fork point is the
  newest revision at or below the selection with changes, and if that is the selection
  and a workspace (yours, an agent's, any) is on it, that workspace is advanced with
  `jj new` first. The new workspace is an empty child of the fork point. So `f` on an
  agent's revision gives you a workspace on its work while the agent carries on.
- `F` makes a workspace that takes the revision itself, as `N` does for an agent: the
  workspace that was on it (if any) moves off onto a fresh empty revision on the fork
  point below. Refused on an agent's revision, and on an immutable one.

  Either way the workspace goes in faff's workspace directory under a made-up name (two
  adjectives and a noun, like `brisk-amber-otter`), with its Claude memory linked to the
  repo's and its folder pre-trusted, as for an agent workspace, but no hooks and no
  agent. The notice says where it is; `N` on its revision later spawns an agent in it.
- `d` describes it. On an agent's revision it asks that agent, exactly as in the agent view
  (describing its working copy from outside would leave its workspace stale). On any other
  revision faff hands the diff to a one-shot `claude -p` in the background and sets what
  comes back with `jj describe`. While claude thinks, the revision's row shows a spinner,
  in both views; the result, or why it failed, lands on the notice line.
- `c` copies its change id to the clipboard (an OSC 52 escape, so it works over SSH too).
- `e` runs `jj edit` on it, moving your `@` there. Refused while another jj command that
  could rewrite commits is running in the repo.
- `v` shows the branch as one diff, from where it forked off trunk up to the selected
  revision (`jj diff --from 'fork_point(<rev> | trunk())' --to <rev>`), in your pager. On a
  merge already in trunk it's what the merge brought in (from its first parent); on any
  other revision already in trunk, its own diff. It reads without snapshotting. Quitting the
  pager (or closing an external diff tool) returns you to the log; if it was over within
  200ms (a pager like `less -F` exiting by itself on a short diff) or failed, faff waits for a
  key first so you can read it.

  To use one of your jj `merge-tools` instead of the pager, name it in jj's config. A
  `faff.*` key rather than `ui.diff-formatter`, so your agents' `jj diff` keeps its default:

  ```toml
  [faff]
  diff-tool = "zed"      # v runs `jj diff --tool zed`
  diff-tool-gui = true   # it opens its own window: run it beside faff, which stays live
  ```

  Without `diff-tool-gui`, the tool gets the terminal as the pager would. faff reads these
  on every `v`, so a change applies straight away.
- `z` squashes the branch for review: a new revision on the fork point holding everything
  up to the selected revision, called `squashed: <bookmark>`. No working copy moves; the
  new revision is selected, and `e` edits it. On a merge already in trunk it squashes what
  the merge brought in; on any other revision already in trunk it's refused.
- `x` / `X` remove the workspace on it, as they remove a task (see
  [Removing a task](#removing-a-task)); an agent's goes with its task. With no workspace on
  it, the revision itself is abandoned, `x` only if it's empty. Refused on your own `@`, or
  while a jj command that could rewrite commits runs in the repo or that workspace.

The keys that act on a task (`↵` `s` `S` `r` `R` `a`) don't apply in the log view,
so it doesn't show them; `Tab` back to the agent view to use them.

### `faff logs`

```sh
faff logs                        # your `revsets.log`, in the cwd's checkout
```

faff's log view on its own, meant to replace `watch jj peek` in a spare pane: the same
revisions, rows, search, folds and keys, just without `Tab` to the agent view. It reads
faff's database (read-only) so agents' rows carry their labels and status, and `d` on an
agent's revision asks that agent; but it creates no tasks (`n`, `N`) and drives no merge
train (`A`), so it runs happily beside faff (or another `faff logs`). `x` / `X` on an agent's
revision remove its workspace but leave the task and pane to faff, which drops the task once
it sees the workspace gone.

- Like faff, it reloads the moment jj records an operation, by watching the repo's op
  heads, with a slow poll as a backstop. Reads never snapshot.
- Unlike faff, it snapshots the checkout it was started in when files change (ignoring
  `.jj`, `.git` and gitignored paths), a second after they go quiet. It never snapshots
  while a jj
  command that could rewrite commits is running in the checkout (`git fetch`, a rebase, a
  `describe` waiting on its editor, …) — that race is what makes changes go divergent. The
  header says what it's waiting on (`snapshot deferred: jj git fetch`). Read-only commands
  (`log`, `diff`, `st`, aliases that run them like `peek`, and `util exec`) don't hold it up.
Its keys are `f` `F` `←→` `d` `c` `e` `v` `z` `/` `q` `?` `!`.

### Notices and the toolbar

The line above the toolbar is the notice line. It shows the most recent thing faff has
to say and stays there until the next notice replaces it — no timer, so a result can't
scroll away while you're reading the agent pane. Each notice carries a level:

| | | |
| --- | --- | --- |
| `✓` green | success | `snapshotted #7`, `swapped @ ⇄ #7` |
| `✗` red | attempted and failed | `snapshot failed: <jj's error>` |
| `!` yellow | refused, nothing attempted | `no workspace to snapshot for this task` |
| `?` cyan | an armed confirmation | `#7 is working — press s to confirm swap` |
| `·` grey | neutral | `swap cancelled` |

The yellow/red distinction matters: yellow is a key that didn't apply to the selected
row, red is jj or WezTerm actually failing. A long error wraps over up to three lines;
anything past that is in the log.

`!` opens the notice log — the last 100 notices, newest first, timestamped. Merge-train
notices (`#7 has conflicts — dropped`, `merged #7 into your workspace`) fire with no
keypress behind them, so the log is where you catch up on what happened while you were
in the agent pane. The `!` in the toolbar carries an unseen count (`!3`) coloured by the
worst unseen level. Any key closes the log.

`?` opens the full keymap, including `j`/`k` and `R`.

The toolbar groups keys by what they do, and colours each group: **new** (`n` `N`, plus
`f` `F` in a log view),
**open** (`↵`, or `←→` in a log view), **rev** (`s` `S` `r` `R` `d` `c`, plus `e` `v` and
`z` in a log view), **train** (`a` `A`), **del** (`x` `X`), then `/` `⇥` `q` `?` `!`. Each
view shows only the keys that apply in it; within a view, keys that don't apply to the
selected row are dimmed rather than hidden, so nothing moves as the selection does. When
faff is docked beside a session and the pane is too narrow for the full labels, the toolbar
drops to its compact form — same groups, same order, just the letters:

```
 new nN │ open ↵ │ rev sSrRdcv │ train aA │ del xX │ /⇥ q?!
```

While searching, the toolbar leads with the search's own keys.

## How state moves

```
injected hooks → faff report-event → SQLite + socket nudge → TUI refresh
```

`report-event` writes the database, then nudges the socket
(`$XDG_RUNTIME_DIR/faf-<hash>.sock`) so a running TUI refreshes sooner. Events still land
with the TUI closed. Refresh is throttled to ~1s idle, 400ms floor while events arrive; jj
itself is only asked again once it has recorded an operation (faff watches the repo's op
heads), or every 10s as a backstop.

Hooks injected per workspace:

| Hook | Effect |
|---|---|
| `UserPromptSubmit` | status → working; first prompt captured |
| `Stop` | status → idle |
| `Notification` | needs input — but only if the agent was *working* (a permission prompt); a notification while already idle is Claude Code's ~60s "waiting for your input" notice and is ignored, so a finished agent isn't stuck showing `!` |
| `PostToolUse` | appends an activity row; clears a stale needs-input |
| `SessionStart` | records the claude session id, overwriting the one faff minted at task creation |

Each refresh also reconciles: a task whose pane has died goes back to idle (its agent can be
brought back with `Enter` — see [Reviving a lost agent](#reviving-a-lost-agent)), and a task
whose jj workspace has vanished is dropped.

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
| `workspace` | fork, memory seed, hook injection, trust, teardown, swap, snapshot, take-over |
| `wezterm` | `wezterm cli` argv, exec, list parsing |
| `events` | event enum and Unix-socket transport |
| `scheduler` | applies events to the store |
| `cli` | argument parsing and the `report-event` subcommand |
| `watch` | `faff logs`' watchers (op heads, files minus ignores) and the running-jj guard |
| `tui` | ratatui app: state, event loop, rendering, actions |
| `tui::logview` | the log view's folds, search and selection, shared by faff and `faff logs` |
| `tui::logs` | `faff logs`' engine: load, reload, snapshot |
| `tui::revops` | what both engines share: the log view's revset, and its `e`/`z`/`f`/`F` writes |
| `tui::train` | the merge train: accepted-revision set, per-member stage, membership ops |
| `tui::session` | pure session decisions: the `Enter` toggle, revivability, and the `claude` launch argv |
