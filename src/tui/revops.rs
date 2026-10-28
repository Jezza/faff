//! What faff's engine and `faff logs`' engine do the same way: the revset the log view
//! loads, and the log view's writes (`e`, `z`, `f`, `F`, `x`, `X`), each made only once no jj command that
//! could rewrite commits is running in the checkout (see [`crate::watch`]).

use super::engine::{Cmd, Outbox};
use crate::domain::Task;
use crate::watch::{self, Aliases};
use crate::{config, jj, workspace};
use std::path::Path;
use std::sync::mpsc::Sender;

/// The log view's own revisions: the user's `revsets.log` (jj's default when unset).
pub fn base_revset(root: &Path) -> String {
    jj::config_get(root, "revsets.log").unwrap_or_else(|| "builtin_log()".to_string())
}

/// Older ancestors each [`super::engine::Cmd::LoadMore`] adds, in generations.
pub const PAGE: usize = 50;

/// The log view's revisions: `base`, every workspace's working copy (so agents show
/// whatever `base` says), each branch a search pulled in from its fork point off trunk,
/// each squash made here, and `depth` generations of their ancestors (what scrolling down
/// has loaded so far).
pub fn log_revset(base: &str, pulled: &[String], pinned: &[String], depth: usize) -> String {
    let mut parts = vec![format!("({base})"), "working_copies()".to_string()];
    parts.extend(
        pulled
            .iter()
            .map(|b| format!("fork_point(trunk() | {b})::{b}")),
    );
    parts.extend(pinned.iter().map(|p| format!("present({p})")));
    let all = parts.join(" | ");
    if depth == 0 {
        all
    } else {
        // `ancestors(x, 1)` is `x` itself.
        format!("ancestors({all}, {})", depth + 1)
    }
}

/// What one load from jj says: the revisions, the workspaces and the bookmarks, asked for
/// side by side so the first screen waits on the slowest of them, not on all of them.
pub struct Loaded {
    pub revs: anyhow::Result<Vec<jj::RevInfo>>,
    /// `also`'s revisions, when asked for.
    pub also: Option<anyhow::Result<Vec<jj::RevInfo>>>,
    pub workspaces: anyhow::Result<Vec<jj::Workspace>>,
    pub bookmarks: anyhow::Result<Vec<jj::BookmarkRef>>,
}

/// Load `revset` and, beside it, `also`: a `jj log` of its own rather than part of
/// `revset`'s, since jj groups a revset's branches by its heads and `revset`'s would
/// reorder `also`'s.
pub fn load(root: &Path, revset: &str, also: Option<&str>) -> Loaded {
    std::thread::scope(|s| {
        let workspaces = s.spawn(|| jj::workspace_list_peek(root));
        let bookmarks = s.spawn(|| jj::bookmark_list_peek(root));
        let also = also.map(|also| s.spawn(move || jj::log_peek(root, also)));
        let revs = jj::log_peek(root, revset);
        fn joined<T>(r: std::thread::Result<anyhow::Result<T>>) -> anyhow::Result<T> {
            r.unwrap_or_else(|_| Err(anyhow::anyhow!("jj reader panicked")))
        }
        Loaded {
            revs,
            also: also.map(|h| joined(h.join())),
            workspaces: joined(workspaces.join()),
            bookmarks: joined(bookmarks.join()),
        }
    })
}

/// What a search found in the repo's files (see [`super::engine::Cmd::SearchFiles`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Found {
    /// The text searched for, so a stale answer is told from a fresh one.
    pub query: String,
    pub matches: Vec<jj::FileMatch>,
}

impl Found {
    /// The found revisions, by commit, for the revset to load.
    pub fn revisions(found: &Option<Found>) -> Vec<String> {
        found
            .iter()
            .flat_map(|f| &f.matches)
            .map(|m| m.commit_id.clone())
            .collect()
    }
}

/// Ask jj, side by side, for the revisions whose changed paths or lines contain `query`:
/// what it found, and why either search failed.
pub fn search_files(root: &Path, query: &str) -> (Found, Vec<String>) {
    let (paths, lines) = std::thread::scope(|s| {
        let lines = s.spawn(|| jj::search_lines(root, query));
        (jj::search_paths(root, query), lines.join())
    });
    let lines = lines.unwrap_or_else(|_| Err(anyhow::anyhow!("jj reader panicked")));
    let mut matches = Vec::new();
    let mut errors = Vec::new();
    for found in [paths, lines] {
        match found {
            Ok(m) => matches.extend(m),
            Err(e) => errors.push(format!("searching files failed: {e:#}")),
        }
    }
    let found = Found {
        query: query.to_string(),
        matches,
    };
    (found, errors)
}

