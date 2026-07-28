//! Workspace manager: the verified fork recipe, memory seeding, hook injection,
//! and teardown. See spec §5, §9. All jj shelling goes through `crate::jj`.

use crate::{config, jj};
use anyhow::{Context, Result, bail};
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
///   advances HEAD, and the task forks from the frozen commit;
/// - if `@` is empty (nothing edited since the last fork), we fork straight from the
///   existing content commit and **do not** advance HEAD — so repeatedly creating
///   tasks without editing doesn't stack up empty fork-points that clutter the log.
///
/// Either way the fork point is non-empty and already has children, so it's frozen
/// (staleness-safe). The caller supplies `name`/`path`.
pub fn create(repo: &Path, name: &str, path: &Path) -> Result<WorkspaceInfo> {
    let at = jj::resolve_change_id(repo, "@").context("resolving @")?;
    let fork_point =
        jj::resolve_change_id(repo, "heads(::@ ~ empty())").unwrap_or_else(|_| at.clone());

    // Only advance HEAD when @ itself carries the content we're forking from.
    if fork_point == at {
        jj::new(repo).context("jj new (advancing HEAD)")?;
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

/// Retire a task workspace: forget it, delete its directory, and abandon its commits
/// **only if they are all empty**.
///
/// The task's own commits are `(fork_point..head) ~ ::@` — its branch, minus anything
/// already integrated into HEAD's `@` (which must never be rewritten). Of that set:
/// - if any commit carries real content, faf leaves the whole branch alone as ordinary
///   history — you integrate or `jj abandon` it yourself. faf never discards real work
///   on removal;
/// - if they are all empty (a bare fork, an empty tip — graph noise), they're abandoned
///   so no empty heads linger.
///
/// This also makes `swap` safe to undo by removal: after a swap the agent workspace
/// holds your old (non-empty) line, so removing that task keeps your work. The abandon
/// runs *before* forgetting, while the workspace still exists.
pub fn teardown(repo: &Path, ws_name: &str, ws_path: &Path, fork_point: &str) -> Result<()> {
    if let Some(head) = jj::workspace_list(repo)?
        .into_iter()
        .find(|w| w.name == ws_name)
        .map(|w| w.change_id)
        && head != fork_point
    {
        let own = format!("({fork_point}..{head}) ~ ::@");
        // `({own}) ~ empty()` is the task's own commits with the empty ones removed —
        // i.e. its real work. If any exists, preserve the branch (skip the abandon).
        // On a query error, default to preserving: never risk discarding real work.
        let has_content = jj::any_revision(repo, &format!("({own}) ~ empty()")).unwrap_or(true);
        if !has_content {
            // Best-effort: an empty result (fully integrated) / gone revisions are fine.
            let _ = jj::abandon(repo, &own);
        }
    }
    jj::workspace_forget(repo, ws_name).context("jj workspace forget")?;
    if ws_path.exists() {
        fs::remove_dir_all(ws_path)
            .with_context(|| format!("removing workspace dir {}", ws_path.display()))?;
    }
    Ok(())
}

/// Swap the default workspace's `@` with a task workspace's revision — a literal trade
/// of checkouts. Your repo ends up on the agent's work; the agent's workspace ends up
/// on your old line (so its next work is based on your current line, not an ever-staler
/// fork). Returns the change_id the agent workspace now sits on (your old `@`).
///
/// jj has no atomic two-workspace swap, so this is a snapshot then two `jj edit`s, in
/// an order chosen around one jj rule: an empty, description-less commit is auto-
/// abandoned the moment its *last* workspace leaves it.
/// 1. Snapshot both workspaces, so nothing uncommitted is lost and each commit holds
///    its latest content before the trade (the agent may never have run jj itself).
/// 2. Bail if the agent's revision is empty — there's nothing to adopt, and moving onto
///    it would let jj abandon it mid-trade.
/// 3. Move the *agent* onto your old line first, while the default workspace still holds
///    it. That keeps your `@` doubly-referenced, so it survives even when it's empty —
///    the common case, where you dispatched an agent and never touched your own tree.
/// 4. Move the default workspace onto the agent's revision. Your old `@` survives (the
///    agent now holds it); the agent's revision is now held by both.
///
/// If step 4 fails the swap is left half-applied — recoverable, since "both workspaces
/// on the agent's rev" is exactly the combined HEAD+agent node faf already renders
/// (re-run swap or fix by hand). Bails untouched if `@` is already the agent's revision.
pub fn swap(repo: &Path, ws_name: &str, ws_path: &Path) -> Result<String> {
    let user_head = jj::resolve_change_id(repo, "@").context("resolving @")?;
    let agent_head = jj::workspace_list(repo)?
        .into_iter()
        .find(|w| w.name == ws_name)
        .map(|w| w.change_id)
        .with_context(|| format!("workspace {ws_name} not found"))?;
    if user_head == agent_head {
        bail!("@ is already on {ws_name}'s revision — nothing to swap");
    }
    // 1. Capture both workspaces' uncommitted edits (change_ids are unchanged by this).
    jj::snapshot_in(ws_path).context("snapshotting the agent workspace")?;
    jj::snapshot_in(repo).context("snapshotting your workspace")?;
    // 2. Nothing to adopt from an empty agent revision.
    if !jj::any_revision(repo, &format!("{agent_head} ~ empty()"))? {
        bail!("{ws_name}'s revision is empty — nothing to adopt");
    }
    // 3. Agent onto your old line first (keeps an empty `@` alive — see above).
    jj::edit_in(ws_path, &user_head).context("moving the agent onto your old line")?;
    // 4. Default workspace onto the agent's revision.
    jj::edit(repo, &agent_head).context("moving @ onto the agent's revision")?;
    Ok(user_head)
}

/// Snapshot a task workspace's working copy into its `@` (see `jj::snapshot_in`), so an
/// agent that hasn't run a jj command has its edits reflected in the revision graph.
pub fn snapshot(ws_path: &Path) -> Result<()> {
    jj::snapshot_in(ws_path)
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

/// Copy HEAD's memory snapshot into the new workspace's project key (spec §8).
/// Best-effort: returns Ok(false) if HEAD has no memory yet. `head_cwd` is the
/// directory Claude runs in for HEAD (assumed to be the repo root).
pub fn seed_memory(claude_projects: &Path, head_cwd: &Path, ws_path: &Path) -> Result<bool> {
    let src_key = config::encode_repo_path(head_cwd);
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
        let head = Path::new("/home/jezza/work/repo");
        let ws = Path::new("/data/faf/-home-jezza-work-repo/ws/0001-x");

        let src_key = config::encode_repo_path(head);
        fs::create_dir_all(projects.join(&src_key).join("memory")).unwrap();
        fs::write(projects.join(&src_key).join("memory").join("a.md"), "mem a").unwrap();
        fs::write(projects.join(&src_key).join("MEMORY.md"), "index").unwrap();

        let copied = seed_memory(&projects, head, ws).unwrap();
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
        // same content commit — no extra empty fork-points, HEAD doesn't advance.
        let tmp = tempfile::tempdir().unwrap();
        let (repo, _cfg) = scratch_repo(tmp.path()); // leaves @ empty on top of "base"
        let head_before = jj::resolve_change_id(&repo, "@").unwrap();

        let a = create(&repo, "faf-task-1", &tmp.path().join("ws/1")).unwrap();
        let b = create(&repo, "faf-task-2", &tmp.path().join("ws/2")).unwrap();

        assert_eq!(
            a.fork_point, b.fork_point,
            "both tasks fork from the same content commit"
        );
        assert_ne!(a.change_id, b.change_id, "but they are distinct workspaces");
        assert_eq!(
            jj::resolve_change_id(&repo, "@").unwrap(),
            head_before,
            "HEAD @ did not advance (no new empty fork-point)"
        );
    }

    #[test]
    fn integration_teardown_keeps_nonempty_work() {
        // faf never discards real work on removal: a task branch with content is left
        // as ordinary history (workspace forgotten, dir gone, commits preserved).
        let tmp = tempfile::tempdir().unwrap();
        let (repo, cfg) = scratch_repo(tmp.path());

        let ws_path = tmp.path().join("ws").join("0002");
        let info = create(&repo, "faf-task-2", &ws_path).unwrap();

        // Simulate the agent making two real commits on the task branch.
        fs::write(ws_path.join("a.txt"), "a").unwrap();
        jj_in(&ws_path, &cfg, &["commit", "-m", "WORKA"]);
        fs::write(ws_path.join("b.txt"), "b").unwrap();
        jj_in(&ws_path, &cfg, &["commit", "-m", "WORKB"]);

        teardown(&repo, &info.name, &ws_path, &info.fork_point).unwrap();

        // Both task commits SURVIVE (the branch is real work, not noise); the workspace
        // is forgotten and its directory removed.
        let after = jj::log(&repo, "all()").unwrap();
        assert!(
            after.iter().any(|r| r.description == "WORKA"),
            "WORKA must be kept, not abandoned"
        );
        assert!(
            after.iter().any(|r| r.description == "WORKB"),
            "WORKB must be kept, not abandoned"
        );
        assert!(!ws_path.exists());
        assert!(
            !jj::workspace_list(&repo)
                .unwrap()
                .iter()
                .any(|w| w.name == "faf-task-2")
        );
    }

    #[test]
    fn integration_teardown_abandons_empty_branch() {
        // A task that produced no content (just the empty fork tip) is pure graph noise;
        // teardown abandons it so no empty head lingers.
        let tmp = tempfile::tempdir().unwrap();
        let (repo, _cfg) = scratch_repo(tmp.path());

        let ws_path = tmp.path().join("ws").join("0009");
        let info = create(&repo, "faf-task-9", &ws_path).unwrap();
        // The workspace's @ is an empty commit on the fork point.
        assert_ne!(info.change_id, info.fork_point);

        teardown(&repo, &info.name, &ws_path, &info.fork_point).unwrap();

        // The empty tip is gone (abandoned), the workspace forgotten, the dir removed.
        let after = jj::log(&repo, "all()").unwrap();
        assert!(
            !after.iter().any(|r| r.change_id == info.change_id),
            "empty task tip must be abandoned"
        );
        assert!(!ws_path.exists());
        assert!(
            !jj::workspace_list(&repo)
                .unwrap()
                .iter()
                .any(|w| w.name == "faf-task-9")
        );
    }

    #[test]
    fn integration_swap_trades_checkouts() {
        // Trade the default workspace's @ with the agent's revision. The agent here has
        // NOT snapshotted its edits — swap must capture them first (step 1), so the
        // agent's work materialises in the default repo after the trade.
        let tmp = tempfile::tempdir().unwrap();
        let (repo, _cfg) = scratch_repo(tmp.path());

        // Give the default workspace some WIP, then fork a task off it.
        fs::write(repo.join("user.txt"), "mine").unwrap();
        let ws_path = tmp.path().join("ws").join("0004");
        let info = create(&repo, "faf-task-4", &ws_path).unwrap();

        // The agent edits a file but never runs jj (no snapshot of its own).
        fs::write(ws_path.join("agent.txt"), "theirs").unwrap();

        let user_before = jj::resolve_change_id(&repo, "@").unwrap();
        let agent_before = info.change_id.clone();

        let returned = swap(&repo, &info.name, &ws_path).unwrap();

        // The default workspace now sits on the agent's revision; the agent workspace on
        // your old line. swap returns the agent workspace's new head (your old @).
        assert_eq!(returned, user_before, "swap returns your old @");
        assert_eq!(
            jj::resolve_change_id(&repo, "@").unwrap(),
            agent_before,
            "default @ moved onto the agent's revision"
        );
        let agent_head_now = jj::workspace_list(&repo)
            .unwrap()
            .into_iter()
            .find(|w| w.name == "faf-task-4")
            .unwrap()
            .change_id;
        assert_eq!(agent_head_now, user_before, "agent moved onto your old line");

        // Files followed the checkouts: the agent's captured work is now in the default
        // repo (proving the pre-swap snapshot), and gone from the agent workspace.
        assert!(
            repo.join("agent.txt").exists(),
            "agent's work materialised in the default repo"
        );
        assert!(repo.join("user.txt").exists(), "your work is still present");
        assert!(
            !ws_path.join("agent.txt").exists(),
            "agent workspace no longer holds the agent's work"
        );
        assert!(ws_path.join("user.txt").exists());
    }

    #[test]
    fn integration_swap_bails_when_already_parked() {
        // If @ already sits on the agent's revision, there is nothing to trade.
        let tmp = tempfile::tempdir().unwrap();
        let (repo, _cfg) = scratch_repo(tmp.path());
        fs::write(repo.join("user.txt"), "mine").unwrap();
        let ws_path = tmp.path().join("ws").join("0005");
        let info = create(&repo, "faf-task-5", &ws_path).unwrap();

        // Park the default workspace onto the agent's revision (as `jj edit` would).
        jj::edit(&repo, &info.change_id).unwrap();

        let err = swap(&repo, &info.name, &ws_path).unwrap_err();
        assert!(
            err.to_string().contains("nothing to swap"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn integration_snapshot_captures_agent_edits() {
        // An agent that never runs jj leaves its @ empty; `snapshot` folds its edits in.
        let tmp = tempfile::tempdir().unwrap();
        let (repo, _cfg) = scratch_repo(tmp.path());
        let ws_path = tmp.path().join("ws").join("0006");
        let info = create(&repo, "faf-task-6", &ws_path).unwrap();
        let head = info.change_id.clone();

        let empty = |cid: &str| {
            jj::log(&repo, cid)
                .unwrap()
                .into_iter()
                .find(|r| r.change_id == cid)
                .unwrap()
                .empty
        };
        assert!(empty(&head), "the agent tip starts empty");

        fs::write(ws_path.join("new.txt"), "captured").unwrap();
        snapshot(&ws_path).unwrap();

        assert!(!empty(&head), "snapshot must fold the agent's edit into its @");
    }

    #[test]
    fn integration_teardown_preserves_integrated_commit() {
        // The user's scenario: integrate the agent's commit into HEAD, then remove
        // the task. teardown must NOT abandon the integrated commit.
        let tmp = tempfile::tempdir().unwrap();
        let (repo, cfg) = scratch_repo(tmp.path());
        let ws_path = tmp.path().join("ws").join("0003");
        let info = create(&repo, "faf-task-3", &ws_path).unwrap();

        fs::write(ws_path.join("w.txt"), "w").unwrap();
        jj_in(&ws_path, &cfg, &["commit", "-m", "INTEGRATED_WORK"]);

        // Integrate: HEAD's @ is placed on top of the task's work commit (the
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
            "task must inherit HEAD's WIP"
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
