//! What faff's engine and `faff logs`' engine do the same way: the revset the log view
//! loads, and the log view's writes (`e`, `z`), each made only once no jj command that
//! could rewrite commits is running in the checkout (see [`crate::watch`]).

use super::engine::Outbox;
use crate::jj;
use crate::watch::{self, Aliases};
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
