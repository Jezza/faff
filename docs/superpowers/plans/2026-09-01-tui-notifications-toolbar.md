# TUI Notifications and Toolbar Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give faff a levelled, always-visible notification system and a grouped, colour-coded toolbar that fits a docked pane.

**Architecture:** A new `src/tui/notify.rs` replaces `App.status: String` with a capped, levelled `Notices` log. The single overloaded footer line splits into a dedicated notice line (sticky, wrapping to at most three rows) above a key row built from static `Hint` data that renders verbose or compact depending on measured fit. A `!` overlay shows notice scrollback; `?` shows the full keymap.

**Tech Stack:** Rust 2024 edition, ratatui 0.29 (`TestBackend` for render assertions), chrono 0.4 with the `clock` feature (already a dependency), rusqlite in-memory store for test fixtures. Version control is **jj**, not git.

**Spec:** `docs/superpowers/specs/2026-09-01-tui-notifications-toolbar-design.md`

## Global Constraints

- **Do not commit.** All six tasks land in the current jj revision (`@`). jj snapshots the working copy automatically, so editing the files is enough — do not run `jj commit`, `jj new`, or `git commit` at any point. Task boundaries are review checkpoints, not commit boundaries.
- **No new dependencies.** `chrono` already has the `clock` feature; everything else is in `Cargo.toml` already.
- **Do not split `src/tui/mod.rs` into a render module.** It is 2339 lines and that bothers people, but the split is explicitly deferred (spec "Non-goals"). Adding `notify.rs` is the only new file.
- **Do not change any existing keybinding.** `?` and `!` are added; `n N ↵ s S r R d a A x X q j k` and the arrow keys keep their current meanings and their current positions in the toolbar.
- **Run the full suite** with `cargo test` from the repo root. Individual tests run as `cargo test <name> -- --exact` or `cargo test <substring>`.
- The crate has no `#![deny(warnings)]`, so an intermediate commit that emits a `dead_code` warning is acceptable and is called out where it happens. Do not silence such a warning with `#[allow(dead_code)]` — the next task removes it.

---

### Task 1: The `notify` module

Self-contained and pure: levels, notes, the capped log, and the word-wrap helper the notice line needs. Nothing consumes it yet.

**Files:**
- Create: `src/tui/notify.rs`
- Modify: `src/tui/mod.rs:6-9` (the `mod` declarations)
- Test: inline `#[cfg(test)] mod tests` in `src/tui/notify.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub enum Level { Info, Success, Prompt, Warn, Error }` with `pub fn severity(self) -> u8`, `pub fn glyph(self) -> &'static str`, `pub fn color(self) -> ratatui::style::Color`
  - `pub struct Note { pub level: Level, pub text: String, pub at: chrono::DateTime<chrono::Local> }`
  - `pub struct Notices` (implements `Default`) with `push(Level, impl Into<String>)`, `info`, `ok`, `warn`, `err`, `prompt`, `latest() -> Option<&Note>`, `iter() -> impl Iterator<Item = &Note>`, `unseen() -> usize`, `max_unseen_level() -> Option<Level>`, `mark_seen()`
  - `pub fn wrap_words(text: &str, width: usize) -> Vec<String>`

- [ ] **Step 1: Write the failing tests**

