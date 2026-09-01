# TUI notifications and toolbar — design

Date: 2026-09-01
Status: approved, ready for implementation planning

## Problem

`render_footer` (`src/tui/mod.rs:1478`) renders one line: 115 columns of key hints,
then `self.status` appended after three spaces. Notifications therefore begin at
column 118.

Docked beside a Claude pane, faff gets roughly 60–80 columns. Every notification is
off-screen. This is not crowding that a shorter message would fix — at that width the
notification slot is structurally unreachable:

| Message | Column it ends at |
| --- | --- |
| `snapshotted #7` | 132 |
| `#7 is working — press s to confirm swap, any other key cancels` | 180 |
| `snapshot failed: <jj stderr>` | 191 |

Two further defects:

- All 48 messages share one untyped `String`. Success, hard failure, "you can't do
  that", armed confirmations, and cancellations are indistinguishable to the renderer,
  so none of them can be coloured or prioritised.
- `R` (rebase-onto-parent) and `j`/`k` exist in `input.rs` but appear nowhere in the UI.

## Non-goals

- Splitting `src/tui/mod.rs` (2339 lines) into a separate render module. Deferred by
  explicit decision; do not do it as part of this work.
- Timed/auto-expiring notifications. Notices are sticky until superseded.
- Changing any existing keybinding. `?` and `!` are added; nothing moves.

## Design

### 1. `src/tui/notify.rs` (new module)

```rust
pub enum Level { Info, Success, Warn, Error, Prompt }

pub struct Note {
    pub level: Level,
    pub text: String,
    pub at: chrono::DateTime<chrono::Local>,
}

pub struct Notices {
    log: VecDeque<Note>,   // newest at the front, capped at CAP
    unseen: usize,
}
```

`CAP = 100`.

Public API:

| Method | Purpose |
| --- | --- |
| `push(level, text)` | append; evict oldest past `CAP`; `unseen += 1` |
| `info` / `ok` / `warn` / `err` / `prompt` | convenience wrappers over `push` |
| `latest() -> Option<&Note>` | the note shown on the notice line |
| `iter()` | newest-first, for the log overlay |
| `unseen() -> usize` | badge count |
| `max_unseen_level() -> Option<Level>` | badge colour |
| `mark_seen()` | `unseen = 0`; called when the overlay opens |

`Level` keeps a natural declaration order and exposes an explicit
`fn severity(&self) -> u8` (`Info` 0, `Success` 1, `Prompt` 2, `Warn` 3, `Error` 4)
rather than a derived `Ord`, so the ordering is readable at its definition.

Each level carries a glyph and a colour:

| Level | Glyph | Colour | Meaning |
| --- | --- | --- | --- |
| `Success` | `✓` | Green | the operation completed |
| `Error` | `✗` | Red | the operation was attempted and failed |
| `Warn` | `!` | Yellow | refused — nothing was attempted |
| `Prompt` | `?` | Cyan, bold | an armed confirmation awaits a keypress |
| `Info` | `·` | DarkGray | neutral |

The Warn/Error split is the point of the exercise: today `no workspace to snapshot`
and `snapshot failed: <jj stderr>` are visually identical, but one is a mis-pressed key
and the other is jj failing.

### 2. Call-site migration

`App.status: String` is replaced by `App.notices: Notices`. All 48 assignments become
levelled calls:

**Success** — `new task #N`, `handed off #N`, `removed #N`, `removed #N and discarded
its revision`, `swapped @ ⇄ #N`, `snapshotted #N`, `sent rebase to #N`, `sent describe
to #N`, `#N accepted into the merge train`, `#N removed from the merge train`, `merge
train aborted — N dequeued`, `merged #N into your workspace`.

**Error** — `new task failed`, `handoff failed`, `swap failed`, `snapshot failed`,
`rebase failed`, `rebase send failed`, `describe send failed`, `merge of #N failed`,
`jj error`.

