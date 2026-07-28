//! One-time seeding of a task change's jj description via a one-shot
//! `claude -p --model haiku`. See spec §11.
//!
//! When a task captures its first prompt, we derive a short title from it and set it as
//! the change's jj `description` — but only if the change has none yet, so we never
//! clobber a description the agent or the user set. The log view reads the live
//! description, so once seeded (or once the agent describes its own work) the row
//! updates on its own. Non-blocking: runs on a background thread and nudges a refresh.

use crate::domain::{LABEL_WIDTH, TaskId, truncate_first_line};
use crate::jj;
use anyhow::Result;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::Sender;

/// Fallback label used until (or instead of) a generated title.
pub fn fallback_title(prompt: &str) -> String {
    truncate_first_line(prompt, LABEL_WIDTH)
}

/// The instruction we hand the model.
fn title_instruction(prompt: &str) -> String {
    format!(
        "Give a concise 3-6 word title (no quotes, no trailing punctuation) for this task:\n\n{prompt}"
    )
}

/// Clean a model's raw output into a title: first line, stripped quotes, truncated.
fn sanitize(raw: &str) -> String {
    let line = raw
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let line = line
        .trim_matches(|c| c == '"' || c == '\'' || c == '`')
        .trim();
    let line = line.trim_end_matches(['.', ':']);
    truncate_first_line(line, LABEL_WIDTH)
}

/// Derive a title using an injected runner (the runner executes claude and returns
/// stdout). Falls back to the truncated prompt on error or empty output.
pub fn derive_title<F>(prompt: &str, runner: F) -> String
where
    F: FnOnce(&str) -> Result<String>,
{
    match runner(&title_instruction(prompt)) {
        Ok(out) => {
            let t = sanitize(&out);
            if t.is_empty() {
                fallback_title(prompt)
            } else {
                t
            }
        }
        Err(_) => fallback_title(prompt),
    }
}

/// The real runner: `claude -p --model haiku <instruction>`.
pub fn claude_runner(instruction: &str) -> Result<String> {
    let out = Command::new("claude")
        .args(["-p", "--model", "haiku"])
        .arg(instruction)
        .output()?;
    if !out.status.success() {
        anyhow::bail!("claude title gen failed");
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Spawn a background thread that seeds the description of the change `revset` resolves
/// to (typically `"<workspace>@"`). Best-effort and non-clobbering: skips if the change
/// already has a description, or if reading/writing jj fails. On a successful write,
/// sends `id` back so the TUI refreshes and shows the new description.
pub fn spawn_seed_job(
    id: TaskId,
    repo: PathBuf,
    revset: String,
    prompt: String,
    tx: Sender<TaskId>,
) {
    std::thread::spawn(move || {
        // Never overwrite a description the agent or user already set.
        match jj::description(&repo, &revset) {
            Ok(d) if d.trim().is_empty() => {}
            _ => return,
        }
        let desc = derive_title(&prompt, claude_runner);
        if desc.is_empty() {
            return;
        }
        if jj::describe(&repo, &revset, &desc).is_ok() {
            let _ = tx.send(id);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_is_truncated_first_line() {
        assert_eq!(fallback_title("Fix the parser\nand more"), "Fix the parser");
    }

    #[test]
    fn derive_uses_runner_output() {
        let t = derive_title("implement oauth", |_| Ok("Add OAuth Login".to_string()));
        assert_eq!(t, "Add OAuth Login");
    }

    #[test]
    fn derive_strips_quotes_and_punctuation() {
        let t = derive_title("x", |_| Ok("  \"Fix flaky tests.\"  \n".to_string()));
        assert_eq!(t, "Fix flaky tests");
    }

    #[test]
    fn derive_falls_back_on_error() {
        let t = derive_title("Refactor the store layer now", |_| anyhow::bail!("boom"));
        assert_eq!(t, "Refactor the store layer now");
    }

    #[test]
    fn derive_falls_back_on_empty() {
        let t = derive_title("Refactor the store layer", |_| Ok("   \n".to_string()));
        assert_eq!(t, "Refactor the store layer");
    }

    #[test]
    fn instruction_includes_prompt() {
        assert!(title_instruction("do the thing").contains("do the thing"));
    }
}