/// Where a file search stands, in an engine: what was last asked for, and what was found.
/// Searches run off the engine thread, so a slow one never holds up a key; an answer for
/// anything but the latest query is dropped.
#[derive(Default)]
pub struct FileSearch {
    asked: String,
    pub found: Option<Found>,
}

impl FileSearch {
    /// [`super::engine::Cmd::SearchFiles`]: start searching for `query`, reporting back
    /// on `wake` as [`super::engine::Cmd::FilesFound`]. Returns whether `found` changed
    /// now (an empty query clears it).
    pub fn ask(&mut self, root: &Path, query: String, wake: &Sender<Cmd>) -> bool {
        self.asked = query.clone();
        if query.is_empty() {
            return self.found.take().is_some();
        }
        let (root, wake) = (root.to_path_buf(), wake.clone());
        std::thread::spawn(move || {
            let (found, errors) = search_files(&root, &query);
            let _ = wake.send(Cmd::FilesFound { found, errors });
        });
        false
    }

    /// [`super::engine::Cmd::FilesFound`]: keep `found` if it's for the latest query.
    /// Returns whether it was.
    pub fn answer(&mut self, found: Found, errors: Vec<String>, notices: &Outbox) -> bool {
        if found.query != self.asked {
            return false;
        }
        for e in errors {
            notices.err(e);
        }
        self.found = Some(found);
        true
    }
}

/// Whether scrolling down could load more: some loaded revision has a parent that isn't.
pub fn more_below(revs: &[jj::RevInfo]) -> bool {
    let loaded: std::collections::HashSet<&str> =
        revs.iter().map(|r| r.change_id.as_str()).collect();
    revs.iter()
        .flat_map(|r| &r.parents)
        .any(|p| !loaded.contains(p.as_str()))
}

/// A command line cut to fit a notice or the header.
pub fn shorten(cmd: &str) -> String {
    const MAX: usize = 40;
    if cmd.chars().count() <= MAX {
        return cmd.to_string();
    }
    let cut: String = cmd.chars().take(MAX - 1).collect();
    format!("{cut}…")
}

fn short(change_id: &str) -> String {
    change_id.chars().take(8).collect()
}

const ADJECTIVES: &[&str] = &[
    "amber", "bold", "brisk", "calm", "clever", "crisp", "dapper", "eager", "fancy", "fuzzy",
    "gentle", "giddy", "golden", "hasty", "humble", "jolly", "keen", "lively", "lucky", "mellow",
    "merry", "misty", "nimble", "noble", "plucky", "proud", "quick", "quiet", "rapid", "rosy",
    "rusty", "shy", "silver", "sleepy", "snappy", "sunny", "swift", "tidy", "wild", "witty",
];

const NOUNS: &[&str] = &[
    "badger", "beacon", "bramble", "canyon", "comet", "coral", "falcon", "fern", "finch", "fox",
    "glacier", "harbor", "heron", "hollow", "lantern", "maple", "meadow", "otter", "owl", "pebble",
    "pine", "puffin", "quarry", "raven", "reef", "river", "robin", "sparrow", "spruce", "summit",
    "thistle", "tide", "tundra", "valley", "walrus", "willow", "wren", "yak", "zephyr", "lynx",
];

/// Two different adjectives and a noun, e.g. `brisk-amber-otter`, from `seed`.
fn workspace_name(seed: u64) -> String {
    let a = ADJECTIVES.len() as u64;
    let first = (seed % a) as usize;
    // Offset 1..a from the first, so the second is never the same word.
    let second = (first + 1 + ((seed / a) % (a - 1)) as usize) % ADJECTIVES.len();
    let noun = ((seed / (a * (a - 1))) % NOUNS.len() as u64) as usize;
    format!(
        "{}-{}-{}",
        ADJECTIVES[first], ADJECTIVES[second], NOUNS[noun]
    )
}

/// A fresh name for a workspace under `dir`: neither a jj workspace nor a directory there
/// already.
fn free_workspace_name(root: &Path, dir: &Path) -> anyhow::Result<String> {
    use std::hash::BuildHasher;
    let taken: Vec<String> = jj::workspace_list(root)?
        .into_iter()
        .map(|w| w.name)
        .collect();
    let state = std::collections::hash_map::RandomState::new();
    (0u64..64)
        .map(|i| workspace_name(state.hash_one(i)))
        .find(|n| !taken.contains(n) && !dir.join(n).exists())
        .ok_or_else(|| anyhow::anyhow!("no free workspace name"))
}

