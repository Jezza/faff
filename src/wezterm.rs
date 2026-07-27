//! WezTerm controller: `wezterm cli` argv construction + exec + `list` parsing.
//! Verified primitives (spec §8): split-pane --move-pane-id (open a session beside
//! faf) and move-pane-to-new-tab (detach). Builders are separated from exec so they
//! can be unit-tested without mutating any live panes.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::Path;
use std::process::Command;

/// One pane as reported by `wezterm cli list --format json` (unknown fields ignored).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Pane {
    pub window_id: u64,
    pub tab_id: u64,
    pub pane_id: u64,
    #[serde(default)]
    pub workspace: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub cwd: String,
}

// ---- argv builders (pure) ----------------------------------------------------

fn s(v: impl ToString) -> String {
    v.to_string()
}

/// `wezterm cli spawn --cwd <cwd> -- <prog...>` (prints the new pane-id).
pub fn spawn_args(cwd: &Path, prog: &[&str]) -> Vec<String> {
    let mut a = vec![
        s("cli"),
        s("spawn"),
        s("--cwd"),
        cwd.to_string_lossy().into_owned(),
        s("--"),
    ];
    a.extend(prog.iter().map(|p| p.to_string()));
    a
}

/// `wezterm cli list --format json`.
pub fn list_args() -> Vec<String> {
    vec![s("cli"), s("list"), s("--format"), s("json")]
}

/// `wezterm cli activate-pane --pane-id <id>`.
pub fn activate_pane_args(pane_id: u64) -> Vec<String> {
    vec![s("cli"), s("activate-pane"), s("--pane-id"), s(pane_id)]
}

/// Open a session beside faf: move the agent pane into a right split of faf's pane.
/// `wezterm cli split-pane --right --move-pane-id <agent> --pane-id <faf>`.
pub fn split_move_args(faf_pane: u64, agent_pane: u64) -> Vec<String> {
    vec![
        s("cli"),
        s("split-pane"),
        s("--right"),
        s("--move-pane-id"),
        s(agent_pane),
        s("--pane-id"),
        s(faf_pane),
    ]
}

/// Detach a session: `wezterm cli move-pane-to-new-tab --pane-id <agent>`.
pub fn move_to_new_tab_args(agent_pane: u64) -> Vec<String> {
    vec![
        s("cli"),
        s("move-pane-to-new-tab"),
        s("--pane-id"),
        s(agent_pane),
    ]
}

/// `wezterm cli set-tab-title --pane-id <id> <title>`.
pub fn set_tab_title_args(pane_id: u64, title: &str) -> Vec<String> {
    vec![
        s("cli"),
        s("set-tab-title"),
        s("--pane-id"),
        s(pane_id),
        title.to_string(),
    ]
}

/// `wezterm cli get-text --pane-id <id>`.
pub fn get_text_args(pane_id: u64) -> Vec<String> {
    vec![s("cli"), s("get-text"), s("--pane-id"), s(pane_id)]
}

/// `wezterm cli kill-pane --pane-id <id>`.
pub fn kill_pane_args(pane_id: u64) -> Vec<String> {
    vec![s("cli"), s("kill-pane"), s("--pane-id"), s(pane_id)]
}

// ---- exec wrappers -----------------------------------------------------------

fn run(args: &[String]) -> Result<String> {
    let out = Command::new("wezterm")
        .args(args)
        .output()
        .context("spawning wezterm (is it installed and is a mux running?)")?;
    if !out.status.success() {
        bail!(
            "wezterm {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Parse `wezterm cli list --format json` output.
pub fn parse_list(json: &str) -> Result<Vec<Pane>> {
    serde_json::from_str(json).context("parsing wezterm list json")
}

/// List all panes in the mux.
pub fn list() -> Result<Vec<Pane>> {
    parse_list(&run(&list_args())?)
}

/// Spawn `prog` in a new tab with the given cwd; returns the new pane-id.
pub fn spawn(cwd: &Path, prog: &[&str]) -> Result<u64> {
    let out = run(&spawn_args(cwd, prog))?;
    out.trim()
        .parse::<u64>()
        .with_context(|| format!("parsing spawned pane-id from {out:?}"))
}

pub fn activate_pane(pane_id: u64) -> Result<()> {
    run(&activate_pane_args(pane_id)).map(|_| ())
}

/// Move `agent_pane` into a right split of `faf_pane` (open session mode).
pub fn open_beside(faf_pane: u64, agent_pane: u64) -> Result<()> {
    run(&split_move_args(faf_pane, agent_pane)).map(|_| ())
}

/// Eject `agent_pane` back into its own tab, still running (detach).
pub fn detach(agent_pane: u64) -> Result<()> {
    run(&move_to_new_tab_args(agent_pane)).map(|_| ())
}

pub fn set_tab_title(pane_id: u64, title: &str) -> Result<()> {
    run(&set_tab_title_args(pane_id, title)).map(|_| ())
}

pub fn get_text(pane_id: u64) -> Result<String> {
    run(&get_text_args(pane_id))
}

pub fn kill_pane(pane_id: u64) -> Result<()> {
    run(&kill_pane_args(pane_id)).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv_builders_are_exact() {
        assert_eq!(
            spawn_args(Path::new("/ws/0007"), &["claude", "do X"]),
            vec!["cli", "spawn", "--cwd", "/ws/0007", "--", "claude", "do X"]
        );
        assert_eq!(list_args(), vec!["cli", "list", "--format", "json"]);
        assert_eq!(
            activate_pane_args(12),
            vec!["cli", "activate-pane", "--pane-id", "12"]
        );
        assert_eq!(
            split_move_args(3, 12),
            vec![
                "cli",
                "split-pane",
                "--right",
                "--move-pane-id",
                "12",
                "--pane-id",
                "3"
            ]
        );
        assert_eq!(
            move_to_new_tab_args(12),
            vec!["cli", "move-pane-to-new-tab", "--pane-id", "12"]
        );
        assert_eq!(
            set_tab_title_args(12, "#7 add-auth"),
            vec!["cli", "set-tab-title", "--pane-id", "12", "#7 add-auth"]
        );
        assert_eq!(
            get_text_args(12),
            vec!["cli", "get-text", "--pane-id", "12"]
        );
        assert_eq!(
            kill_pane_args(12),
            vec!["cli", "kill-pane", "--pane-id", "12"]
        );
    }

    #[test]
    fn parses_list_json_ignoring_extra_fields() {
        // Shape captured from a real `wezterm cli list --format json`.
        let json = r##"[
          {"window_id":6,"tab_id":6,"pane_id":36,"workspace":"default",
           "size":{"rows":30,"cols":94,"pixel_width":940,"pixel_height":570,"dpi":96},
           "title":"faf","cwd":"file://host/home/jezza/work/faf"},
          {"window_id":6,"tab_id":7,"pane_id":40,"workspace":"default",
           "size":{"rows":30,"cols":94,"pixel_width":940,"pixel_height":570,"dpi":96},
           "title":"#7 add-auth","cwd":"file://host/ws/0007"}
        ]"##;
        let panes = parse_list(json).unwrap();
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].pane_id, 36);
        assert_eq!(panes[0].title, "faf");
        assert_eq!(panes[1].pane_id, 40);
        assert_eq!(panes[1].tab_id, 7);
        assert_eq!(panes[1].title, "#7 add-auth");
        assert!(panes[1].cwd.contains("/ws/0007"));
    }
}
