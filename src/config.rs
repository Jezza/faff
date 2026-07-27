//! Path resolution and repo-path encoding. See spec §4.

use anyhow::{Context, Result};
use directories::ProjectDirs;
use std::path::{Path, PathBuf};

/// Encode an absolute path into a single directory segment, matching Claude Code's
/// project-key scheme (verified against the claude binary): every non-alphanumeric
/// char becomes `-`, and paths longer than 200 chars are truncated to 200 with a
/// hash suffix. Used both to name faf's own data dirs and — crucially — to locate
/// the matching `~/.claude/projects/<key>/` directory for memory seeding.
///
/// `/home/jezza/work/x` -> `-home-jezza-work-x`; `/a/.cfg` -> `-a--cfg`.
///
/// Note: for paths over 200 chars the hash suffix is faf's own (stable) hash, which
/// will not match Claude's for such long paths; memory seeding is best-effort there.
pub fn encode_repo_path(repo: &Path) -> String {
    let s = repo.to_string_lossy();
    let key: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    if key.chars().count() <= 200 {
        return key;
    }
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    let prefix: String = key.chars().take(200).collect();
    format!("{prefix}-{:x}", h.finish())
}

/// The local (non-roaming) data root for faf, e.g. `~/.local/share/faf`.
pub fn data_root() -> Result<PathBuf> {
    let pd = ProjectDirs::from("io", "peeriot", "faf")
        .context("could not determine a home/data directory")?;
    Ok(pd.data_local_dir().to_path_buf())
}

/// Per-repo faf directory: `<data_root>/<encoded-repo>`.
pub fn repo_dir(repo: &Path) -> Result<PathBuf> {
    Ok(data_root()?.join(encode_repo_path(repo)))
}

/// Where task workspaces are materialised: `<repo_dir>/ws`.
pub fn workspace_root(repo: &Path) -> Result<PathBuf> {
    Ok(repo_dir(repo)?.join("ws"))
}

/// Path to this repo's SQLite database: `<repo_dir>/faf.db`.
pub fn db_path(repo: &Path) -> Result<PathBuf> {
    Ok(repo_dir(repo)?.join("faf.db"))
}

/// Path to the per-task workspace directory: `<workspace_root>/<id>-<slug>`.
pub fn task_workspace_dir(repo: &Path, task_id: i64, slug: &str) -> Result<PathBuf> {
    Ok(workspace_root(repo)?.join(format!("{task_id:04}-{slug}")))
}

/// A filesystem-safe slug from arbitrary text (for workspace dir names).
pub fn slugify(text: &str, max_words: usize) -> String {
    let mut words: Vec<String> = Vec::new();
    for raw in text.split_whitespace() {
        let cleaned: String = raw
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .flat_map(|c| c.to_lowercase())
            .collect();
        if !cleaned.is_empty() {
            words.push(cleaned);
        }
        if words.len() >= max_words {
            break;
        }
    }
    if words.is_empty() {
        "task".to_string()
    } else {
        words.join("-")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_repo_path_like_claude() {
        assert_eq!(
            encode_repo_path(Path::new("/home/jezza/work/x")),
            "-home-jezza-work-x"
        );
        assert_eq!(
            encode_repo_path(Path::new("/home/jezza/projects/faf")),
            "-home-jezza-projects-faf"
        );
        // every non-alphanumeric char becomes '-' (matching Claude): dots, underscores, trailing slash
        assert_eq!(
            encode_repo_path(Path::new("/home/j/.config/a_b")),
            "-home-j--config-a-b"
        );
        assert_eq!(
            encode_repo_path(Path::new("/home/jezza/work/x/")),
            "-home-jezza-work-x-"
        );
    }

    #[test]
    fn encode_truncates_and_hashes_very_long_paths() {
        let long = format!("/{}", "a".repeat(300));
        let key = encode_repo_path(Path::new(&long));
        assert!(key.chars().count() <= 217, "200 + '-' + up to 16 hex");
        assert!(key.contains('-'));
    }

    #[test]
    fn data_paths_are_nested_under_repo_dir() {
        let repo = Path::new("/home/jezza/work/x");
        let rd = repo_dir(repo).unwrap();
        assert!(rd.ends_with("-home-jezza-work-x"));
        assert_eq!(workspace_root(repo).unwrap(), rd.join("ws"));
        assert_eq!(db_path(repo).unwrap(), rd.join("faf.db"));
    }

    #[test]
    fn task_workspace_dir_is_zero_padded_id_and_slug() {
        let repo = Path::new("/home/jezza/work/x");
        let p = task_workspace_dir(repo, 7, "add-auth").unwrap();
        assert!(p.ends_with("0007-add-auth"));
    }

    #[test]
    fn slugify_cleans_and_limits() {
        assert_eq!(slugify("Add OAuth login!", 3), "add-oauth-login");
        assert_eq!(slugify("Fix the tests, please, now", 2), "fix-the");
        assert_eq!(slugify("   ", 3), "task");
        assert_eq!(slugify("C++ & Rust", 3), "c-rust");
    }
}
