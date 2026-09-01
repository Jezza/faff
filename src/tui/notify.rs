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
        assert_eq!(
            n.unseen(),
            CAP,
            "badge cannot claim more notes than are kept"
        );
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