**Warn** — `no WEZTERM_PANE; run faff inside WezTerm`, `no session for this task`, `no
workspace to swap/snapshot/rebase/describe/accept for this task`, `no
workspace/pane to rebase for this task`, `no pane to describe for this task`, `no live
pane to send the rebase prompt to`, `no live pane to send the describe prompt to`, `no
live pane to drive #N through the merge train`, `send the task its first prompt before
rebasing/describing/accepting`, `#N is already on the latest base`, `#N has no content
to merge`, `your @ has content — commit or hand it off before accepting`, `no merge
train to abort`, `#N has conflicts — dropped from the merge train`, `#N has no content
to merge — dropped`.

**Prompt** — the three armed confirmations (`press s/r/d to confirm …`).

**Info** — `swap cancelled`, `rebase cancelled`, `describe cancelled`. Initial state is
`Info("ready")`.

Note that `#N has conflicts — dropped`, `#N has no content to merge — dropped`, `merged
#N into your workspace`, `merge of #N failed`, and `jj error` fire from the refresh /
merge-train driving path, with no keypress behind them. These are precisely the notices
the user misses while looking at the agent pane, and the main justification for keeping
a log rather than only a current-notice line.

### 3. Layout

Vertical constraints become:

```
Length(1)          header        (unchanged)
Min(1)             revision graph
Length(notice_h)   notice line   (1..=3)
Length(1)          key row
Length(train_h)    merge train   (unchanged, only when non-empty)
```

The notice line always occupies at least one row, so the graph never jitters as
notices change. `notice_h = min(3, wrapped_line_count(text, width))`, letting a long jj
error be read in full without an unbounded chrome budget. Rendering uses
`Paragraph::wrap(Wrap { trim: false })` with continuation lines indented under the text,
clear of the glyph.

Docked, 70 columns:

```
 faff · swarm · 3 working · ▶ #7
 @   [kmkxwzqr] your work
 ├─● [rzqlvksp] #7 ⚙ :: Convert bridges to JSON
 ◆   [yuvnmxxo] fork point
 ✗ snapshot failed: jj util snapshot: No such file or
   directory (os error 2)
 new nN │ open ↵ │ rev sSrRd │ train aA │ del xX │ q ? !3
```

### 4. Key row — static groups, adaptive labels

Groups and key order never change. Only label verbosity adapts, so the docked view is
recognisably the same toolbar as the wide one.

```rust
struct Group { label: &'static str, color: Color }
struct KeyHint { key: &'static str, verbose: &'static str, group: GroupId }
```

| Group | Keys | Colour |
| --- | --- | --- |
| `new` | `n` `N` | Green |
| `open` | `↵` | Blue |
| `rev` | `s` `S` `r` `R` `d` | Cyan |
| `train` | `a` `A` | Magenta |
| `del` | `x` `X` | Red |
| (app) | `q` `?` `!` | DarkGray |

Verbose form — `key verbose` pairs joined by two spaces within a group, groups joined
by ` │ ` (128 columns as rendered):

```
 n new  N handoff  ↵ open │ s swap  S snap  r rebase  R onto  d desc │ a accept  A abort │ x rm  X drop │ q quit  ? keys  ! log
```

Compact form — `group.label` followed by concatenated keys (56 columns):

```
 new nN │ open ↵ │ rev sSrRd │ train aA │ del xX │ q ? !
```

**Selection is by fit, not by a width constant.** Render the verbose form when its
measured width is `<= area.width`, otherwise the compact form; if even the compact form
overflows (below ~56 columns) truncate it with `…`. Both forms are generated from the
same `KeyHint` data, so their widths are derived rather than hardcoded and the two can
never drift out of sync. An earlier draft of this spec fixed the threshold at 100
columns, which was measured against a form that abbreviated the trailing `q`/`?`/`!`
group; deriving the threshold removes that class of error.

`↵` renders as `open` or `detach` depending on whether the selected task's pane is the
docked one, matching today's behaviour.

**Availability dimming.** Keys invalid for the selected row render dimmed rather than
disappearing, so nothing shifts position. Availability is computed once per frame from a
cheap `Caps { has_ws, has_pane, has_prompt, train_nonempty }` derived from the selected
task:

