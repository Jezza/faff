//! Keeping `faff logs` live: wake when jj records an operation, notice file edits worth
//! snapshotting, and stay out of the way of a jj command that's still running.
//!
//! The last one is the safety net. A snapshot that lands while `jj git fetch` (or a rebase,
//! or a `describe` waiting on its editor) is mid-flight forks the op log, and the two sides
//! can leave `@` divergent — exactly what made faff's old snapshotting poll go wrong. So
//! before snapshotting, [`busy_jj`] looks for a jj process in this checkout that might
//! rewrite commits, and the caller waits until there isn't one.

use anyhow::{Context, Result};
use ignore::gitignore::Gitignore;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The directory jj rewrites on every operation: `<repo>/op_heads/heads`. In a secondary
/// workspace `.jj/repo` is a file holding the shared repo's path, so follow it.
pub fn op_heads_dir(workspace_root: &Path) -> Result<PathBuf> {
    let repo = workspace_root.join(".jj").join("repo");
    let repo = if repo.is_file() {
        let target = std::fs::read_to_string(&repo)
            .with_context(|| format!("reading {}", repo.display()))?;
        let target = PathBuf::from(target.trim());
        if target.is_absolute() {
            target
        } else {
            workspace_root.join(".jj").join(target)
        }
    } else {
        repo
    };
    Ok(repo.join("op_heads").join("heads"))
}

/// Call `on_change` whenever jj records an operation. Keep the returned watcher alive for
/// as long as the calls should keep coming.
pub fn watch_ops(
    workspace_root: &Path,
    on_change: impl Fn() + Send + 'static,
) -> Result<RecommendedWatcher> {
    let dir = op_heads_dir(workspace_root)?;
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if res.is_ok_and(|e| !matches!(e.kind, EventKind::Access(_))) {
            on_change();
        }
    })
    .context("starting the operation watcher")?;
    watcher
        .watch(&dir, RecursiveMode::NonRecursive)
        .with_context(|| format!("watching {}", dir.display()))?;
    Ok(watcher)
}

/// Call `on_change` whenever a file jj would snapshot changes under `root`: anything but
/// `.jj/`, `.git/` and gitignored paths (build output churns constantly and jj ignores it
/// anyway). Keep the returned watcher alive for as long as the calls should keep coming.
pub fn watch_files(
    root: &Path,
    on_change: impl Fn() + Send + 'static,
) -> Result<RecommendedWatcher> {
    let ignores = Ignores::load(root);
    let base = root.to_path_buf();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else { return };
        if matches!(event.kind, EventKind::Access(_)) {
            return;
        }
        if event.paths.iter().any(|p| !ignores.ignored(&base, p)) {
            on_change();
        }
    })
    .context("starting the file watcher")?;
    watcher
        .watch(root, RecursiveMode::Recursive)
        .with_context(|| format!("watching {}", root.display()))?;
    Ok(watcher)
}

/// The checkout's ignore rules: each `.gitignore` (matched relative to its own directory),
/// `.git/info/exclude`, and the user's global excludes. Loaded once, at start-up.
struct Ignores {
    matchers: Vec<Gitignore>,
}

impl Ignores {
    fn load(root: &Path) -> Ignores {
        let mut matchers = Vec::new();
        let (global, _) = Gitignore::global();
        matchers.push(global);
        let exclude = root.join(".git").join("info").join("exclude");
        if exclude.is_file() {
            let mut b = ignore::gitignore::GitignoreBuilder::new(root);
            b.add(&exclude);
            if let Ok(g) = b.build() {
                matchers.push(g);
            }
        }
        // Walk with the ignore rules applied, so a huge ignored tree (`target/`) is never
        // descended into while looking for nested `.gitignore`s.
        for entry in ignore::WalkBuilder::new(root)
            .hidden(false)
            .build()
            .flatten()
        {
            if entry.file_name() == ".gitignore" {
                let dir = entry.path().parent().unwrap_or(root);
                let mut b = ignore::gitignore::GitignoreBuilder::new(dir);
                b.add(entry.path());
                if let Ok(g) = b.build() {
                    matchers.push(g);
                }
            }
        }
        Ignores { matchers }
    }

    fn ignored(&self, root: &Path, path: &Path) -> bool {
        let Ok(rel) = path.strip_prefix(root) else {
            return true;
        };
        if rel
            .components()
            .next()
            .is_some_and(|c| c.as_os_str() == ".jj" || c.as_os_str() == ".git")
        {
            return true;
        }
        let is_dir = path.is_dir();
        self.matchers.iter().any(|g| {
            path.starts_with(g.path()) && g.matched_path_or_any_parents(path, is_dir).is_ignore()
        })
    }
}