/// `f` / `F`: make a workspace forked from `rev` (`f`) or taking `rev` itself (`F`, which
/// refuses a revision an agent works on), under faff's workspace directory with a made-up
/// name. Its Claude memory is linked to the repo's and it is pre-trusted, as for an agent
/// workspace, minus the hooks: no agent comes with it. Returns the new workspace's `@`.
pub fn add_workspace(
    root: &Path,
    aliases: &Aliases,
    rev: &str,
    take: bool,
    tasks: &[Task],
    notices: &Outbox,
) -> Option<String> {
    let verb = if take { "move" } else { "fork" };
    if let Some(busy) = watch::busy_jj(root, aliases) {
        let doing = if take { "moving" } else { "forking" };
        notices.warn(format!("not {doing} while `{}` runs", shorten(&busy)));
        return None;
    }
    let made = config::workspace_root(root).and_then(|dir| {
        if let Some((holder, holder_dir)) =
            workspace::holder(root, &jj::resolve_change_id(root, rev)?)?
        {
            if take
                && (workspace::made_for_task(&holder)
                    || tasks.iter().any(|t| t.ws_name.as_deref() == Some(&holder)))
            {
                anyhow::bail!("an agent works on it in {holder}");
            }
            // Both snapshot the workspace on it and may move it: not while a jj command
            // that could rewrite commits runs there (an agent's, say).
            if let Some(busy) = watch::busy_jj(&holder_dir, aliases) {
                anyhow::bail!("`{}` is running in {holder}", shorten(&busy));
            }
        }
        let name = free_workspace_name(root, &dir)?;
        let path = dir.join(&name);
        if take {
            workspace::move_into(root, &name, &path, rev)
        } else {
            workspace::fork(root, &name, &path, rev)
        }
    });
    match made {
        Ok(ws) => {
            // Best-effort, as for an agent workspace.
            let claude_projects = workspace::claude_projects_dir();
            let _ = workspace::seed_memory(&claude_projects, root, &ws.path);
            let _ = workspace::trust_workspace(&ws.path);
            notices.ok(format!("workspace {} at {}", ws.name, ws.path.display()));
            Some(ws.change_id)
        }
        Err(e) => {
            notices.err(format!("{verb} {} failed: {e:#}", short(rev)));
            None
        }
    }
}

/// `x` / `X` in the log views: remove the workspace on `rev` as an agent's is removed —
/// forget it, delete its directory, and abandon its own revisions, `x` only if all are
/// empty. They start above the fork point its agent's task recorded, else the one `f` /
/// `F` noted, else `N`'s. Refuses the checkout at `root`. Returns whether it went.
pub fn remove_workspace(
    root: &Path,
    aliases: &Aliases,
    rev: &str,
    discard: bool,
    tasks: &[Task],
    notices: &Outbox,
) -> bool {
    if let Some(busy) = watch::busy_jj(root, aliases) {
        notices.warn(format!("not removing while `{}` runs", shorten(&busy)));
        return false;
    }
    let removed = (|| {
        let w = jj::resolve_change_id(root, rev)?;
        let Some((name, dir)) = workspace::holder(root, &w)? else {
            anyhow::bail!("no workspace is on it");
        };
        let here = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        if here(&dir) == here(root) {
            anyhow::bail!("{name} is the workspace this runs in");
        }
        if let Some(busy) = watch::busy_jj(&dir, aliases) {
            anyhow::bail!("`{}` is running in {name}", shorten(&busy));
        }
        let fork_point = match tasks
            .iter()
            .find(|t| t.ws_name.as_deref() == Some(name.as_str()))
            .and_then(|t| t.fork_point.clone())
        {
            Some(f) => f,
            None => workspace::fork_point_of(root, &dir, &w)?,
        };
        if discard {
            workspace::teardown_discarding_revision(root, &name, &dir, &fork_point)?;
        } else {
            workspace::teardown(root, &name, &dir, &fork_point)?;
        }
        Ok(name)
    })();
    match removed {
        Ok(name) if discard => {
            notices.ok(format!("removed {name} and discarded its revisions"));
            true
        }
        Ok(name) => {
            notices.ok(format!("removed {name}"));
            true
        }
        Err(e) => {
            notices.err(format!("remove {} failed: {e:#}", short(rev)));
            false
        }
    }
}