| Key | Available when |
| --- | --- |
| `n` `N` `x` `X` `q` `?` `!` | always |
| `↵` | `has_pane` |
| `s` `S` | `has_ws` |
| `r` `R` `d` | `has_ws && has_pane && has_prompt` |
| `a` | `has_ws && has_pane && has_prompt` |
| `A` | `train_nonempty` |

`Caps` is deliberately an approximation of the real guards. In particular `a`'s
emptiness check (`jj::any_revision`) and the clear-`@` check are **not** consulted —
they shell out to jj, and render must stay pure and cheap. The authoritative guards
stay inside the action functions, where they produce the Warn notices. Dimming is a
hint, not an enforcement.

### 5. Log overlay

`!` sets `App.show_log = true` and calls `notices.mark_seen()`. The overlay is drawn
over the graph area with `ratatui::widgets::Clear`, newest first, timestamped, wrapped:

```
┌─ notices ───────────────────────────────────────────┐
│ 13:42:11 ✗ snapshot failed: jj util snapshot: No    │
│            such file or directory (os error 2)      │
│ 13:41:58 ✓ swapped @ ⇄ #7                           │
│ 13:41:02 ! no live pane to drive #3 through the     │
│            merge train                              │
│                                     any key closes  │
└─────────────────────────────────────────────────────┘
```

`?` opens the same overlay in keymap mode, listing every binding with its full
description (including `j`/`k`, which have no toolbar entry).

**Input handling.** `handle_key` gains a modal check *before* the existing `pending_*`
checks: if `show_log` is set, any key closes the overlay and returns. Ordering is safe
because an armed confirmation cannot coexist with an open overlay — while a `pending_*`
is set, pressing `!` is swallowed as "any other key cancels", which is the correct
existing behaviour. `input::map_key` gains `Action::ShowLog` (`!`) and `Action::Help`
(`?`); its doc comment already scopes it to the non-modal view, and the overlay is
handled in `mod.rs` rather than by a second keymap.

### 6. Badge

The key row ends with `!` plus the unseen count when non-zero (`!3`), coloured by
`max_unseen_level`. It clears when the overlay opens.

## Testing

New:

- `notify` unit tests: cap eviction at 100, newest-first ordering, `unseen` counting
  and clearing, `max_unseen_level` picks the highest severity.
- **Regression test for the reported bug**: render the app at 60 columns with a
  `snapshotted #7` notice and assert the string appears in the `TestBackend` buffer.
  This test fails against today's code.
- Key row fits: compact form renders within 60 columns without truncation.
- Verbose/compact selection: the verbose form renders at its own measured width, the
  compact form one column below it, and the compact form is truncated below 56 columns.
- Every key in the `Action` enum appears in exactly one group, so no binding can be
  added to `input.rs` without also surfacing in the toolbar or the `?` overlay.
- Overlay opens on `!`, closes on any key, and clears the unseen count.
- Dimming: a task with no workspace renders `s`/`S` dimmed; `A` is dimmed with an empty
  train.

Migrated: the 12 existing `app.status.contains(...)` / `assert_eq!(app.status, ...)`
assertions become assertions over `app.notices.latest()`, checking level as well as
text — e.g. the swap-confirmation test asserts `Level::Prompt`, and `swap cancelled`
asserts `Level::Info`.

`header_and_footer_render_without_panicking` extends to cover the notice line, the
overlay, and both toolbar widths.

## Files touched

| File | Change |
| --- | --- |
| `src/tui/notify.rs` | new — `Level`, `Note`, `Notices`, unit tests |
| `src/tui/mod.rs` | `status` → `notices`; 48 call sites; `render_footer` split into `render_notice` + `render_keys`; overlay; `show_log` field; layout constraints |
| `src/tui/input.rs` | `Action::ShowLog`, `Action::Help`, `?` / `!` mappings |
| `README.md` | document the notice line, the log overlay, `?` / `!`, and the group colours |

`chrono` is already a dependency with the `clock` feature, so `Local::now()` needs no
manifest change. No new dependencies.