/// jj's global options that take a value as the next argument.
const VALUE_FLAGS: &[&str] = &[
    "-R",
    "--repository",
    "--at-operation",
    "--at-op",
    "--color",
    "--config",
    "--config-file",
    "--config-toml",
];

/// Subcommands (and `group sub` pairs) that never rewrite a commit after their start-up
/// snapshot, so a snapshot of ours can't race them into divergence. Everything else —
/// including anything unknown — counts as a writer.
const READ_ONLY: &[&str] = &[
    "log",
    "show",
    "diff",
    "st",
    "status",
    "evolog",
    "obslog",
    "interdiff",
    "root",
    "help",
    "version",
    "config",
    "op log",
    "op show",
    "operation log",
    "operation show",
    "file show",
    "file list",
    "file annotate",
    "bookmark list",
    "b l",
    "b list",
    "bookmark l",
    "tag list",
    "workspace list",
    "workspace root",
    "util exec",
];

/// The first two words of each `jj` alias, so `jj peek` is judged as the `log` it runs.
pub type Aliases = HashMap<String, Vec<String>>;

/// Read the user's aliases from `jj config list aliases`. Only the leading words of each
/// alias matter, and they always sit on the alias's first line.
pub fn load_aliases(repo: &Path) -> Aliases {
    let out = std::process::Command::new("jj")
        .arg("-R")
        .arg(repo)
        .args(["--ignore-working-copy", "config", "list", "aliases"])
        .output();
    let Ok(out) = out else {
        return Aliases::new();
    };
    parse_aliases(&String::from_utf8_lossy(&out.stdout))
}

fn parse_aliases(listing: &str) -> Aliases {
    let mut aliases = Aliases::new();
    for line in listing.lines() {
        let Some(rest) = line.strip_prefix("aliases.") else {
            continue;
        };
        let Some((name, value)) = rest.split_once(" = [") else {
            continue;
        };
        let words: Vec<String> = value
            .split(',')
            .map(str::trim)
            .take(2)
            .filter_map(|w| w.strip_prefix('"')?.strip_suffix('"').map(str::to_string))
            .collect();
        if !words.is_empty() {
            aliases.insert(name.trim_matches('"').to_string(), words);
        }
    }
    aliases
}

/// Whether a jj invocation (its arguments after `jj`) might rewrite commits while it runs.
pub fn may_write(args: &[String], aliases: &Aliases) -> bool {
    let mut words: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        // `--ignore-working-copy` is no exemption: it skips the working-copy lock, so a
        // writer run with it (`jj --ignore-working-copy describe`) is the one a snapshot
        // can't serialize against, and racing it leaves `@` divergent.
        if a == "--" {
            break;
        }
        if a.starts_with('-') {
            if VALUE_FLAGS.contains(&a.as_str()) {
                i += 1;
            }
        } else {
            words.push(a.clone());
            if words.len() == 2 {
                break;
            }
        }
        i += 1;
    }
    let Some(first) = words.first() else {
        // A bare `jj` is `jj log`.
        return false;
    };
    if let Some(expansion) = aliases.get(first) {
        let mut expanded = expansion.clone();
        expanded.extend(words.iter().skip(1).cloned());
        words = expanded;
    }
    let one = words[0].as_str();
    let two = words.get(1).map(|w| format!("{one} {w}"));
    !(READ_ONLY.contains(&one) || two.is_some_and(|t| READ_ONLY.contains(&t.as_str())))
}

/// A running jj process, rooted in this checkout, that may rewrite commits: its command
/// line, for telling the user what a snapshot is waiting on. `None` when it's safe.
///
/// Linux-only (it reads `/proc`); elsewhere it never finds anything.
pub fn busy_jj(root: &Path, aliases: &Aliases) -> Option<String> {
    let root = root.canonicalize().ok()?;
    let me = std::process::id();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == me {
            continue;
        }
        let dir = entry.path();
        let is_jj = std::fs::read_to_string(dir.join("comm")).is_ok_and(|c| c.trim() == "jj");
        if !is_jj {
            continue;
        }
        let Ok(raw) = std::fs::read(dir.join("cmdline")) else {
            continue;
        };
        let args: Vec<String> = raw
            .split(|b| *b == 0)
            .skip(1)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        let cwd = std::fs::read_link(dir.join("cwd")).unwrap_or_default();
        if !targets(&root, &cwd, &args) || !may_write(&args, aliases) {
            continue;
        }
        return Some(format!("jj {}", args.join(" ")));
    }
    None
}

