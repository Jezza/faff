//! MEMORY.md index reconciliation. The auto-memory index is pure derived data
//! (every memory file carries `name` and `description` frontmatter), so a lost
//! or clobbered index line is recoverable by rebuilding from the files on disk.
//! `sync_index` is invoked by the injected SessionStart hook, making the shared
//! index self-healing: hand-written entry lines are preserved, entries for
//! deleted files are dropped, and unindexed files are appended from frontmatter.

use anyhow::Result;
use std::collections::HashSet;
use std::fs;
use std::path::Path;

/// Reconcile `<memory_dir>/MEMORY.md` with the memory files actually present.
/// Returns whether the index was rewritten. Missing dir is a no-op; the write
/// is atomic (temp file + rename) so concurrent readers never see a torn index.
pub fn sync_index(memory_dir: &Path) -> Result<bool> {
    if !memory_dir.is_dir() {
        return Ok(false);
    }
    let mut files: Vec<String> = fs::read_dir(memory_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".md") && n != "MEMORY.md")
        .collect();
    files.sort();

    let index_path = memory_dir.join("MEMORY.md");
    let existing = fs::read_to_string(&index_path).ok();
    let present: HashSet<&str> = files.iter().map(String::as_str).collect();

    let mut out: Vec<String> = Vec::new();
    let mut indexed: HashSet<String> = HashSet::new();
    if let Some(cur) = &existing {
        for line in cur.lines() {
            match entry_target(line) {
                Some(t) if present.contains(t) => {
                    indexed.insert(t.to_string());
                    out.push(line.to_string());
                }
                Some(_) => {} // stale entry: target file is gone
                None => out.push(line.to_string()),
            }
        }
        while out.last().is_some_and(String::is_empty) {
            out.pop();
        }
    } else {
        if files.is_empty() {
            return Ok(false);
        }
        out.push("# Memory Index".to_string());
        out.push(String::new());
    }

    for f in &files {
        if indexed.contains(f) {
            continue;
        }
        let content = fs::read_to_string(memory_dir.join(f)).unwrap_or_default();
        let (name, description) = frontmatter_fields(&content);
        let name = name.unwrap_or_else(|| f.trim_end_matches(".md").to_string());
        out.push(match description {
            Some(d) if !d.is_empty() => format!("- [{name}]({f}) — {d}"),
            _ => format!("- [{name}]({f})"),
        });
    }

    let updated = out.join("\n") + "\n";
    if existing.as_deref() == Some(updated.as_str()) {
        return Ok(false);
    }
    let tmp = memory_dir.join(format!(".MEMORY.md.{}.tmp", std::process::id()));
    fs::write(&tmp, &updated)?;
    fs::rename(&tmp, &index_path)?;
    Ok(true)
}

/// The link-target basename of an index entry line (`- [title](file.md) — hook`),
/// or None for anything that isn't an entry.
fn entry_target(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix("- [")?;
    let (_, after) = rest.split_once("](")?;
    let (target, _) = after.split_once(')')?;
    target.rsplit('/').next()
}

/// Top-level `name:` and `description:` from YAML frontmatter, if any.
fn frontmatter_fields(content: &str) -> (Option<String>, Option<String>) {
    let mut lines = content.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return (None, None);
    }
    let (mut name, mut description) = (None, None);
    for line in lines {
        if line.trim_end() == "---" {
            break;
        }
        if let Some(v) = line.strip_prefix("name:") {
            name = Some(unquote(v));
        } else if let Some(v) = line.strip_prefix("description:") {
            description = Some(unquote(v));
        }
    }
    (name, description)
}

fn unquote(v: &str) -> String {
    v.trim().trim_matches('"').trim_matches('\'').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, content).unwrap();
        p
    }

    fn mem_file(name: &str, description: &str) -> String {
        format!(
            "---\nname: {name}\ndescription: {description}\nmetadata:\n  type: project\n---\n\nbody\n"
        )
    }

    #[test]
    fn sync_creates_index_from_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "beta.md",
            &mem_file("beta-fact", "what beta is"),
        );
        write(
            tmp.path(),
            "alpha.md",
            &mem_file("alpha-fact", "what alpha is"),
        );

        assert!(sync_index(tmp.path()).unwrap());

        assert_eq!(
            fs::read_to_string(tmp.path().join("MEMORY.md")).unwrap(),
            "# Memory Index\n\n\
             - [alpha-fact](alpha.md) — what alpha is\n\
             - [beta-fact](beta.md) — what beta is\n"
        );
    }

    #[test]
    fn sync_preserves_existing_lines_appends_missing_drops_stale() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "a.md",
            &mem_file("a-fact", "machine description"),
        );
        write(tmp.path(), "b.md", &mem_file("b-fact", "about b"));
        write(
            tmp.path(),
            "MEMORY.md",
            "# Memory Index\n\n\
             - [Custom title](a.md) — hand-written hook\n\
             - [Gone](gone.md) — points at a deleted file\n",
        );

        assert!(sync_index(tmp.path()).unwrap());

        assert_eq!(
            fs::read_to_string(tmp.path().join("MEMORY.md")).unwrap(),
            "# Memory Index\n\n\
             - [Custom title](a.md) — hand-written hook\n\
             - [b-fact](b.md) — about b\n"
        );
    }

    #[test]
    fn sync_is_noop_when_index_current() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.md", &mem_file("a-fact", "about a"));
        assert!(sync_index(tmp.path()).unwrap());
        let before = fs::read_to_string(tmp.path().join("MEMORY.md")).unwrap();

        assert!(!sync_index(tmp.path()).unwrap());
        assert_eq!(
            fs::read_to_string(tmp.path().join("MEMORY.md")).unwrap(),
            before
        );
    }

    #[test]
    fn sync_missing_dir_is_noop() {
        assert!(!sync_index(Path::new("/no/such/memory/dir")).unwrap());
    }

    #[test]
    fn sync_handles_file_without_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "raw.md", "just prose, no frontmatter\n");

        assert!(sync_index(tmp.path()).unwrap());

        assert_eq!(
            fs::read_to_string(tmp.path().join("MEMORY.md")).unwrap(),
            "# Memory Index\n\n- [raw](raw.md)\n"
        );
    }
}
