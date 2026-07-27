//! Workspace manager: the verified fork recipe, memory seeding, hook injection,
//! and teardown. See spec §5, §9. All jj shelling goes through `crate::jj`.

use crate::{config, jj};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

/// The result of forking a task workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceInfo {
    pub name: String,
    pub path: PathBuf,
    pub change_id: String,
    pub fork_point: String,
}

/// Fork + materialise a task workspace, forking from the nearest revision with real
/// content (spec §5).
///
/// Fork point = `heads(::@ ~ empty())` — the nearest ancestor of `@` (including `@`
/// itself) that is non-empty. Then:
/// - if `@` *is* that commit (you have uncommitted content), `jj new` freezes it and
///   advances master, and the task forks from the frozen commit;
/// - if `@` is empty (nothing edited since the last fork), we fork straight from the
///   existing content commit and **do not** advance master — so repeatedly creating
///   tasks without editing doesn't stack up empty fork-points that clutter the log.
///
/// Either way the fork point is non-empty and already has children, so it's frozen
/// (staleness-safe). The caller supplies `name`/`path`.
pub fn create(repo: &Path, name: &str, path: &Path) -> Result<WorkspaceInfo> {
    let at = jj::resolve_change_id(repo, "@").context("resolving @")?;
    let fork_point =
        jj::resolve_change_id(repo, "heads(::@ ~ empty())").unwrap_or_else(|_| at.clone());

    // Only advance master when @ itself carries the content we're forking from.
    if fork_point == at {
        jj::new(repo).context("jj new (advancing master)")?;
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating workspace root {}", parent.display()))?;
    }
    jj::workspace_add(repo, name, &fork_point, path).context("jj workspace add")?;

    let change_id = jj::workspace_list(repo)?
        .into_iter()
        .find(|w| w.name == name)
        .map(|w| w.change_id)
        .with_context(|| format!("workspace {name} missing after add"))?;

    Ok(WorkspaceInfo {
        name: name.to_string(),
        path: path.to_path_buf(),
        change_id,
        fork_point,
    })
}

/// Convenience: compute the standard name + path (via `config`) and `create`.
pub fn create_for_task(repo: &Path, task_id: i64, slug: &str) -> Result<WorkspaceInfo> {
    let name = format!("faf-task-{task_id}");
    let path = config::task_workspace_dir(repo, task_id, slug)?;
    create(repo, &name, &path)
}

/// Retire a task workspace: abandon the task's *un-integrated* commits, forget the
/// workspace, and remove its directory.
///
/// The abandon set is `(fork_point..head) ~ ::@`: the task's own commits, MINUS
/// anything that is now an ancestor of master's `@`. This is the crucial safety
/// property — if you integrated the task (e.g. rebased master onto its commit),
/// that commit is in `::@` and is preserved; abandoning it would rewrite master and
/// undo your integration. If nothing was integrated (a discard), the whole branch is
/// abandoned so no orphaned heads are left. Done *before* forgetting, while the
/// workspace still exists.
pub fn teardown(repo: &Path, ws_name: &str, ws_path: &Path, fork_point: &str) -> Result<()> {
    if let Some(head) = jj::workspace_list(repo)?
        .into_iter()
        .find(|w| w.name == ws_name)
        .map(|w| w.change_id)
        && head != fork_point
    {
        // Best-effort: empty result (fully integrated) / gone revisions are fine.
        let _ = jj::abandon(repo, &format!("({fork_point}..{head}) ~ ::@"));
    }
    jj::workspace_forget(repo, ws_name).context("jj workspace forget")?;
    if ws_path.exists() {
        fs::remove_dir_all(ws_path)
            .with_context(|| format!("removing workspace dir {}", ws_path.display()))?;
    }
    Ok(())
}