/// Whether a jj run from `cwd` with `args` acts on the checkout at `root`: `-R` wins over
/// the working directory, as it does in jj.
fn targets(root: &Path, cwd: &Path, args: &[String]) -> bool {
    let repo_arg = args.iter().enumerate().find_map(|(i, a)| {
        if a == "-R" || a == "--repository" {
            args.get(i + 1).cloned()
        } else {
            a.strip_prefix("--repository=").map(str::to_string)
        }
    });
    let dir = match repo_arg {
        Some(r) => {
            let p = PathBuf::from(r);
            if p.is_absolute() { p } else { cwd.join(p) }
        }
        None => cwd.to_path_buf(),
    };
    dir.canonicalize().unwrap_or(dir).starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn readers_never_hold_up_a_snapshot() {
        let none = Aliases::new();
        for cmd in [
            "log",
            "--color always log -r @",
            "-R /x st",
            "diff --git",
            "op log",
            "bookmark list --all-remotes",
            "util exec -- jayjay .",
            "--ignore-working-copy log -r @",
            "",
        ] {
            assert!(!may_write(&args(cmd), &none), "{cmd:?} reads only");
        }
    }

    #[test]
    fn writers_and_unknowns_hold_up_a_snapshot() {
        let none = Aliases::new();
        for cmd in [
            "git fetch",
            "rebase -d main",
            "describe",
            "new",
            "--config ui.color=never squash",
            "bookmark move main --to @",
            "op restore abc",
            "--ignore-working-copy describe -r @ -m x",
            "something-new",
        ] {
            assert!(may_write(&args(cmd), &none), "{cmd:?} may write");
        }
    }

    #[test]
    fn aliases_are_judged_by_what_they_run() {
        let listing = r#"aliases.peek = ["log", "--color", "always", "--ignore-working-copy"]
aliases.ui = ["util", "exec", "--", "jayjay", "."]
aliases.sq = ["util", "exec", "--", "bash", "-c", """
aliases.restack = ["rebase", "--onto", "trunk()"]
"#;
        let aliases = parse_aliases(listing);
        assert_eq!(aliases["peek"], vec!["log", "--color"]);
        assert!(!may_write(&args("peek"), &aliases));
        assert!(!may_write(&args("ui"), &aliases));
        assert!(!may_write(&args("sq my-branch"), &aliases));
        assert!(may_write(&args("restack"), &aliases));
    }

    #[test]
    fn a_repository_flag_beats_the_working_directory() {
        let root = std::env::temp_dir();
        let elsewhere = Path::new("/");
        assert!(targets(&root.canonicalize().unwrap(), &root, &args("log")));
        assert!(!targets(
            &root.canonicalize().unwrap(),
            elsewhere,
            &args("log")
        ));
        let r = format!("-R {} log", root.display());
        assert!(targets(&root.canonicalize().unwrap(), elsewhere, &args(&r)));
    }

    #[test]
    fn op_heads_follow_a_secondary_workspace_to_the_shared_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(main.join(".jj/repo/op_heads/heads")).unwrap();
        std::fs::create_dir_all(ws.join(".jj")).unwrap();
        std::fs::write(ws.join(".jj/repo"), main.join(".jj/repo").to_str().unwrap()).unwrap();
        assert_eq!(
            op_heads_dir(&main).unwrap(),
            main.join(".jj/repo/op_heads/heads")
        );
        assert_eq!(
            op_heads_dir(&ws).unwrap(),
            main.join(".jj/repo/op_heads/heads")
        );
    }

    #[test]
    fn ignored_paths_never_trigger_a_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/.gitignore"), "*.log\n").unwrap();
        let ig = Ignores::load(root);
        assert!(ig.ignored(root, &root.join(".jj/working_copy/x")));
        assert!(ig.ignored(root, &root.join(".git/index")));
        assert!(ig.ignored(root, &root.join("target/debug/faff")));
        assert!(ig.ignored(root, &root.join("sub/run.log")));
        assert!(
            !ig.ignored(root, &root.join("run.log")),
            "sub's rules stay in sub"
        );
        assert!(!ig.ignored(root, &root.join("src/main.rs")));
    }
}