/// `e`: move the checkout at `root`'s `@` onto `rev`. Returns whether it moved.
pub fn edit(root: &Path, aliases: &Aliases, rev: &str, notices: &Outbox) -> bool {
    let short = short(rev);
    if let Some(busy) = watch::busy_jj(root, aliases) {
        notices.warn(format!(
            "not editing {short} while `{}` runs",
            shorten(&busy)
        ));
        return false;
    }
    match jj::edit_in(root, rev) {
        Ok(()) => {
            notices.ok(format!("editing {short}"));
            true
        }
        Err(e) => {
            notices.err(format!("edit {short} failed: {e}"));
            false
        }
    }
}

/// `z`: a new revision on `base` holding everything from there up to `rev`, called
/// `squashed: <name>`, with no working copy moved. Returns its change id.
pub fn squash(
    root: &Path,
    aliases: &Aliases,
    rev: &str,
    base: &str,
    name: &str,
    notices: &Outbox,
) -> Option<String> {
    if let Some(busy) = watch::busy_jj(root, aliases) {
        notices.warn(format!("not squashing while `{}` runs", shorten(&busy)));
        return None;
    }
    // A revision already in trunk is its own fork point: there's no branch to squash.
    match jj::resolve_change_id(root, base) {
        Ok(b) if b == rev => {
            notices.warn(format!(
                "{} is already in trunk — nothing to squash",
                short(rev)
            ));
            return None;
        }
        Ok(_) => {}
        Err(e) => {
            notices.err(format!("squash {name} failed: {e}"));
            return None;
        }
    }
    let message = format!("squashed: {name}");
    let made = jj::new_no_edit(root, base, &message)
        .and_then(|id| jj::restore_into(root, rev, &id).map(|()| id));
    match made {
        Ok(id) => {
            notices.ok(format!(
                "squashed {name} into {} — e to edit it",
                short(&id)
            ));
            Some(id)
        }
        Err(e) => {
            notices.err(format!("squash {name} failed: {e}"));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_names_are_two_different_adjectives_and_a_noun() {
        for seed in (0..20_000u64).chain([u64::MAX, u64::MAX / 3]) {
            let name = workspace_name(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
            let words: Vec<&str> = name.split('-').collect();
            assert_eq!(words.len(), 3, "{name}");
            assert!(
                ADJECTIVES.contains(&words[0]) && ADJECTIVES.contains(&words[1]),
                "{name}"
            );
            assert_ne!(words[0], words[1], "{name}");
            assert!(NOUNS.contains(&words[2]), "{name}");
        }
    }

    #[test]
    fn the_log_revset_adds_working_copies_pulls_and_pins_to_the_base() {
        assert_eq!(
            log_revset(
                "trunk()..@",
                &["bookmarks(exact:\"b\")".into()],
                &["sq".into()],
                0
            ),
            "(trunk()..@) | working_copies() \
             | fork_point(trunk() | bookmarks(exact:\"b\"))::bookmarks(exact:\"b\") | present(sq)"
        );
    }

    #[test]
    fn scrolling_down_adds_generations_of_ancestors() {
        assert_eq!(
            log_revset("trunk()..@", &[], &[], 50),
            "ancestors((trunk()..@) | working_copies(), 51)"
        );
    }

    #[test]
    fn a_file_search_keeps_only_the_answer_to_the_latest_query() {
        let (wake, _queued) = std::sync::mpsc::channel();
        let (tx, _rx) = std::sync::mpsc::channel();
        let notices = Outbox::new(tx);
        let root = Path::new("/nonexistent");
        let mut s = FileSearch::default();
        let found = |q: &str| Found {
            query: q.into(),
            matches: Vec::new(),
        };
        assert!(!s.ask(root, "lanes".into(), &wake), "nothing yet");
        assert!(!s.ask(root, "lanes!".into(), &wake));
        assert!(!s.answer(found("lanes"), Vec::new(), &notices), "stale");
        assert_eq!(s.found, None);
        assert!(s.answer(found("lanes!"), Vec::new(), &notices));
        assert_eq!(s.found, Some(found("lanes!")));
        assert!(s.ask(root, String::new(), &wake), "cleared");
        assert_eq!(s.found, None);
    }

    #[test]
    fn more_is_below_while_a_parent_is_unloaded() {
        let rev = |id: &str, parents: &[&str]| jj::RevInfo {
            change_id: id.into(),
            parents: parents.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        assert!(more_below(&[rev("b", &["a"])]));
        assert!(!more_below(&[
            rev("b", &["a"]),
            rev("a", &["root"]),
            rev("root", &[])
        ]));
    }
}