/// Pre-trust `ws_path` in `~/.claude.json` so the spawned agent skips the
/// "Do you trust the files in this folder?" dialog. Trust is keyed per exact path
/// (not inherited from a parent dir), so every workspace needs its own entry.
///
/// Best-effort atomic merge: read the JSON, set `projects[ws].hasTrustDialogAccepted`,
/// write to a temp file and rename over. faf writes this once at task creation,
/// before that agent is spawned; a rare race with another claude updating the shared
/// file at the same instant would at worst drop this entry (the dialog reappears once).
pub fn trust_workspace(ws_path: &Path) -> Result<()> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("no HOME for ~/.claude.json")?;
    trust_workspace_in(&home.join(".claude.json"), ws_path)
}

/// The trust-merge against an explicit config file (testable without touching $HOME).
fn trust_workspace_in(cfg: &Path, ws_path: &Path) -> Result<()> {
    let mut root: Value = if cfg.exists() {
        serde_json::from_str(&fs::read_to_string(cfg)?).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };
    if !root.is_object() {
        root = json!({});
    }
    let obj = root.as_object_mut().unwrap();
    let projects = obj.entry("projects").or_insert_with(|| json!({}));
    if !projects.is_object() {
        *projects = json!({});
    }
    let key = ws_path.to_string_lossy().to_string();
    let entry = projects
        .as_object_mut()
        .unwrap()
        .entry(key)
        .or_insert_with(|| json!({}));
    if !entry.is_object() {
        *entry = json!({});
    }
    entry
        .as_object_mut()
        .unwrap()
        .insert("hasTrustDialogAccepted".to_string(), json!(true));

    let tmp = cfg.with_extension("json.faf-tmp");
    fs::write(&tmp, serde_json::to_string(&root)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, cfg).with_context(|| format!("replacing {}", cfg.display()))?;
    Ok(())
}

/// Default location of Claude's per-project data: `$CLAUDE_CONFIG_DIR|$HOME/.claude` + `/projects`.
pub fn claude_projects_dir() -> PathBuf {
    let base = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default();
            home.join(".claude")
        });
    base.join("projects")
}

/// Copy master's memory snapshot into the new workspace's project key (spec §8).
/// Best-effort: returns Ok(false) if master has no memory yet. `master_cwd` is the
/// directory Claude runs in for master (assumed to be the repo root).
pub fn seed_memory(claude_projects: &Path, master_cwd: &Path, ws_path: &Path) -> Result<bool> {
    let src_key = config::encode_repo_path(master_cwd);
    let dst_key = config::encode_repo_path(ws_path);
    let src_mem = claude_projects.join(&src_key).join("memory");
    if !src_mem.is_dir() {
        return Ok(false);
    }
    let dst_mem = claude_projects.join(&dst_key).join("memory");
    copy_dir_all(&src_mem, &dst_mem)?;

    // Copy the MEMORY.md index if it sits at the project-key root.
    let src_index = claude_projects.join(&src_key).join("MEMORY.md");
    if src_index.is_file() {
        let dst_index = claude_projects.join(&dst_key).join("MEMORY.md");
        if let Some(p) = dst_index.parent() {
            fs::create_dir_all(p)?;
        }
        fs::copy(&src_index, &dst_index)?;
    }
    Ok(true)
}

fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &to)?;
        } else {
            fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

/// Write the auto-injected Claude Code hooks into `<ws>/.claude/settings.local.json`.
/// Each hook invokes the faf binary's `report-event`, which persists to `db` and
/// nudges the TUI on `socket`.
pub fn write_hooks(
    ws_path: &Path,
    task_id: i64,
    faf_exe: &Path,
    socket: &Path,
    db: &Path,
) -> Result<PathBuf> {
    let dir = ws_path.join(".claude");
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("settings.local.json");
    let settings = hook_settings(task_id, faf_exe, socket, db);
    fs::write(&path, serde_json::to_string_pretty(&settings)?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Single-quote a path for safe embedding in a shell command string (Claude Code
/// runs command hooks via a shell), escaping any embedded single quotes.
fn shell_quote(p: &Path) -> String {
    format!("'{}'", p.to_string_lossy().replace('\'', "'\\''"))
}

fn hook_cmd(task_id: i64, faf_exe: &Path, event: &str, socket: &Path, db: &Path) -> String {
    format!(
        "{} report-event --task {} --event {} --socket {} --db {}",
        shell_quote(faf_exe),
        task_id,
        event,
        shell_quote(socket),
        shell_quote(db),
    )
}

fn hook_settings(task_id: i64, faf_exe: &Path, socket: &Path, db: &Path) -> Value {
    let group = |event: &str| json!([{ "hooks": [{ "type": "command", "command": hook_cmd(task_id, faf_exe, event, socket, db) }] }]);
    let matched = |event: &str| json!([{ "matcher": "*", "hooks": [{ "type": "command", "command": hook_cmd(task_id, faf_exe, event, socket, db) }] }]);
    json!({
        "hooks": {
            "Stop": group("stop"),
            "Notification": group("notification"),
            "UserPromptSubmit": group("prompt"),
            "SessionStart": group("session-start"),
            "PostToolUse": matched("post-tool"),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn write_hooks_produces_expected_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let faf = Path::new("/opt/my apps/faf"); // note the space
        let sock = Path::new("/run/faf/7.sock");
        let db = Path::new("/data/faf/repo/faf.db");
        let path = write_hooks(ws, 7, faf, sock, db).unwrap();
        assert!(path.ends_with(".claude/settings.local.json"));

        let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let stop = v["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        // faf_exe path contains a space, so it must be shell-quoted
        assert!(stop.contains("'/opt/my apps/faf' report-event"));
        assert!(stop.contains("--task 7"));
        assert!(stop.contains("--event stop"));
        assert!(stop.contains("--socket '/run/faf/7.sock'"));
        assert!(stop.contains("--db '/data/faf/repo/faf.db'"));
        // PostToolUse is matcher-based
        assert_eq!(v["hooks"]["PostToolUse"][0]["matcher"], "*");
        assert!(
            v["hooks"]["PostToolUse"][0]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .contains("--event post-tool")
        );
        // all five events present
        for e in [
            "Stop",
            "Notification",
            "UserPromptSubmit",
            "SessionStart",
            "PostToolUse",
        ] {
            assert!(v["hooks"].get(e).is_some(), "missing hook {e}");
        }
    }

    #[test]
    fn seed_memory_copies_snapshot_and_index() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let master = Path::new("/home/jezza/work/repo");
        let ws = Path::new("/data/faf/-home-jezza-work-repo/ws/0001-x");

        let src_key = config::encode_repo_path(master);
        fs::create_dir_all(projects.join(&src_key).join("memory")).unwrap();
        fs::write(projects.join(&src_key).join("memory").join("a.md"), "mem a").unwrap();
        fs::write(projects.join(&src_key).join("MEMORY.md"), "index").unwrap();

        let copied = seed_memory(&projects, master, ws).unwrap();
        assert!(copied);

        let dst_key = config::encode_repo_path(ws);
        assert_eq!(
            fs::read_to_string(projects.join(&dst_key).join("memory").join("a.md")).unwrap(),
            "mem a"
        );
        assert_eq!(
            fs::read_to_string(projects.join(&dst_key).join("MEMORY.md")).unwrap(),
            "index"
        );
    }

    #[test]
    fn trust_workspace_sets_flag_and_preserves_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join(".claude.json");
        // Pre-existing config with another project + a top-level key that must survive.
        fs::write(
            &cfg,
            r#"{"model":"opus","projects":{"/other":{"hasTrustDialogAccepted":true,"lastCost":0.5}}}"#,
        )
        .unwrap();

        trust_workspace_in(&cfg, Path::new("/ws/0001-x")).unwrap();

        let v: Value = serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
        // new workspace trusted
        assert_eq!(
            v["projects"]["/ws/0001-x"]["hasTrustDialogAccepted"],
            json!(true)
        );
        // existing data preserved
        assert_eq!(v["model"], json!("opus"));
        assert_eq!(
            v["projects"]["/other"]["hasTrustDialogAccepted"],
            json!(true)
        );
        assert_eq!(v["projects"]["/other"]["lastCost"], json!(0.5));
    }

    #[test]
    fn trust_workspace_creates_config_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join(".claude.json");
        trust_workspace_in(&cfg, Path::new("/ws/0002-y")).unwrap();
        let v: Value = serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            v["projects"]["/ws/0002-y"]["hasTrustDialogAccepted"],
            json!(true)
        );
    }

    #[test]
    fn seed_memory_is_noop_without_source() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let copied =
            seed_memory(&projects, Path::new("/no/such/repo"), Path::new("/no/ws")).unwrap();
        assert!(!copied);
    }

    // --- Integration: create + teardown against a scratch repo ---

    fn jj(repo: &Path, cfg: &Path, args: &[&str]) {
        let ok = Command::new("jj")
            .arg("-R")
            .arg(repo)
            .arg("--no-pager")
            .args(args)
            .env("JJ_CONFIG", cfg)
            .status()
            .unwrap()
            .success();
        assert!(ok, "jj {args:?} failed");
    }

    // Run jj from inside a workspace dir (operates on that workspace's @).
    fn jj_in(dir: &Path, cfg: &Path, args: &[&str]) {
        let ok = Command::new("jj")
            .arg("--no-pager")
            .args(args)
            .current_dir(dir)
            .env("JJ_CONFIG", cfg)
            .status()
            .unwrap()
            .success();
        assert!(ok, "jj (in {}) {args:?} failed", dir.display());
    }

    // init + configure + one base commit; returns (repo, cfg).
    fn scratch_repo(tmp: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let repo = tmp.join("repo");
        fs::create_dir_all(&repo).unwrap();
        let cfg = tmp.join("jjcfg.toml");
        fs::write(&cfg, "[user]\nname=\"Test\"\nemail=\"t@x.io\"\n").unwrap();
        let init = Command::new("jj")
            .args(["git", "init"])
            .arg(&repo)
            .env("JJ_CONFIG", &cfg)
            .status()
            .unwrap();
        assert!(init.success());
        jj(
            &repo,
            &cfg,
            &["config", "set", "--repo", "user.name", "Test"],
        );
        jj(
            &repo,
            &cfg,
            &["config", "set", "--repo", "user.email", "t@x.io"],
        );
        fs::write(repo.join("base.txt"), "base").unwrap();
        jj(&repo, &cfg, &["commit", "-m", "base"]);
        (repo, cfg)
    }

    #[test]
    fn integration_empty_at_reuses_fork_point() {
        // Two tasks created back-to-back with no edits between must fork from the
        // same content commit — no extra empty fork-points, master doesn't advance.
        let tmp = tempfile::tempdir().unwrap();
        let (repo, _cfg) = scratch_repo(tmp.path()); // leaves @ empty on top of "base"
        let master_before = jj::resolve_change_id(&repo, "@").unwrap();

        let a = create(&repo, "faf-task-1", &tmp.path().join("ws/1")).unwrap();
        let b = create(&repo, "faf-task-2", &tmp.path().join("ws/2")).unwrap();

        assert_eq!(
            a.fork_point, b.fork_point,
            "both tasks fork from the same content commit"
        );
        assert_ne!(a.change_id, b.change_id, "but they are distinct workspaces");
        assert_eq!(
            jj::resolve_change_id(&repo, "@").unwrap(),
            master_before,
            "master @ did not advance (no new empty fork-point)"
        );
    }

    #[test]
    fn integration_teardown_abandons_whole_task_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, cfg) = scratch_repo(tmp.path());

        let ws_path = tmp.path().join("ws").join("0002");
        let info = create(&repo, "faf-task-2", &ws_path).unwrap();

        // Simulate the agent making two real commits on the task branch.
        fs::write(ws_path.join("a.txt"), "a").unwrap();
        jj_in(&ws_path, &cfg, &["commit", "-m", "WORKA"]);
        fs::write(ws_path.join("b.txt"), "b").unwrap();
        jj_in(&ws_path, &cfg, &["commit", "-m", "WORKB"]);

        let before = jj::log(&repo, "all()").unwrap();
        assert!(before.iter().any(|r| r.description == "WORKA"));
        assert!(before.iter().any(|r| r.description == "WORKB"));

        teardown(&repo, &info.name, &ws_path, &info.fork_point).unwrap();

        // Both task commits are gone (no orphaned heads), master's base survives.
        let after = jj::log(&repo, "all()").unwrap();
        assert!(
            !after.iter().any(|r| r.description == "WORKA"),
            "WORKA must be abandoned, not orphaned"
        );
        assert!(
            !after.iter().any(|r| r.description == "WORKB"),
            "WORKB must be abandoned, not orphaned"
        );
        assert!(after.iter().any(|r| r.description == "base"));
        assert!(!ws_path.exists());
    }

    #[test]
    fn integration_teardown_preserves_integrated_commit() {
        // The user's scenario: integrate the agent's commit into master, then remove
        // the task. teardown must NOT abandon the integrated commit.
        let tmp = tempfile::tempdir().unwrap();
        let (repo, cfg) = scratch_repo(tmp.path());
        let ws_path = tmp.path().join("ws").join("0003");
        let info = create(&repo, "faf-task-3", &ws_path).unwrap();

        fs::write(ws_path.join("w.txt"), "w").unwrap();
        jj_in(&ws_path, &cfg, &["commit", "-m", "INTEGRATED_WORK"]);

        // Integrate: master's @ is placed on top of the task's work commit (the
        // committed work is the parent of the task workspace's now-empty head).
        let head = jj::workspace_list(&repo)
            .unwrap()
            .into_iter()
            .find(|w| w.name == "faf-task-3")
            .unwrap()
            .change_id;
        let work = jj::resolve_change_id(&repo, &format!("{head}-")).unwrap();
        jj(&repo, &cfg, &["new", &work]);

        teardown(&repo, &info.name, &ws_path, &info.fork_point).unwrap();

        let after = jj::log(&repo, "all()").unwrap();
        assert!(
            after.iter().any(|r| r.description == "INTEGRATED_WORK"),
            "integrated commit must survive teardown (not be abandoned)"
        );
        assert!(!ws_path.exists());
    }

    #[test]
    fn integration_create_inherits_wip_then_teardown_removes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let cfg = tmp.path().join("jjcfg.toml");
        fs::write(&cfg, "[user]\nname=\"Test\"\nemail=\"t@x.io\"\n").unwrap();

        // Configure repo-level user so library jj calls (which don't set JJ_CONFIG) work.
        let init = Command::new("jj")
            .args(["git", "init"])
            .arg(&repo)
            .env("JJ_CONFIG", &cfg)
            .status()
            .unwrap();
        assert!(init.success());
        jj(
            &repo,
            &cfg,
            &["config", "set", "--repo", "user.name", "Test"],
        );
        jj(
            &repo,
            &cfg,
            &["config", "set", "--repo", "user.email", "t@x.io"],
        );
        fs::write(repo.join("base.txt"), "base").unwrap();
        jj(&repo, &cfg, &["commit", "-m", "base"]);
        fs::write(repo.join("wip.txt"), "work in progress").unwrap();

        // Explicit workspace path keeps the test hermetic (no data-dir/env hacks).
        let ws_path = tmp.path().join("workspaces").join("0001-add-auth");
        let info = create(&repo, "faf-task-1", &ws_path).unwrap();
        assert_eq!(info.name, "faf-task-1");
        assert!(info.path.exists(), "workspace dir should exist");
        assert!(
            info.path.join("wip.txt").exists(),
            "task must inherit master's WIP"
        );
        assert_ne!(info.change_id, info.fork_point);

        let ws = jj::workspace_list(&repo).unwrap();
        assert!(ws.iter().any(|w| w.name == "faf-task-1"));

        teardown(&repo, &info.name, &info.path, &info.fork_point).unwrap();
        assert!(!info.path.exists(), "workspace dir removed");
        let ws2 = jj::workspace_list(&repo).unwrap();
        assert!(!ws2.iter().any(|w| w.name == "faf-task-1"));
    }
}
