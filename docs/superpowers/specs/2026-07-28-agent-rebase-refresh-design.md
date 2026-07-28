# Design: `r` / `R` — refresh a running agent onto a newer base

Date: 2026-07-28
Status: approved, ready for implementation plan

## Problem

Each faff task forks a `jj` workspace off the newest non-empty ancestor of your `@` at
creation time. As you keep working and your `@` advances, that fork point goes stale: the
agent is building on an ever-older base.

Today the only remedy is `s` (swap), which trades your `@` with the agent's revision — it
*adopts the agent's work into your repo* as a side effect. That's the wrong tool when you
just want to keep the agent fresh without pulling its (possibly unfinished) work onto your
own line.

We want a way to re-base a *running* agent forward, in place, without adopting its work and
without faff itself performing a rebase.

## Core idea

faff can already drive each agent's WezTerm pane (`spawn`, `get-text`, `kill-pane`). Add the
send side — `wezterm cli send-text` — so faff can inject a prompt into a running agent. Then:

- faff computes the new base (the same fork-point recipe `workspace::create` uses).
- faff injects a prompt telling the **agent** to rebase itself onto that base and continue.

The agent performs the rebase, so faff honors its stated principle — *"faff does not review,
rebase, or merge."* The agent, which holds the live working copy, also owns any conflict
resolution. Claude Code already queues input typed mid-turn, so a busy agent queues the
prompt for free — faff builds no queue of its own.

## Keys

| Key | Base it targets | jj write by faff? |
|---|---|---|
| `r` | Latest possible fork point, **freezing your WIP** first (identical to `create`: `jj new` when `@` is itself the fork point). Hands the agent your uncommitted work. | Yes — `jj new` on **your** `@` only, never the agent's. Same write `n` already performs. |
| `R` (shift+r) | Your **parent** / last committed line (`heads(::@- ~ empty())`). WIP not included. | No — fully read-only. |

## Mechanics

### 1. New wezterm primitive

In `wezterm.rs`, mirroring `get_text`:

```
send_text_args(pane_id, text) -> ["cli", "send-text", "--no-paste", "--pane-id", <id>]
```

The prompt text is written to the child's stdin. `--no-paste` makes the input behave as
typed rather than a bracketed paste, so the terminating carriage return actually submits.
faff sends the prompt line followed by `\r`. Add the exec wrapper `send_text()` and an argv
unit test alongside the existing `argv_builders_are_exact` test.

### 2. Fork-point computation (reuse, don't duplicate)

`workspace::create` already computes `heads(::@ ~ empty())` and does the `jj new` freeze.
Extract that into a small reusable function so `create` and `refresh` share one recipe:

```
resolve_fork_point(repo, freeze: bool) -> change_id
```

- `r`  → `freeze: true`  — resolves `heads(::@ ~ empty())`, runs `jj new` when `@` is that
  commit (freezes your WIP), returns the frozen change id.
- `R`  → `freeze: false` — resolves `heads(::@- ~ empty())` (your parent line), never writes.

`create` is refactored to call the `freeze: true` path so there is exactly one implementation
of the recipe.

### 3. The injected prompt

faff removes all ambiguity by computing the exact jj invocation. `jj rebase -b @ -d <base>`
moves the agent's whole line onto the new base without faff needing to know the agent's old
fork point:

```
Your task's base has moved. Run: jj rebase -b @ -d <new-base-change-id> — then resolve any
conflicts and continue your task.
```

Sent as a single line + `\r`. (Decision: exact command over vague intent — deterministic,
while the agent still owns conflict resolution, which was the reason for "agent does it.")

### 4. No-op guard

If the agent's revision already descends from the computed base, there is nothing to do.
`r`/`R` surfaces a status line ("already fresh") and injects nothing, mirroring how `swap`
bails with "nothing to swap." Detection: the agent head is a descendant of the base
(`base::` contains the agent head, or equivalently the agent head is in `descendants(base)`).

### 5. Confirm on a live agent

Injecting a redirect into an actively-working agent is disruptive, so `r`/`R` on a task whose
status is `working` asks for confirmation once, then goes through — reusing swap's
first-`s`/second-`s` confirm pattern in the TUI.

## Code touch points

| File | Change |
|---|---|
| `wezterm.rs` | `send_text_args` (pure) + `send_text()` exec wrapper; extend the argv unit test. |
| `workspace.rs` | Extract `resolve_fork_point(repo, freeze)`; refactor `create` onto it; add `refresh()` that computes the base, no-op-guards, and returns the prompt string (or a "already fresh" signal). |
| `tui` | `r` / `R` key handlers → `refresh()` → `wezterm::send_text(pane_id, prompt)`; confirm-on-busy state reusing the swap confirm pattern; status-line messaging for no-op and "sent". |
| `README.md` | New rows in the key table; a short "Refreshing an agent (`r` / `R`)" section. |

## Edge cases

- **Brand-new task with no prompt yet.** The `UserPromptSubmit` hook captures the *first*
  prompt as the task title. If `r` fires before you've typed anything, the injected rebase
  text would become the title. Guard: skip/defer `r`/`R` until the task has a captured title
  (i.e. the user has sent at least one real prompt).
- **Dead pane.** If the agent's pane has died (faff already reconciles this to idle on
  refresh), `r`/`R` reports that instead of sending into a nonexistent pane.
- **Half-applied is impossible here.** faff's only write is `jj new` on your own `@` (for
  `r`); the agent's rebase is atomic from faff's perspective (faff just sends text). No
  multi-step trade to leave half-done, unlike `swap`.

## Testing

- Unit: `send_text_args` argv exactness; `resolve_fork_point` freeze vs. non-freeze against a
  scratch jj repo (extends the existing `workspace.rs` integration harness).
- Unit: no-op guard — agent already a descendant of the base returns the "already fresh"
  signal and no prompt.
- Manual: `r`/`R` against a live agent in WezTerm, confirming queue-while-busy behaviour and
  that the emitted `jj rebase -b @ -d <rev>` moves the agent's line as expected.

## Non-goals

- faff does not run `jj rebase` itself, does not resolve conflicts, does not merge.
- No faff-side message queue — Claude Code's own input queue handles busy agents.
- No change to `s` (swap) or `S` (snapshot); `r`/`R` is complementary to them.
