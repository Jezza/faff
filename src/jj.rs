//! jj adapter: structured extraction via `jj log`/`jj workspace list` templates.
//! No `jj-lib`, no consuming jj's rendered graph — we parse `\x1f`-delimited records
//! (verified against jj 0.43). See spec §3 and the fork recipe in §5.

use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Command;

/// Field separator emitted by our templates (ASCII Unit Separator, 0x1f).
const US: char = '\u{1f}';

/// One-line-per-commit record: change_id, parents(comma), current_wc, empty,
/// conflict, id_prefix (shortest unique), id_rest (padding to 8), description.
const LOG_TEMPLATE: &str = r#"change_id ++ "\x1f" ++ parents.map(|c| c.change_id()).join(",") ++ "\x1f" ++ if(current_working_copy,"1","0") ++ "\x1f" ++ if(empty,"1","0") ++ "\x1f" ++ if(conflict,"1","0") ++ "\x1f" ++ change_id.shortest(8).prefix() ++ "\x1f" ++ change_id.shortest(8).rest() ++ "\x1f" ++ description.first_line() ++ "\n""#;

/// name<US>change_id per workspace.
const WS_TEMPLATE: &str = r#"name ++ "\x1f" ++ target.change_id() ++ "\n""#;

/// Just the change_id, one per matched revision.
const CHANGE_ID_TEMPLATE: &str = r#"change_id ++ "\n""#;

/// Structured info about one revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevInfo {
    pub change_id: String,
    pub parents: Vec<String>,
    pub is_current_wc: bool,
    pub empty: bool,
    /// The revision has unresolved conflicts (jj's `conflict` keyword).
    pub conflict: bool,
    /// Shortest change-id prefix unique across the repo (jj's own disambiguation).
    pub id_prefix: String,
    /// The remaining chars padding the id to 8 (jj's `shortest(8).rest()`).
    pub id_rest: String,
    pub description: String,
}

/// (workspace name, change_id of its working-copy commit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub name: String,
    pub change_id: String,
}