Create `src/tui/notify.rs` containing *only* the test module for now, so the first `cargo test` genuinely fails to compile against absent items:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_is_the_most_recent_push() {
        let mut n = Notices::default();
        n.ok("first");
        n.err("second");
        let latest = n.latest().expect("a note was pushed");
        assert_eq!(latest.text, "second");
        assert_eq!(latest.level, Level::Error);
    }

    #[test]
    fn iter_yields_newest_first() {
        let mut n = Notices::default();
        n.ok("a");
        n.ok("b");
        n.ok("c");
        let texts: Vec<_> = n.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(texts, vec!["c", "b", "a"]);
    }

    #[test]
    fn log_evicts_the_oldest_past_the_cap() {
        let mut n = Notices::default();
        for i in 0..(CAP + 10) {
            n.ok(format!("note {i}"));
        }
        assert_eq!(n.iter().count(), CAP, "log is capped");
        assert_eq!(n.latest().unwrap().text, format!("note {}", CAP + 9));
        let oldest = n.iter().last().unwrap();
        assert_eq!(oldest.text, "note 10", "the first 10 were evicted");
    }

    #[test]
    fn unseen_counts_pushes_and_clears_on_mark_seen() {
        let mut n = Notices::default();
        assert_eq!(n.unseen(), 0);
        n.ok("a");
        n.warn("b");
        assert_eq!(n.unseen(), 2);
        n.mark_seen();
        assert_eq!(n.unseen(), 0);
        n.err("c");
        assert_eq!(n.unseen(), 1);
    }

    #[test]
    fn unseen_never_exceeds_what_the_log_holds() {
        let mut n = Notices::default();
        for i in 0..(CAP + 50) {
            n.ok(format!("note {i}"));
        }
        assert_eq!(n.unseen(), CAP, "badge cannot claim more notes than are kept");
    }

    #[test]
    fn max_unseen_level_picks_the_highest_severity() {
        let mut n = Notices::default();
        n.info("a");
        n.err("b");
        n.warn("c");
        assert_eq!(n.max_unseen_level(), Some(Level::Error));
    }

    #[test]
    fn max_unseen_level_ignores_notes_already_seen() {
        let mut n = Notices::default();
        n.err("an old failure");
        n.mark_seen();
        n.ok("a fresh success");
        assert_eq!(
            n.max_unseen_level(),
            Some(Level::Success),
            "the seen error must not keep colouring the badge red"
        );
    }

    #[test]
    fn max_unseen_level_is_none_when_nothing_is_unseen() {
        let mut n = Notices::default();
        n.ok("a");
        n.mark_seen();
        assert_eq!(n.max_unseen_level(), None);
    }

    #[test]
    fn severity_orders_info_below_error() {
        assert!(Level::Error.severity() > Level::Warn.severity());
        assert!(Level::Warn.severity() > Level::Prompt.severity());
        assert!(Level::Prompt.severity() > Level::Success.severity());
        assert!(Level::Success.severity() > Level::Info.severity());
    }

    #[test]
    fn wrap_words_breaks_on_spaces_within_the_width() {
        assert_eq!(
            wrap_words("the quick brown fox", 10),
            vec!["the quick", "brown fox"]
        );
    }

    #[test]
    fn wrap_words_keeps_a_short_text_on_one_line() {
        assert_eq!(wrap_words("snapshotted #7", 40), vec!["snapshotted #7"]);
    }

    #[test]
    fn wrap_words_hard_breaks_a_word_longer_than_the_width() {
        // A jj error can carry one long unbroken path with no space to break on.
        assert_eq!(
            wrap_words("/very/long/path/that/never/breaks", 10),
            vec!["/very/long", "/path/that", "/never/bre", "aks"]
        );
    }

    #[test]
    fn wrap_words_never_returns_an_empty_vec() {
        assert_eq!(wrap_words("", 10), vec![String::new()]);
    }

    #[test]
    fn wrap_words_tolerates_zero_width() {
        // Rect widths can be 0 during a resize; this must not divide by zero or loop.
        assert_eq!(wrap_words("hello", 0), vec!["hello"]);
    }
}
```

Add the module declaration in `src/tui/mod.rs`, keeping the existing list alphabetical:

```rust
mod input;
mod model;
mod notify;
mod session;
mod train;
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib tui::notify`
Expected: FAIL — compilation errors, `cannot find type Notices in this scope`, `cannot find function wrap_words in this scope`, `cannot find value CAP in this scope`.

- [ ] **Step 3: Write the implementation**

Insert above the test module in `src/tui/notify.rs`:

```rust
//! Levelled notifications for the TUI: a capped log plus the current notice.
//!
//! Replaces the single `status: String` the app used to carry. The renderer could not
//! tell a success from a refusal from a hard failure through a bare string, so none of
//! them could be coloured or prioritised; a `Level` per note fixes that.

use chrono::{DateTime, Local};
use ratatui::style::Color;
use std::collections::VecDeque;

/// How many notices the log retains before evicting the oldest.
const CAP: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Neutral — a cancellation, or a note about state.
    Info,
    /// The operation completed.
    Success,
    /// An armed confirmation is waiting for the next keypress.
    Prompt,
    /// Refused: a guard rejected the action, so nothing was attempted. Distinct from
    /// `Error` — this is a mis-pressed key, not a broken tool.
    Warn,
    /// The operation was attempted and failed.
    Error,
}

impl Level {
    /// Ranking used to colour the unseen badge. Written out rather than derived from
    /// the variant order, so the ordering is visible where it is defined.
    pub fn severity(self) -> u8 {
        match self {
            Level::Info => 0,
            Level::Success => 1,
            Level::Prompt => 2,
            Level::Warn => 3,
            Level::Error => 4,
        }
    }

