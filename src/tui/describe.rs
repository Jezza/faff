//! The log view's headless `d`: ask a one-shot `claude -p` for a short description of a
//! revision's diff. Mirrors the `desc` script — same prompt, same clean-up of the reply.

use crate::proc;
use anyhow::{Result, bail};
use std::process::Command;
use std::time::Duration;

/// Cap on the diff sent to claude, in bytes, to keep the request fast and cheap.
const MAX_DIFF: usize = 100_000;

/// How long a describe may take before it's abandoned as an error.
const TIMEOUT: Duration = Duration::from_secs(120);

/// The model asked to describe — `desc`'s default.
const MODEL: &str = "opus";

/// The instruction and the (capped) diff, as one prompt.
pub fn prompt(diff: &str) -> String {
    let diff = if diff.len() > MAX_DIFF {
        let mut end = MAX_DIFF;
        while !diff.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}\n[diff truncated]", &diff[..end])
    } else {
        diff.to_string()
    };
    format!(
        "Write a change description for the diff below. Respond with a single line of 4 to 6 \
         words in the imperative mood (e.g. \"Add retry logic to client\"). Output ONLY that \
         line: no quotes, no trailing period, no preamble, no markdown.\n\n<diff>\n{diff}\n</diff>"
    )
}

/// The description in claude's reply: its first non-blank line, without the list numbering,
/// bullets, wrapping quotes, or trailing period a model sometimes adds anyway.
pub fn clean(reply: &str) -> Option<String> {
    let line = reply.lines().map(str::trim).find(|l| !l.is_empty())?;
    let digits = line.trim_start_matches(|c: char| c.is_ascii_digit());
    let line = match digits.strip_prefix(['.', ')', ':']) {
        Some(rest) if digits.len() < line.len() => rest.trim_start(),
        _ => line,
    };
    let line = line.strip_prefix(['-', '*']).map_or(line, str::trim_start);
    let line = line.strip_prefix(['"', '\'', '`']).unwrap_or(line);
    let line = line.strip_suffix(['"', '\'', '`']).unwrap_or(line);
    let line = line.trim_end_matches(|c: char| c == '.' || c.is_whitespace());
    (!line.is_empty()).then(|| line.to_string())
}

/// Ask a one-shot `claude -p` to describe `diff`, returning the cleaned description. The
/// prompt goes in on stdin: a diff can outgrow the kernel's limit on one argument. Blocks
/// for as long as claude thinks, so run it off the engine thread.
pub fn ask_claude(diff: &str) -> Result<String> {
    let mut cmd = Command::new("claude");
    cmd.args(["-p", "--model", MODEL]);
    let out = proc::output_with_input(&mut cmd, prompt(diff).as_bytes(), TIMEOUT)
        .map_err(|e| proc::failure("claude", "is Claude Code installed?", e))?;
    if !out.status.success() {
        bail!(
            "claude failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    match clean(&String::from_utf8_lossy(&out.stdout)) {
        Some(d) => Ok(d),
        None => bail!("claude returned no usable description"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_strips_list_marks_quotes_and_trailing_period() {
        assert_eq!(
            clean("Add retry logic to client").as_deref(),
            Some("Add retry logic to client")
        );
        assert_eq!(
            clean("  1. Add retry logic.  ").as_deref(),
            Some("Add retry logic")
        );
        assert_eq!(
            clean("2) Add retry logic").as_deref(),
            Some("Add retry logic")
        );
        assert_eq!(
            clean("- Add retry logic").as_deref(),
            Some("Add retry logic")
        );
        assert_eq!(
            clean("* Add retry logic").as_deref(),
            Some("Add retry logic")
        );
        assert_eq!(
            clean("\"Add retry logic\"").as_deref(),
            Some("Add retry logic")
        );
        assert_eq!(
            clean("`Add retry logic`").as_deref(),
            Some("Add retry logic")
        );
    }

    #[test]
    fn clean_takes_the_first_non_blank_line() {
        assert_eq!(
            clean("\n\n  Fix the parser\nsecond").as_deref(),
            Some("Fix the parser")
        );
    }

    #[test]
    fn clean_rejects_a_reply_with_nothing_usable() {
        assert_eq!(clean(""), None);
        assert_eq!(clean("  \n \n"), None);
        assert_eq!(clean("\"\""), None);
    }

    #[test]
    fn prompt_wraps_the_diff_and_caps_its_size() {
        let p = prompt("diff --git a/x b/x");
        assert!(p.contains("<diff>\ndiff --git a/x b/x\n</diff>"), "{p}");
        let huge = "é".repeat(MAX_DIFF);
        let p = prompt(&huge);
        assert!(p.contains("[diff truncated]"));
        assert!(
            p.len() < MAX_DIFF + 1000,
            "capped in bytes, on a char boundary"
        );
    }
}