/// Run `jj -R <repo> --no-pager <args>` and return stdout, erroring with stderr.
fn run_jj(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("jj")
        .arg("-R")
        .arg(repo)
        .arg("--no-pager")
        .args(args)
        .output()
        .context("spawning jj (is it installed and on PATH?)")?;
    if !out.status.success() {
        bail!(
            "jj {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run `jj --no-pager <args>` with the working directory set to `dir` and **no** `-R`
/// flag. jj infers the repo + workspace from `dir`, so this acts on whatever workspace
/// `dir` belongs to. This is the only way to operate on a non-default workspace: `-R`
/// (as `run_jj` passes it) always pins the `default` workspace and ignores the cwd.
fn run_jj_in(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("jj")
        .arg("--no-pager")
        .args(args)
        .current_dir(dir)
        .output()
        .context("spawning jj (is it installed and on PATH?)")?;
    if !out.status.success() {
        bail!(
            "jj {:?} (in {}) failed: {}",
            args,
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Parse the log template output into RevInfo records.
fn parse_log(stdout: &str) -> Vec<RevInfo> {
    stdout
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|line| {
            let mut f = line.split(US);
            let change_id = f.next()?.to_string();
            let parents_raw = f.next().unwrap_or("");
            let current = f.next().unwrap_or("0");
            let empty = f.next().unwrap_or("0");
            let conflict = f.next().unwrap_or("0");
            let id_prefix = f.next().unwrap_or("").to_string();
            let id_rest = f.next().unwrap_or("").to_string();
            // description is the remainder (rejoin in the unlikely event it held a US)
            let description = f.collect::<Vec<_>>().join(&US.to_string());
            if change_id.is_empty() {
                return None;
            }
            Some(RevInfo {
                change_id,
                parents: parents_raw
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect(),
                is_current_wc: current == "1",
                empty: empty == "1",
                conflict: conflict == "1",
                id_prefix,
                id_rest,
                description,
            })
        })
        .collect()
}

/// Revisions matching `revset`, in jj's default (child→parent) order.
pub fn log(repo: &Path, revset: &str) -> Result<Vec<RevInfo>> {
    let out = run_jj(
        repo,
        &["log", "--no-graph", "-r", revset, "-T", LOG_TEMPLATE],
    )?;
    Ok(parse_log(&out))
}

/// The revset for the revision graph: `ancestors(<each workspace's @> | @, limit)`.
///
/// Each workspace is named by its `<name>@` working-copy reference, **not** by its
/// change_id. A change_id that has gone *divergent* (two visible commits share it) is
/// ambiguous as a bare revset symbol, so `jj log` errors out ("Change ID … is divergent")
/// and takes the whole graph down with it — every task then falls into the detached list.
/// `<name>@` names the one commit each workspace actually sits on, so a divergent agent
/// still resolves to a single node. `@` (your own working copy) is included unconditionally.
pub fn workspace_ancestors_revset(workspaces: &[Workspace], limit: usize) -> String {
    let mut heads: Vec<String> = workspaces.iter().map(|w| format!("{}@", w.name)).collect();
    heads.push("@".to_string());
    format!("ancestors({}, {})", heads.join(" | "), limit)
}

/// All workspaces attached to the repo and the change_id each is checked out at.
pub fn workspace_list(repo: &Path) -> Result<Vec<Workspace>> {
    let out = run_jj(repo, &["workspace", "list", "-T", WS_TEMPLATE])?;
    Ok(out
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|line| {
            let (name, cid) = line.split_once(US)?;
            Some(Workspace {
                name: name.to_string(),
                change_id: cid.to_string(),
            })
        })
        .collect())
}

/// The revset for faff's fork point: the newest ancestor of `head` (inclusive) that
/// carries real content — `heads(::<head> ~ (empty() ~ merges()))`. See spec §5.
///
/// The carve-out is the whole point. jj's `empty()` means "modifies no files *relative to
/// its parents*", which includes a merge whose parents were combined cleanly with no extra
/// edits on top — the usual result of `jj new A B` or a conflict-free rebase merge. Such a
/// commit is `empty()` yet is emphatically not empty *content*: it is the only revision in
/// the graph that holds both sides. So we subtract only `empty() ~ merges()` — empty
/// non-merge commits, the bare fork-points and fresh working copies that are genuine noise.
///
/// A plain `~ empty()` doesn't just mislabel such a merge, it breaks the recipe: dropping
/// the merge leaves *both* its parents as maximal elements, so `heads()` returns two
/// revisions and [`resolve_change_id`] silently keeps whichever jj lists first. New agents
/// would then fork from one side of the merge with the other side's work missing from their
/// base. Keeping the merge in the set makes it the single head, as it should be.
pub fn fork_point_revset(head: &str) -> String {
    format!("heads(::{head} ~ (empty() ~ merges()))")
}

/// Resolve a revset expected to identify a single revision into its change_id.
pub fn resolve_change_id(repo: &Path, revset: &str) -> Result<String> {
    let out = run_jj(
        repo,
        &["log", "--no-graph", "-r", revset, "-T", CHANGE_ID_TEMPLATE],
    )?;
    out.lines()
        .find(|l| !l.trim().is_empty())
        .map(|s| s.trim().to_string())
        .with_context(|| format!("revset {revset:?} matched no revision"))
}

/// `jj new` in the given repo's default workspace (advances @, freezing the old one).
pub fn new(repo: &Path) -> Result<()> {
    run_jj(repo, &["new"])?;
    Ok(())
}

/// `jj new <rev>` in the default workspace: create a fresh empty commit as a child of
/// `rev` and check it out. Unlike [`new`] (a child of the current `@`), this bases the
/// new working copy on an arbitrary revision — `handoff` uses it to retreat your `@` onto
/// the fork point from before your changes. `-R` pins the default workspace (see `edit`).
pub fn new_at(repo: &Path, rev: &str) -> Result<()> {
    run_jj(repo, &["new", rev])?;
    Ok(())
}

/// `jj workspace add --name <name> -r <revset> <path>`.
pub fn workspace_add(repo: &Path, name: &str, revset: &str, path: &Path) -> Result<()> {
    let path_s = path.to_string_lossy();
    run_jj(
        repo,
        &["workspace", "add", "--name", name, "-r", revset, &path_s],
    )?;
    Ok(())
}

/// `jj workspace forget <name>` (stops tracking the workspace's working-copy commit).
pub fn workspace_forget(repo: &Path, name: &str) -> Result<()> {
    run_jj(repo, &["workspace", "forget", name])?;
    Ok(())
}

/// `jj abandon -r <revset>`.
pub fn abandon(repo: &Path, revset: &str) -> Result<()> {
    run_jj(repo, &["abandon", "-r", revset])?;
    Ok(())
}

/// `jj -R <repo> edit <rev>` — repoint the **default** workspace's `@` at an existing
/// revision. `-R` pins the default workspace regardless of cwd, so this only ever moves
/// HEAD's own working copy.
pub fn edit(repo: &Path, rev: &str) -> Result<()> {
    run_jj(repo, &["edit", rev])?;
    Ok(())
}

/// `jj edit <rev>` run inside a workspace directory (no `-R`): move that workspace's
/// `@` onto `rev`. The only way to repoint a non-default (agent) workspace.
pub fn edit_in(ws_dir: &Path, rev: &str) -> Result<()> {
    run_jj_in(ws_dir, &["edit", rev])?;
    Ok(())
}

/// `jj util snapshot` inside a workspace dir (no `-R`): fold that workspace's
/// working-copy changes into its `@`, with no other effect. Lets faff capture edits from
/// an agent that never ran a jj command itself (so nothing snapshotted it).
pub fn snapshot_in(ws_dir: &Path) -> Result<()> {
    run_jj_in(ws_dir, &["util", "snapshot"])?;
    Ok(())
}

/// Whether `revset` matches at least one revision. Lets callers branch on set
/// (non-)emptiness without parsing counts.
pub fn any_revision(repo: &Path, revset: &str) -> Result<bool> {
    let out = run_jj(
        repo,
        &["log", "--no-graph", "-r", revset, "-T", CHANGE_ID_TEMPLATE],
    )?;
    Ok(out.lines().any(|l| !l.trim().is_empty()))
}

/// First line of the description of the revision `revset` resolves to (empty string
/// when it has none, or when nothing matches).
pub fn description(repo: &Path, revset: &str) -> Result<String> {
    let out = run_jj(
        repo,
        &[
            "log",
            "--no-graph",
            "-r",
            revset,
            "-T",
            r#"description.first_line() ++ "\n""#,
        ],
    )?;
    Ok(out.lines().next().unwrap_or("").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    // Fixture in the LOG_TEMPLATE shape (8 US-separated fields; US shown as \u{1f}):
    // change_id, parents, current_wc, empty, conflict, id_prefix, id_rest, description.
    const FIXTURE: &str = "rwmqzmnkwwnknszkrypzzoxyklmqzyol\u{1f}nlqmxnsrrzpswxrqwlrlstszrrstqpkq\u{1f}0\u{1f}1\u{1f}0\u{1f}rw\u{1f}mqzmnk\u{1f}\n\
        zylsskwvzvzyunuryqzqxstpnlmupyqx\u{1f}nlqmxnsrrzpswxrqwlrlstszrrstqpkq\u{1f}1\u{1f}1\u{1f}0\u{1f}zy\u{1f}lsskwv\u{1f}\n\
        nlqmxnsrrzpswxrqwlrlstszrrstqpkq\u{1f}vttuzqwuxunwuvsqytuwnlqpxxskoooy\u{1f}0\u{1f}0\u{1f}0\u{1f}n\u{1f}lqmxnsr\u{1f}\n\
        vttuzqwuxunwuvsqytuwnlqpxxskoooy\u{1f}zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz\u{1f}0\u{1f}0\u{1f}1\u{1f}v\u{1f}ttuzqwu\u{1f}base commit\n\
        zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz\u{1f}\u{1f}0\u{1f}1\u{1f}0\u{1f}zzzzzzzz\u{1f}\u{1f}\n";

    #[test]
    fn parses_log_records() {
        let revs = parse_log(FIXTURE);
        assert_eq!(revs.len(), 5);

        // task workspace @ (not current wc, empty, one parent = fork-point)
        assert_eq!(revs[0].change_id, "rwmqzmnkwwnknszkrypzzoxyklmqzyol");
        assert_eq!(revs[0].parents, vec!["nlqmxnsrrzpswxrqwlrlstszrrstqpkq"]);
        assert!(!revs[0].is_current_wc);
        assert!(revs[0].empty);
        assert!(!revs[0].conflict);
        assert_eq!(revs[0].id_prefix, "rw");
        assert_eq!(revs[0].id_rest, "mqzmnk");
        assert_eq!(revs[0].description, "");

        // HEAD @ is the current working copy
        assert!(revs[1].is_current_wc);

        // the base commit carries its description and is flagged conflicted
        assert_eq!(revs[3].description, "base commit");
        assert!(!revs[3].empty);
        assert!(revs[3].conflict);

        // root has no parents
        assert_eq!(revs[4].parents, Vec::<String>::new());

        // HEAD @ and task @ share the same parent (the fork-point)
        assert_eq!(revs[0].parents, revs[1].parents);
    }

    #[test]
    fn workspace_ancestors_revset_names_workspaces_by_at_ref() {
        // Heads are `<name>@` refs, never change_ids — so a divergent change can't make the
        // revset ambiguous. `@` (your own working copy) is always included.
        let ws = vec![
            Workspace {
                name: "default".into(),
                change_id: "aaaaaaaa".into(),
            },
            Workspace {
                name: "faf-task-1".into(),
                change_id: "bbbbbbbb".into(),
            },
        ];
        let r = workspace_ancestors_revset(&ws, 25);
        assert_eq!(r, "ancestors(default@ | faf-task-1@ | @, 25)");
        assert!(
            !r.contains("aaaaaaaa") && !r.contains("bbbbbbbb"),
            "must reference workspaces by name, not by (divergence-prone) change_id"
        );
    }

    // --- Integration: run real jj against a scratch repo (jj must be installed) ---

    fn jj_cfg(dir: &Path) -> std::path::PathBuf {
        let cfg = dir.join("jjcfg.toml");
        std::fs::write(&cfg, "[user]\nname = \"Test\"\nemail = \"test@x.io\"\n").unwrap();
        cfg
    }

    fn jj_setup(repo: &Path, cfg: &Path, args: &[&str]) {
        let status = Command::new("jj")
            .arg("-R")
            .arg(repo)
            .arg("--no-pager")
            .args(args)
            .env("JJ_CONFIG", cfg)
            .status()
            .unwrap();
        assert!(status.success(), "jj {args:?} failed");
    }

    #[test]
    fn integration_extracts_dag_and_workspaces() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let cfg = jj_cfg(tmp.path());

        // init (no -R: that targets an existing repo) + base commit + WIP,
        // then the fork recipe (jj new; workspace add -r @-).
        let init = Command::new("jj")
            .args(["git", "init"])
            .arg(&repo)
            .env("JJ_CONFIG", &cfg)
            .status()
            .unwrap();
        assert!(init.success(), "jj git init failed");
        std::fs::write(repo.join("base.txt"), "base").unwrap();
        jj_setup(&repo, &cfg, &["commit", "-m", "base commit"]);
        std::fs::write(repo.join("wip.txt"), "wip").unwrap();
        jj_setup(&repo, &cfg, &["new"]);
        let ws = tmp.path().join("task-ws");
        jj_setup(
            &repo,
            &cfg,
            &[
                "workspace",
                "add",
                "--name",
                "faf-task-1",
                "-r",
                "@-",
                ws.to_str().unwrap(),
            ],
        );

        // workspace_list maps names -> change_ids
        let workspaces = workspace_list(&repo).unwrap();
        let names: Vec<_> = workspaces.iter().map(|w| w.name.as_str()).collect();
        assert!(names.contains(&"default"));
        assert!(names.contains(&"faf-task-1"));

        // resolve @ == the default workspace's change_id
        let head_at = resolve_change_id(&repo, "@").unwrap();
        let default_ws = workspaces.iter().find(|w| w.name == "default").unwrap();
        assert_eq!(head_at, default_ws.change_id);

        // log(all()) contains both working copies; HEAD @ and task @ share a parent
        let revs = log(&repo, "all()").unwrap();
        let head = revs.iter().find(|r| r.is_current_wc).expect("HEAD @");
        let task_ws = workspaces.iter().find(|w| w.name == "faf-task-1").unwrap();
        let task = revs
            .iter()
            .find(|r| r.change_id == task_ws.change_id)
            .expect("task @ present in log");
        assert_eq!(
            head.parents, task.parents,
            "HEAD and task must branch from the same frozen fork-point"
        );
        assert_eq!(head.parents.len(), 1);
    }

    #[test]
    fn integration_any_revision_matches_and_misses() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let cfg = jj_cfg(tmp.path());
        let init = Command::new("jj")
            .args(["git", "init"])
            .arg(&repo)
            .env("JJ_CONFIG", &cfg)
            .status()
            .unwrap();
        assert!(init.success());
        std::fs::write(repo.join("base.txt"), "base").unwrap();
        jj_setup(&repo, &cfg, &["commit", "-m", "base"]); // leaves @ empty on top of base

        // A matching revset is truthy; an empty one is falsy.
        assert!(any_revision(&repo, "all()").unwrap());
        assert!(any_revision(&repo, "@").unwrap());
        assert!(!any_revision(&repo, "none()").unwrap());
        // The fresh @ is empty, so its non-empty subset matches nothing.
        assert!(!any_revision(&repo, "@ ~ empty()").unwrap());
    }

    #[test]
    fn integration_describe_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let cfg = jj_cfg(tmp.path());

        let init = Command::new("jj")
            .args(["git", "init"])
            .arg(&repo)
            .env("JJ_CONFIG", &cfg)
            .status()
            .unwrap();
        assert!(init.success(), "jj git init failed");
        // Persist the committer identity in the repo config so the production `run_jj`
        // (which doesn't set JJ_CONFIG) can write a commit inside the sandbox.
        jj_setup(&repo, &cfg, &["config", "set", "--repo", "user.name", "Test"]);
        jj_setup(
            &repo,
            &cfg,
            &["config", "set", "--repo", "user.email", "test@x.io"],
        );

        // A fresh working copy has no description.
        assert_eq!(description(&repo, "@").unwrap(), "");
        // Setting a description makes `description` read back its first line.
        jj_setup(&repo, &cfg, &["describe", "-r", "@", "-m", "a described change"]);
        assert_eq!(description(&repo, "@").unwrap(), "a described change");
    }

    #[test]
    fn integration_divergent_workspace_still_logs() {
        // Regression: when an agent's workspace change goes *divergent* (two visible commits
        // share its change_id), a revset that names the workspace by its bare change_id makes
        // `jj log` error ("Change ID … is divergent"). That error blanked faff's whole graph
        // and dropped every task into the detached list. `workspace_ancestors_revset` names
        // each workspace by `<name>@`, which resolves to the single commit it sits on.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let cfg = jj_cfg(tmp.path());

        let init = Command::new("jj")
            .args(["git", "init"])
            .arg(&repo)
            .env("JJ_CONFIG", &cfg)
            .status()
            .unwrap();
        assert!(init.success(), "jj git init failed");
        // Identity in the repo config so production `run_jj` (no JJ_CONFIG) can rewrite commits.
        jj_setup(&repo, &cfg, &["config", "set", "--repo", "user.name", "Test"]);
        jj_setup(
            &repo,
            &cfg,
            &["config", "set", "--repo", "user.email", "test@x.io"],
        );

        // A base commit, then a task workspace forked off it (faff's fork recipe).
        std::fs::write(repo.join("base.txt"), "base").unwrap();
        jj_setup(&repo, &cfg, &["commit", "-m", "base"]);
        let ws = tmp.path().join("task-ws");
        jj_setup(
            &repo,
            &cfg,
            &[
                "workspace",
                "add",
                "--name",
                "faf-task-1",
                "-r",
                "@-",
                ws.to_str().unwrap(),
            ],
        );

        // Force the task workspace's change divergent: two concurrent describes of it. The
        // second, run `--at-operation @-`, doesn't see the first, so the change ends up with
        // two visible commits (jj resolves the concurrent ops on the next command).
        let task_cid = resolve_change_id(&repo, "faf-task-1@").unwrap();
        jj_setup(&repo, &cfg, &["describe", "-r", &task_cid, "-m", "v1"]);
        jj_setup(
            &repo,
            &cfg,
            &["--at-operation", "@-", "describe", "-r", &task_cid, "-m", "v2"],
        );

        let workspaces = workspace_list(&repo).unwrap();

        // The old change_id-based revset errors on the divergent change — the regression.
        let old_revset = format!(
            "ancestors({}, 25)",
            workspaces
                .iter()
                .map(|w| w.change_id.clone())
                .collect::<Vec<_>>()
                .join(" | ")
        );
        assert!(
            log(&repo, &old_revset).is_err(),
            "a bare divergent change_id in the revset must error (this is what broke the graph)"
        );

        // The `<name>@` revset survives, and the divergent workspace resolves to exactly one
        // node — not duplicated, not dropped.
        let revs = log(&repo, &workspace_ancestors_revset(&workspaces, 25))
            .expect("name@ revset must survive a divergent workspace");
        let count = revs.iter().filter(|r| r.change_id == task_cid).count();
        assert_eq!(
            count, 1,
            "the divergent workspace resolves to a single graph node"
        );
    }
}