    /// Single-column glyph shown at the head of the notice line.
    pub fn glyph(self) -> &'static str {
        match self {
            Level::Info => "·",
            Level::Success => "✓",
            Level::Prompt => "?",
            Level::Warn => "!",
            Level::Error => "✗",
        }
    }

    pub fn color(self) -> Color {
        match self {
            Level::Info => Color::DarkGray,
            Level::Success => Color::Green,
            Level::Prompt => Color::Cyan,
            Level::Warn => Color::Yellow,
            Level::Error => Color::Red,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Note {
    pub level: Level,
    pub text: String,
    pub at: DateTime<Local>,
}

/// The notice log: newest at the front, capped at [`CAP`].
#[derive(Debug, Default)]
pub struct Notices {
    log: VecDeque<Note>,
    /// Pushes since the log was last opened. Saturates at the log length so the badge
    /// can never claim more notes than are actually kept.
    unseen: usize,
}

impl Notices {
    pub fn push(&mut self, level: Level, text: impl Into<String>) {
        self.log.push_front(Note {
            level,
            text: text.into(),
            at: Local::now(),
        });
        self.log.truncate(CAP);
        self.unseen = self.unseen.saturating_add(1).min(self.log.len());
    }

    pub fn info(&mut self, text: impl Into<String>) {
        self.push(Level::Info, text);
    }

    pub fn ok(&mut self, text: impl Into<String>) {
        self.push(Level::Success, text);
    }

    pub fn warn(&mut self, text: impl Into<String>) {
        self.push(Level::Warn, text);
    }

    pub fn err(&mut self, text: impl Into<String>) {
        self.push(Level::Error, text);
    }

    pub fn prompt(&mut self, text: impl Into<String>) {
        self.push(Level::Prompt, text);
    }

    /// The note shown on the notice line: sticky until the next push replaces it.
    pub fn latest(&self) -> Option<&Note> {
        self.log.front()
    }

    /// Newest first, for the log overlay.
    pub fn iter(&self) -> impl Iterator<Item = &Note> {
        self.log.iter()
    }

    pub fn unseen(&self) -> usize {
        self.unseen
    }

    /// Highest severity among the unseen notes — the badge colour.
    pub fn max_unseen_level(&self) -> Option<Level> {
        self.log
            .iter()
            .take(self.unseen)
            .map(|n| n.level)
            .max_by_key(|l| l.severity())
    }

    pub fn mark_seen(&mut self) {
        self.unseen = 0;
    }
}

/// Break `text` into lines of at most `width` columns, splitting on whitespace and
/// hard-breaking any single word longer than `width` (a jj error can carry one long
/// unbroken path). Always returns at least one line.
///
/// The notice line does its own wrapping rather than handing `Paragraph` a `Wrap`,
/// because the layout needs the resulting line *count* to size the notice area before
/// the widget renders.
pub fn wrap_words(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let mut word = word;
        while word.chars().count() > width {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            let cut = word
                .char_indices()
                .nth(width)
                .map(|(i, _)| i)
                .unwrap_or(word.len());
            out.push(word[..cut].to_string());
            word = &word[cut..];
        }
        if word.is_empty() {
            continue;
        }
        if cur.is_empty() {
            cur = word.to_string();
        } else if cur.chars().count() + 1 + word.chars().count() <= width {
            cur.push(' ');
            cur.push_str(word);
        } else {
            out.push(std::mem::take(&mut cur));
            cur = word.to_string();
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib tui::notify`
Expected: PASS, 13 tests.

Then run the whole suite: `cargo test`
Expected: PASS. A `dead_code` warning naming `Notices`, `Level`, `wrap_words` and friends is expected here — nothing consumes the module until Task 2. Do not suppress it.


---

### Task 2: Replace `App.status` with `App.notices`

The field swap breaks compilation until every call site is migrated, so this is one atomic task. Purely a translation — no rendering changes yet, so the footer keeps appending `notices.latest()` where it used to append `status`.

**Files:**
- Modify: `src/tui/mod.rs:144` (field), `src/tui/mod.rs:196` (`App::new`), `src/tui/mod.rs:1521` (`test_app`), all 48 assignment sites, `src/tui/mod.rs:1478-1492` (`render_footer`), and the 12 test assertions at lines 1559, 1563, 1587, 1591, 1610, 1634, 1638, 1657, 1682, 1699, 1712, 1719, 1725
- Test: existing tests in `src/tui/mod.rs`, migrated

**Interfaces:**
- Consumes: `notify::{Level, Note, Notices}` from Task 1.
- Produces: `App.notices: notify::Notices` — every later task reads notices through it. `App.status` no longer exists.

- [ ] **Step 1: Migrate the tests first**

These tests currently assert on a bare string; they must assert on level *and* text, which is the behaviour Task 2 adds. Edit each in `src/tui/mod.rs`. Add this helper next to `test_app`:

```rust
/// The current notice as `(level, text)` — the shape the migrated assertions want.
fn latest(app: &App) -> (notify::Level, String) {
    let n = app.notices.latest().expect("a notice was pushed");
    (n.level, n.text.clone())
}
```

Then rewrite the 12 assertions:

```rust
// was: assert!(app.status.contains("press s to confirm"));
let (level, text) = latest(&app);
assert_eq!(level, notify::Level::Prompt);
assert!(text.contains("press s to confirm"));

// was: assert_eq!(app.status, "swap cancelled");
assert_eq!(latest(&app), (notify::Level::Info, "swap cancelled".to_string()));

// was: assert!(app.status.contains("press r to confirm"));
let (level, text) = latest(&app);
assert_eq!(level, notify::Level::Prompt);
assert!(text.contains("press r to confirm"));

// was: assert_eq!(app.status, "rebase cancelled");
assert_eq!(latest(&app), (notify::Level::Info, "rebase cancelled".to_string()));

// was: assert!(app.status.contains("first prompt"), "status: {}", app.status);
//      (three occurrences — rebase, describe, accept)
let (level, text) = latest(&app);
assert_eq!(level, notify::Level::Warn, "refused, not failed: {text}");
assert!(text.contains("first prompt"), "notice: {text}");

// was: assert!(app.status.contains("press d to confirm"));
let (level, text) = latest(&app);
assert_eq!(level, notify::Level::Prompt);
assert!(text.contains("press d to confirm"));

// was: assert_eq!(app.status, "describe cancelled");
assert_eq!(latest(&app), (notify::Level::Info, "describe cancelled".to_string()));

// was: assert!(app.status.contains("no live pane"), "status: {}", app.status);
let (level, text) = latest(&app);
assert_eq!(level, notify::Level::Warn, "refused, not failed: {text}");
assert!(text.contains("no live pane"), "notice: {text}");

// was: assert!(app.status.contains("removed from the merge train"));
let (level, text) = latest(&app);
assert_eq!(level, notify::Level::Success);
assert!(text.contains("removed from the merge train"));

// was: assert_eq!(app.status, "no merge train to abort");
assert_eq!(
    latest(&app),
    (notify::Level::Warn, "no merge train to abort".to_string())
);

// was: assert!(app.status.contains("aborted"), "status: {}", app.status);
let (level, text) = latest(&app);
assert_eq!(level, notify::Level::Success);
assert!(text.contains("aborted"), "notice: {text}");
```

Leave lines 1844, 1848 and 1850 alone — those are `TaskStatus`, an unrelated `.status`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib tui`
Expected: FAIL — compilation errors, `no field notices on type &App`.

- [ ] **Step 3: Swap the field**

In `src/tui/mod.rs`, replace line 144:

```rust
    /// Levelled notification log. The most recent note is shown on the notice line;
    /// `!` opens the scrollback. Replaces the old untyped `status: String`.
    notices: notify::Notices,
```

In `App::new` (line ~196), replace `status: "ready".to_string(),` with `notices: notify::Notices::default(),` and seed it *after* the struct literal, before `app.refresh()`:

```rust
        app.notices.info("ready");
        // The opening "ready" is not news; don't start the session with a badge.
        app.notices.mark_seen();
        app.refresh();
```

In `test_app` (line ~1521), replace `status: "ready".into(),` with `notices: notify::Notices::default(),`.

- [ ] **Step 4: Migrate all 48 assignment sites**

Mechanical. `self.status = <expr>;` becomes `self.notices.<level>(<expr>);`, dropping any `.to_string()` / `format!` wrapper that is now redundant (`push` takes `impl Into<String>`, so a `&str` passes directly).

**`ok` (Success) — 11 sites** (12 messages — line 512 is one `if/else` site producing two):

| Line | Message |
| --- | --- |
| 336 | `new task #{id} — type your task in the pane` |
| 351 | `handed off #{id} — type what to finish in the pane` |
| 512 | `removed #{}` / `removed #{} and discarded its revision` |
| 582 | `swapped @ ⇄ #{}` |
| 600 | `snapshotted #{}` |
| 659 | `sent rebase to #{}` |
| 713 | `sent describe to #{}` |
| 733 | `#{} removed from the merge train` |
| 760 | `#{} accepted into the merge train` |
| 772 | `merge train aborted — {n} dequeued` |
| 993 | `merged #{} into your workspace` |

Line 512 is an `if/else` producing a `String`; keep the expression and wrap the whole thing:

```rust
        let msg = if discard_revision {
            format!("removed #{} and discarded its revision", id.0)
        } else {
            format!("removed #{}", id.0)
        };
        self.notices.ok(msg);
```

**`err` (Error) — 9 sites:** lines 339 (`new task failed: {e}`), 354 (`handoff failed: {e}`), 584 (`swap failed: {e}`), 601 (`snapshot failed: {e}`), 660 (`rebase send failed: {e}`), 662 (`rebase failed: {e}`), 714 (`describe send failed: {e}`), 995 (`merge of #{} failed: {e}`), 1047 (`jj error: {e}`).

**`warn` (Warn) — 22 sites:** lines 479, 483, 555, 575, 596, 617, 621, 628, 650, 655, 678, 682, 689, 709, 737, 741, 745, 750, 755, 767, 840, 845.

Every one of these is a guard that refused before attempting anything. Keep the message text byte-for-byte identical; only the call changes. For example:

```rust
// line 596
            self.notices.warn("no workspace to snapshot for this task");
            return;
```

**`prompt` (Prompt) — 3 sites:** lines 560, 634, 694 — the armed `press s/r/d to confirm` confirmations.

**`info` (Info) — 3 sites:** lines 257 (`swap cancelled`), 270 (`rebase cancelled`), 281 (`describe cancelled`).

- [ ] **Step 5: Keep `render_footer` compiling**

Task 3 rewrites this properly. For now, just read the text out of the log so the file compiles and behaviour is unchanged:

```rust
        let status = self
            .notices
            .latest()
            .map(|n| n.text.as_str())
            .unwrap_or_default();
        let keys = format!(
            " [n]ew [N]handoff {enter} [s]wap [S]napshot [r]ebase [d]escribe [a]ccept [A]abort [x]remove [X]remove+drop [q]uit   {status}"
        );
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test`
Expected: PASS, all tests. The `dead_code` warning from Task 1 is gone.

Sanity-check nothing was missed:

Run: `grep -n 'self\.status' src/tui/mod.rs`
Expected: no output.


---

### Task 3: Give notices their own line

The actual bug fix. The notice moves off the end of the key row onto a dedicated line above it, sticky and wrapping.

**Files:**
- Modify: `src/tui/mod.rs` — `render` (line ~1211), `render_footer` (line ~1478, split), plus a new `NOTICE_MAX_LINES` constant near `ID_W` (line ~32)
- Test: inline tests in `src/tui/mod.rs`

**Interfaces:**
- Consumes: `notify::wrap_words`, `Notices::latest`, `Level::{glyph, color}` from Task 1; `App.notices` from Task 2.
- Produces: `fn notice_height(&self, width: u16) -> u16`, `fn render_notice(&self, f: &mut Frame, area: Rect)`, `fn render_keys(&self, f: &mut Frame, area: Rect)`. `render_footer` no longer exists. Test helper `fn buffer_text(term: &Terminal<TestBackend>) -> String`.

- [ ] **Step 1: Write the failing tests**

Add to the test module in `src/tui/mod.rs`. `buffer_text` is used by Tasks 3, 4 and 5, so define it once here beside `test_app`:

```rust
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
        let mut app = test_app();
        assert_eq!(app.notice_height(60), 1);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib tui::tests::notice`
Expected: FAIL — `no method named notice_height found for struct App`, `cannot find value NOTICE_MAX_LINES in this scope`.

Run: `cargo test --lib notice_is_visible_in_a_narrow_docked_pane`
Expected: FAIL. Once it compiles this is the regression test for the reported bug; confirm it fails for the right reason (the assertion, not a missing symbol) after Step 3 is partially in.

- [ ] **Step 3: Add the constant and the layout row**

Near `ID_W` at line ~32 in `src/tui/mod.rs`:

```rust
/// Most rows the notice line may occupy. A jj error can be long; three wrapped lines
/// is enough to read one in full without letting chrome crowd out the graph.
const NOTICE_MAX_LINES: u16 = 3;
```

Rewrite `render` (line ~1211) to give the notice its own constraint between the graph and the keys:

```rust
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
        self.render_notice(f, chunks[2]);
        self.render_keys(f, chunks[3]);
        if train_h > 0 {
            self.render_merge_train(f, chunks[4]);
        }
    }
```

- [ ] **Step 4: Replace `render_footer` with `notice_height` + `render_notice` + `render_keys`**

Delete `render_footer` entirely (lines ~1478-1492) and put these in its place:

```rust
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

    /// The key row. Task 4 replaces this body with the grouped, adaptive toolbar.
    fn render_keys(&self, f: &mut Frame, area: Rect) {
        let selected_pane = self.selected_task().and_then(|t| t.pane_id);
        let enter = if self.open_pane.is_some() && self.open_pane == selected_pane {
            "[↵]detach"
        } else {
            "[↵]open"
        };
        let keys = format!(
            " [n]ew [N]handoff {enter} [s]wap [S]napshot [r]ebase [d]escribe [a]ccept [A]abort [x]remove [X]remove+drop [q]uit"
        );
        f.render_widget(
            Paragraph::new(keys).style(Style::default().fg(Color::DarkGray)),
            area,
        );
    }
```

Rename the existing test `header_and_footer_render_without_panicking` to `header_notice_and_keys_render_without_panicking` and leave its body as is.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test`
Expected: PASS, including `notice_is_visible_in_a_narrow_docked_pane`.


---

### Task 4: Grouped, colour-coded, adaptive key row

**Files:**
- Modify: `src/tui/mod.rs` — `render_keys` (rewritten), plus new `Grp`, `Hint`, `HINTS`, `Caps` items placed just above `impl App` (line ~168)
- Test: inline tests in `src/tui/mod.rs`

**Interfaces:**
- Consumes: `buffer_text` / `draw` test helpers from Task 3.
- Produces: `fn keys_line(&self, verbose: bool) -> Line<'static>`, `fn caps(&self) -> Caps`, `fn hint_available(key: &str, c: &Caps) -> bool`, `const HINTS: &[Hint]`. Task 5 reads `HINTS` to build the `?` overlay and appends the badge inside `keys_line`.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn verbose_key_row_is_used_when_it_fits() {
        let mut app = test_app();
        let w = app.keys_line(true).width() as u16;
        let term = draw(&mut app, w, 12);
        assert!(buffer_text(&term).contains("N handoff"), "verbose labels shown");
    }

    #[test]
    fn compact_key_row_is_used_when_verbose_does_not_fit() {
        let mut app = test_app();
        let w = app.keys_line(true).width() as u16 - 1;
        let term = draw(&mut app, w, 12);
        let text = buffer_text(&term);
        assert!(!text.contains("N handoff"), "verbose labels dropped:\n{text}");
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
        for key in ["n", "N", "↵", "s", "S", "r", "R", "d", "a", "A", "x", "X", "q"] {
            assert!(
                HINTS.iter().any(|h| h.key == key),
                "key {key} is bound in input.rs but missing from HINTS"
            );
        }
    }

    #[test]
    fn enter_label_flips_between_open_and_detach() {
        let (mut app, _) = docked_node_app(true);
        let w = app.keys_line(true).width() as u16;
        assert!(buffer_text(&draw(&mut app, w, 12)).contains("↵ detach"));
        let (mut app, _) = docked_node_app(false);
        let w = app.keys_line(true).width() as u16;
        assert!(buffer_text(&draw(&mut app, w, 12)).contains("↵ open"));
    }

    #[test]
    fn keys_are_dimmed_when_unavailable_for_the_selected_row() {
        // A task with no workspace: `s`/`S` are dimmed, `n` is not.
        let mut app = test_app();
        let t = app.store.create_task("x", 0, Autonomy::Inherit).unwrap();
        app.tasks = app.store.list_tasks().unwrap();
        app.task_order = vec![t.id];
        app.selected = 0;
        let c = app.caps();
        assert!(!c.has_ws, "fixture has no workspace");
        assert!(!App::hint_available("s", &c), "swap needs a workspace");
        assert!(!App::hint_available("S", &c), "snapshot needs a workspace");
        assert!(App::hint_available("n", &c), "new task is always available");
        assert!(!App::hint_available("A", &c), "abort needs a non-empty train");
    }

    #[test]
    fn dimming_renders_as_a_dim_modifier_not_a_missing_key() {
        let mut app = test_app();
        let w = app.keys_line(true).width() as u16;
        let term = draw(&mut app, w, 12);
        // The key is still on screen — dimming must never shift positions.
        assert!(buffer_text(&term).contains("s swap"));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib tui::tests`
Expected: FAIL — `cannot find value HINTS in this scope`, `no method named keys_line found for struct App`, `no method named caps found`.

- [ ] **Step 3: Add the toolbar data**

Insert immediately above `impl App {` (line ~168) in `src/tui/mod.rs`:

```rust
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
/// render time (open vs detach). Keep in sync with `input::map_key` — the test
/// `every_action_key_appears_in_the_toolbar_data` enforces it.
const HINTS: &[Hint] = &[
    Hint { key: "n", verbose: "new", grp: Grp::New },
    Hint { key: "N", verbose: "handoff", grp: Grp::New },
    Hint { key: "↵", verbose: "open", grp: Grp::Open },
    Hint { key: "s", verbose: "swap", grp: Grp::Rev },
    Hint { key: "S", verbose: "snap", grp: Grp::Rev },
    Hint { key: "r", verbose: "rebase", grp: Grp::Rev },
    Hint { key: "R", verbose: "onto", grp: Grp::Rev },
    Hint { key: "d", verbose: "desc", grp: Grp::Rev },
    Hint { key: "a", verbose: "accept", grp: Grp::Train },
    Hint { key: "A", verbose: "abort", grp: Grp::Train },
    Hint { key: "x", verbose: "rm", grp: Grp::Del },
    Hint { key: "X", verbose: "drop", grp: Grp::Del },
    Hint { key: "q", verbose: "quit", grp: Grp::App },
    Hint { key: "?", verbose: "keys", grp: Grp::App },
    Hint { key: "!", verbose: "log", grp: Grp::App },
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
    has_prompt: bool,
    train: bool,
}
```

- [ ] **Step 4: Build the key row**

Replace the placeholder `render_keys` from Task 3 with:

```rust
    fn caps(&self) -> Caps {
        let t = self.selected_task();
        Caps {
            has_ws: t.as_ref().is_some_and(|t| t.ws_path.is_some()),
            has_pane: t.as_ref().is_some_and(|t| t.pane_id.is_some()),
            has_prompt: t.as_ref().is_some_and(|t| !t.prompt.is_empty()),
            train: !self.merge_train.is_empty(),
        }
    }

    /// Whether `key` is usable on the selected row. See [`Caps`] on why this is an
    /// approximation.
    fn hint_available(key: &str, c: &Caps) -> bool {
        match key {
            "↵" => c.has_pane,
            "s" | "S" => c.has_ws,
            "r" | "R" | "d" | "a" => c.has_ws && c.has_pane && c.has_prompt,
            "A" => c.train,
            _ => true,
        }
    }

    /// Build the key row. `verbose` picks `n new  N handoff …`; otherwise the compact
    /// `new nN │ open ↵ │ …`. Both come from [`HINTS`], so their widths are derived and
    /// the two forms cannot drift apart.
    fn keys_line(&self, verbose: bool) -> Line<'static> {
        let caps = self.caps();
        let selected_pane = self.selected_task().and_then(|t| t.pane_id);
        let detached = self.open_pane.is_some() && self.open_pane == selected_pane;
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut cur: Option<Grp> = None;
        for h in HINTS {
            let label = if h.key == "↵" && detached {
                "detach"
            } else {
                h.verbose
            };
            if cur != Some(h.grp) {
                if cur.is_some() {
                    spans.push(Span::styled(
                        " │ ",
                        Style::default().fg(Color::DarkGray),
                    ));
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
            // Unavailable keys stay in place and dim, so nothing ever shifts sideways.
            let mut style = Style::default().fg(h.grp.color());
            if !Self::hint_available(h.key, &caps) {
                style = style.add_modifier(Modifier::DIM);
            }
            if verbose {
                spans.push(Span::styled(
                    format!("{} ", h.key),
                    style.add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(label.to_string(), style));
            } else {
                spans.push(Span::styled(h.key.to_string(), style));
            }
        }
        Line::from(spans)
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
```

Add `truncate_line` as a free function next to `truncate_first_line`'s import site — put it just below the `Caps` struct:

```rust
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
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test`
Expected: PASS.

Eyeball the two forms and confirm the widths match the spec (128 verbose, 56 compact):

Run: `cargo test --lib compact_key_row_fits_a_docked_pane -- --nocapture`
Expected: PASS.


---

### Task 5: The `!` log and `?` keymap overlays

**Files:**
- Modify: `src/tui/input.rs` (two new actions and mappings), `src/tui/mod.rs` (`Overlay` enum, `App.overlay` field, `handle_key`, `render`, `keys_line` badge, two render functions)
- Test: inline tests in both files

**Interfaces:**
- Consumes: `HINTS`, `keys_line` from Task 4; `Notices::{iter, unseen, max_unseen_level, mark_seen}` from Task 1.
- Produces: `Action::{ShowLog, Help}`, `App.overlay: Overlay`, `fn render_overlay(&self, f: &mut Frame, area: Rect)`.

- [ ] **Step 1: Write the failing tests**

In `src/tui/input.rs`, extend `maps_core_keys`:

```rust
        assert_eq!(map_key(k(KeyCode::Char('!'))), Action::ShowLog);
        assert_eq!(map_key(k(KeyCode::Char('?'))), Action::Help);
```

In `src/tui/mod.rs`:

```rust
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
        assert!(!app.should_quit, "q closed the overlay rather than quitting");
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
        assert_eq!(app.pending_swap, Some(t.id));
        app.handle_key(key(KeyCode::Char('!')));
        assert_eq!(app.overlay, Overlay::None, "the log did not open");
        assert_eq!(app.pending_swap, None, "the confirmation was cancelled");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test`
Expected: FAIL — `no variant ShowLog found for enum Action`, `cannot find type Overlay in this scope`, `no field overlay on type App`.

- [ ] **Step 3: Add the input actions**

In `src/tui/input.rs`, add to `enum Action` before `None`:

```rust
    /// `!`: open the notice log — scrollback of everything faff has reported, including
    /// the merge-train notices that fire with no keypress behind them.
    ShowLog,
    /// `?`: open the keymap overlay — every binding with its full description, including
    /// `j`/`k`, which the toolbar has no room for.
    Help,
```

And to `map_key`:

```rust
        KeyCode::Char('!') => Action::ShowLog,
        KeyCode::Char('?') => Action::Help,
```

- [ ] **Step 4: Add the overlay state and input handling**

In `src/tui/mod.rs`, above `struct App` (line ~120):

```rust
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
```

Add the field to `struct App`, after `notices`:

```rust
    /// The open overlay, if any. See [`Overlay`].
    overlay: Overlay,
```

Initialise it in both `App::new` and `test_app` with `overlay: Overlay::None,`.

At the very top of `handle_key`, before the `pending_swap` block:

```rust
        // An open overlay is modal: the next key dismisses it and nothing else. Safe to
        // check before the `pending_*` blocks because the two cannot coexist — while a
        // confirmation is armed, `!`/`?` are swallowed as "any other key cancels".
        if self.overlay != Overlay::None {
            self.overlay = Overlay::None;
            return;
        }
```

And in the `match action` block, next to the other actions:

```rust
            Action::ShowLog => {
                self.overlay = Overlay::Log;
                self.notices.mark_seen();
            }
            Action::Help => self.overlay = Overlay::Help,
```

- [ ] **Step 5: Render the overlays**

In `render`, after `self.render_body(f, chunks[1]);`:

```rust
        if self.overlay != Overlay::None {
            self.render_overlay(f, chunks[1]);
        }
```

Add `Clear` to the widget imports at line ~22:

```rust
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
```

Add the render function next to `render_keys`:

```rust
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
        f.render_widget(Paragraph::new(lines).block(block), area);
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

    /// Every binding, including the ones the toolbar has no room for.
    fn help_lines(&self) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from(vec![
            Span::styled(" ↑/↓ j/k ", Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(
                "move the selection",
                Style::default().fg(Color::DarkGray),
            ),
        ])];
        for h in HINTS {
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" {:>7} ", h.key),
                    Style::default()
                        .fg(h.grp.color())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(h.verbose.to_string(), Style::default().fg(h.grp.color())),
            ]));
        }
        lines
    }
```

- [ ] **Step 6: Add the unseen badge to the key row**

In `keys_line` from Task 4, hoist the unseen count above the loop, next to `let mut cur: Option<Grp> = None;`:

```rust
        // The `!` key doubles as the unseen-notice badge, coloured by the worst unseen
        // level so a red `!3` is visible without opening anything.
        let unseen = self.notices.unseen();
        let badge = self.notices.max_unseen_level().map(|l| l.color());
```

Then replace everything in the loop body from `let label = …` down to the closing brace of the `if verbose { … } else { … }` block with:

```rust
            let label = if h.key == "↵" && detached {
                "detach"
            } else {
                h.verbose
            };
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
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test`
Expected: PASS, all tests in `input` and `tui`.


---

### Task 6: Document the new UI

**Files:**
- Modify: `README.md` — insert a subsection after "The revision view" (which ends at line ~296, just before "## How state moves")

**Interfaces:**
- Consumes: everything above. Produces no code.

- [ ] **Step 1: Write the section**

Insert before the `## How state moves` heading in `README.md`:

````markdown
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

The toolbar groups keys by what they do, and colours each group: **new** (`n` `N`),
**open** (`↵`), **rev** (`s` `S` `r` `R` `d`), **train** (`a` `A`), **del** (`x` `X`),
then `q` `?` `!`. Keys that don't apply to the selected row are dimmed rather than
hidden, so nothing ever moves. When faff is docked beside a session and the pane is too
narrow for the full labels, the toolbar drops to its compact form — same groups, same
order, just the letters:

```
 new nN │ open ↵ │ rev sSrRd │ train aA │ del xX │ q ? !
```
````

- [ ] **Step 2: Verify the documented keys match the code**

Run: `grep -o "KeyCode::Char('.')" src/tui/input.rs | sort -u`
Expected: every listed key appears in the README section above (plus `j`/`k`, documented in the `?` paragraph).

- [ ] **Step 3: Run the full suite one more time**

Run: `cargo test`
Expected: PASS.

Run: `cargo clippy --all-targets`
Expected: no new warnings introduced by this work.


---

## Verification

After Task 6, confirm the original complaint is fixed end to end:

```bash
cargo test notice_is_visible_in_a_narrow_docked_pane -- --exact --nocapture
cargo test --lib tui
cargo test --lib
```

Then run faff inside WezTerm, dock a session with `↵` so faff is narrowed, and press `S`
on an agent. `✓ snapshotted #<id>` must be legible on the notice line. Press `!` and
confirm the log lists it with a timestamp.
