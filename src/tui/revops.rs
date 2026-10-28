//! What faff's engine and `faff logs`' engine do the same way: the revset the log view
//! loads, and the log view's writes (`e`, `z`, `f`, `F`, `x`, `X`), each made only once no jj command that
//! could rewrite commits is running in the checkout (see [`crate::watch`]).

use super::engine::Outbox;
use crate::domain::Task;
use crate::watch::{self, Aliases};
use crate::{config, jj, workspace};
use std::path::Path;

/// The log view's own revisions: the user's `revsets.log` (jj's default when unset).
pub fn base_revset(root: &Path) -> String {
    jj::config_get(root, "revsets.log").unwrap_or_else(|| "builtin_log()".to_string())
}

/// Everything the log view loads: `base`, every workspace's working copy (so agents show
/// whatever `base` says), each branch a search pulled in from its fork point off trunk,
/// and each squash made here.
pub fn log_revset(
    base: &str,
    workspaces: &[jj::Workspace],
    pulled: &[String],
    pinned: &[String],
) -> String {
    let mut parts = vec![format!("({base})"), "present(@)".to_string()];
    parts.extend(
        workspaces
            .iter()
            .map(|w| format!("present({}@)", jj::revset_str(&w.name))),
    );
    parts.extend(
        pulled
            .iter()
            .map(|b| format!("fork_point(trunk() | {b})::{b}")),
    );
    parts.extend(pinned.iter().map(|p| format!("present({p})")));
    parts.join(" | ")
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
        let ws = vec![jj::Workspace {
            name: "faf-task-3".into(),
            change_id: "x".into(),
        }];
        assert_eq!(
            log_revset(
                "trunk()..@",
                &ws,
                &["bookmarks(exact:\"b\")".into()],
                &["sq".into()]
            ),
            "(trunk()..@) | present(@) | present(\"faf-task-3\"@) \
             | fork_point(trunk() | bookmarks(exact:\"b\"))::bookmarks(exact:\"b\") | present(sq)"
        );
    }
}
