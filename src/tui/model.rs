//! Turn jj revisions + workspaces + task state into graph nodes for the renderer,
//! plus a parallel node→task map for selection. Pure and unit-tested. See spec §11.

use crate::domain::{Task, TaskId, TaskStatus};
use crate::graph::GraphNode;
use crate::jj::{RevInfo, Workspace};

/// Pin master's line to the front of the revision list, so the log always leads with
/// your `@` on the leftmost lane and agent branches sit alongside below.
///
/// The "line" is master's `@` **plus any descendant of it in the set** — a fork rebased
/// *onto* master (master becomes its parent). We must move the whole line, not just `@`:
/// hoisting `@` above its own descendant would violate the child-before-parent invariant
/// the layout relies on, and that descendant would then draw a disconnected line.
///
/// A stable partition keeps master's line (in its original child→parent order, so the
/// descendant stays above `@`) ahead of everything else, which keeps its relative order
/// too. Since the line is master plus *all* its descendants, no node outside it is a
/// descendant of one inside it, so the move never reorders a child below its parent.
pub fn pin_current_wc_first(revs: &mut [RevInfo]) {
    let Some(master_id) = revs
        .iter()
        .find(|r| r.is_current_wc)
        .map(|r| r.change_id.clone())
    else {
        return;
    };
    // `reaches[id]` = the node is master or reaches master via parent links (i.e. is a
    // descendant of it). Computed parent→child (reverse of jj's order) so each node's
    // parents are resolved before it.
    let mut reaches: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    for r in revs.iter().rev() {
        let on_line = r.change_id == master_id
            || r.parents
                .iter()
                .any(|p| reaches.get(p).copied().unwrap_or(false));
        reaches.insert(r.change_id.clone(), on_line);
    }
    // Stable sort: master's line first (key false), everything else after (key true).
    revs.sort_by_key(|r| !reaches.get(&r.change_id).copied().unwrap_or(false));
}

/// Nodes to render, plus which task (if any) each node represents.
pub struct GraphModel {
    pub nodes: Vec<GraphNode>,
    pub task_of: Vec<Option<TaskId>>,
}

/// Status icon + human label for the annotation line.
pub fn status_label(status: TaskStatus) -> (&'static str, &'static str) {
    match status {
        TaskStatus::Working => ("⚙", "working"),
        TaskStatus::NeedsInput => ("🔔", "needs you"),
        TaskStatus::Idle => ("✓", "review-ready"),
    }
}

fn task_annotation(t: &Task) -> String {
    let (icon, human) = status_label(t.status);
    match t.pane_id {
        Some(p) => format!("{icon} {human} · %{p}"),
        None => format!("{icon} {human}"),
    }
}

/// Build graph nodes from the revision list (child→parent order), the workspace map,
/// and the live tasks. Empty, description-less non-workspace commits collapse away.
pub fn build(revs: &[RevInfo], workspaces: &[Workspace], tasks: &[Task]) -> GraphModel {
    let mut nodes = Vec::with_capacity(revs.len());
    let mut task_of = Vec::with_capacity(revs.len());

    for rev in revs {
        let ws = workspaces.iter().find(|w| w.change_id == rev.change_id);
        // A rev can be the working copy of several workspaces at once: after
        // `jj edit <agent-rev>` master's default workspace and the agent's own workspace
        // both point at it. Find the faf task among ALL matching workspaces (not just the
        // first), so the agent keeps its node instead of hiding behind master and falling
        // into the detached list.
        let task = workspaces
            .iter()
            .filter(|w| w.change_id == rev.change_id)
            .find_map(|w| tasks.iter().find(|t| t.ws_name.as_deref() == Some(&w.name)));

        let (glyph, lines, collapse, tid) = if let (Some(t), true) = (task, rev.is_current_wc) {
            // Master is parked on this agent's revision (you `jj edit`ed it). Keep
            // master's own identity on the node and hang the agent beneath it as an
            // indented line, so the agent shows here — mapped to its task, not detached —
            // without conflating the two into one label.
            let mut head = "master (you)".to_string();
            if rev.conflict {
                head.push_str("  ⚠ conflict");
            }
            (
                '@',
                vec![
                    head,
                    format!("↳ #{} {}", t.id, t.label()),
                    format!("  {}", task_annotation(t)),
                ],
                false,
                Some(t.id),
            )
        } else if let Some(t) = task {
            // A task node; a conflicted revision gets the × glyph, else a filled ●.
            let g = if rev.conflict { '×' } else { '●' };
            (
                g,
                vec![format!("#{} {}", t.id, t.label()), task_annotation(t)],
                false,
                Some(t.id),
            )
        } else if rev.is_current_wc {
            let mut line = "master (you)".to_string();
            if rev.conflict {
                line.push_str("  ⚠ conflict");
            }
            ('@', vec![line], false, None)
        } else if rev.conflict {
            // A conflicted revision is exactly what the user reviews and resolves: always
            // show it, mark it with the × glyph, and never collapse it — collapsing a
            // (usually empty, description-less) conflicted *merge* would follow only its
            // first parent and split the other branch off as a disconnected line.
            let d = if rev.description.is_empty() {
                "⚠ conflict".to_string()
            } else {
                format!("{}  ⚠ conflict", rev.description)
            };
            ('×', vec![d], false, None)
        } else if ws.is_some() {
            // Some other workspace's @ (not a faf task) — non-agent, so a hollow ○.
            let d = if rev.description.is_empty() {
                "(working copy)".to_string()
            } else {
                rev.description.clone()
            };
            ('○', vec![d], false, None)
        } else if rev.empty && rev.description.is_empty() && rev.parents.len() < 2 {
            // Empty, description-less, single-parent fork-point / noise: collapse out of
            // the graph. Never collapse a merge (2+ parents) — that would drop a parent.
            ('○', vec![String::new()], true, None)
        } else {
            // An ordinary (non-agent) commit — your own history — gets a hollow ○; a
            // faf agent's revision is a filled ● (see the task branch above).
            let d = if rev.description.is_empty() {
                rev.change_id.chars().take(8).collect::<String>()
            } else {
                rev.description.clone()
            };
            ('○', vec![d], false, None)
        };

        nodes.push(GraphNode {
            change_id: rev.change_id.clone(),
            parents: rev.parents.clone(),
            glyph,
            lines,
            collapse,
        });
        task_of.push(tid);
    }

    GraphModel { nodes, task_of }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Autonomy;

    fn rev(id: &str, parents: &[&str], cwc: bool, empty: bool, desc: &str) -> RevInfo {
        RevInfo {
            change_id: id.into(),
            parents: parents.iter().map(|s| s.to_string()).collect(),
            is_current_wc: cwc,
            empty,
            conflict: false,
            id_prefix: id.chars().take(1).collect(),
            id_rest: id.chars().skip(1).collect(),
            description: desc.into(),
        }
    }

    fn task(id: i64, ws_name: &str, status: TaskStatus) -> Task {
        Task {
            id: TaskId(id),
            prompt: "add oauth login".into(),
            title: Some("Add OAuth".into()),
            status,
            priority: 0,
            autonomy: Autonomy::AcceptEdits,
            created_at: 0,
            started_at: None,
            finished_at: None,
            archived_at: None,
            fork_point: None,
            ws_name: Some(ws_name.into()),
            ws_path: None,
            ws_change_id: None,
            pane_id: Some(12),
            session_id: None,
        }
    }

    #[test]
    fn builds_master_task_forkpoint_and_base() {
        let revs = vec![
            rev("mp", &["fk"], true, true, ""),  // master @
            rev("t7", &["fk"], false, true, ""), // task @ (faf-task-7)
            rev("fk", &["p"], false, true, ""),  // empty fork-point -> collapse
            rev("p", &[], false, false, "base"), // real commit
        ];
        let workspaces = vec![
            Workspace {
                name: "default".into(),
                change_id: "mp".into(),
            },
            Workspace {
                name: "faf-task-7".into(),
                change_id: "t7".into(),
            },
        ];
        let tasks = vec![task(7, "faf-task-7", TaskStatus::Working)];

        let m = build(&revs, &workspaces, &tasks);
        assert_eq!(m.nodes.len(), 4);

        // master @
        assert_eq!(m.nodes[0].glyph, '@');
        assert_eq!(m.nodes[0].lines, vec!["master (you)"]);
        assert_eq!(m.task_of[0], None);

        // task node: two lines, glyph ● (a faf agent), mapped to task 7
        assert_eq!(m.nodes[1].glyph, '●');
        assert_eq!(m.nodes[1].lines[0], "#7 Add OAuth");
        assert!(m.nodes[1].lines[1].contains("⚙"));
        assert!(m.nodes[1].lines[1].contains("%12"));
        assert_eq!(m.task_of[1], Some(TaskId(7)));

        // fork-point collapses
        assert!(m.nodes[2].collapse);
        assert_eq!(m.task_of[2], None);

        // base commit: ordinary non-agent history → hollow ○, shows its description
        assert_eq!(m.nodes[3].glyph, '○');
        assert_eq!(m.nodes[3].lines, vec!["base"]);
        assert!(!m.nodes[3].collapse);
    }

    #[test]
    fn master_editing_an_agent_rev_shows_both_and_stays_mapped() {
        // `jj edit <agent-rev>`: master's default workspace AND the agent's workspace
        // both point at the same rev, which is the current wc. It must render as one
        // node showing master + the agent, mapped to the task (so it isn't detached).
        let revs = vec![
            rev("x", &["base"], true, false, "agent: did work"),
            rev("base", &[], false, false, "base"),
        ];
        // `default` is listed first — the faf task must still be found behind it.
        let workspaces = vec![
            Workspace {
                name: "default".into(),
                change_id: "x".into(),
            },
            Workspace {
                name: "faf-task-1".into(),
                change_id: "x".into(),
            },
        ];
        let tasks = vec![task(1, "faf-task-1", TaskStatus::Working)];
        let m = build(&revs, &workspaces, &tasks);
        assert_eq!(m.nodes[0].glyph, '@');
        // master keeps its own line; the agent hangs beneath as an indented sub-line.
        assert_eq!(m.nodes[0].lines[0], "master (you)");
        assert!(m.nodes[0].lines[1].starts_with("↳ #1"));
        assert!(m.nodes[0].lines[2].contains("working"));
        // Mapped to the task → the refresh's detached list won't claim it.
        assert_eq!(m.task_of[0], Some(TaskId(1)));
    }

    #[test]
    fn pin_current_wc_moves_master_to_top_preserving_order() {
        // task head, then master @ (no descendants), then ancestors — master's line is
        // just master, so it jumps to index 0.
        let mut revs = vec![
            rev("t7", &["fk"], false, true, ""),
            rev("master", &["fk"], true, true, ""),
            rev("fk", &["p"], false, true, ""),
            rev("p", &[], false, false, "base"),
        ];
        pin_current_wc_first(&mut revs);
        let order: Vec<&str> = revs.iter().map(|r| r.change_id.as_str()).collect();
        assert_eq!(order, vec!["master", "t7", "fk", "p"]);
        // no-op when already first
        pin_current_wc_first(&mut revs);
        assert_eq!(revs[0].change_id, "master");
    }

    #[test]
    fn keeps_a_fork_from_master_connected() {
        // `kmk` was rebased ONTO master, so master is kmk's parent (kmk descends from
        // master). Master must NOT be hoisted above kmk — the whole master line (kmk
        // then master) leads, so kmk stays connected on master's lane instead of drawing
        // its own. A separate agent off `base` follows.
        let mut revs = vec![
            rev("kmk", &["master"], false, false, "rebased onto master"),
            rev("master", &["base"], true, false, "master work"),
            rev("agent", &["base"], false, true, ""),
            rev("base", &[], false, false, "base"),
        ];
        pin_current_wc_first(&mut revs);
        let order: Vec<&str> = revs.iter().map(|r| r.change_id.as_str()).collect();
        assert_eq!(order, vec!["kmk", "master", "agent", "base"]);
    }

    #[test]
    fn pins_buried_master_line_ahead_of_an_agent_head() {
        // An agent head sorts above master, which has no descendants — master's line
        // (just master) is pulled ahead so master lands on the leftmost lane.
        let mut revs = vec![
            rev("agent", &["base"], false, true, ""),
            rev("master", &["base"], true, false, "master work"),
            rev("base", &[], false, false, "base"),
        ];
        pin_current_wc_first(&mut revs);
        let order: Vec<&str> = revs.iter().map(|r| r.change_id.as_str()).collect();
        assert_eq!(order, vec!["master", "agent", "base"]);
    }

    #[test]
    fn task_node_uses_filled_glyph_and_status_label() {
        let revs = vec![rev("t1", &["p"], false, true, "")];
        let workspaces = vec![Workspace {
            name: "faf-task-1".into(),
            change_id: "t1".into(),
        }];
        let tasks = vec![task(1, "faf-task-1", TaskStatus::Working)];
        let m = build(&revs, &workspaces, &tasks);
        assert_eq!(m.nodes[0].glyph, '●');
        assert!(m.nodes[0].lines[1].contains("working"));
    }

    #[test]
    fn conflicted_merge_is_shown_not_collapsed() {
        // A description-less, empty, conflicted merge (e.g. `jj new master agent`) must
        // stay in the graph — collapsing it used to drop a parent and split the branch.
        let mut merge = rev("cm", &["mw", "ag"], false, true, "");
        merge.conflict = true;
        let revs = vec![
            rev("m", &["cm"], true, true, ""), // master @
            merge,                             // the conflicted merge
            rev("mw", &["base"], false, false, "master: work"),
            rev("ag", &["base"], false, false, "agent: work"),
            rev("base", &[], false, false, "base"),
        ];
        let m = build(&revs, &[], &[]);
        // The merge node is present, marked, and NOT collapsed.
        let cm = &m.nodes[1];
        assert_eq!(cm.change_id, "cm");
        assert!(!cm.collapse, "a conflicted merge must never collapse");
        assert_eq!(cm.glyph, '×');
        assert!(cm.lines[0].contains("conflict"));
        // Its two parents are preserved (nothing dropped).
        assert_eq!(cm.parents, vec!["mw", "ag"]);
    }

    #[test]
    fn conflicted_master_working_copy_is_flagged() {
        let mut wc = rev("m", &["base"], true, true, "");
        wc.conflict = true;
        let m = build(&[wc, rev("base", &[], false, false, "base")], &[], &[]);
        assert_eq!(m.nodes[0].glyph, '@');
        assert!(m.nodes[0].lines[0].contains("conflict"));
    }

    #[test]
    fn empty_merge_without_conflict_still_not_collapsed() {
        // Even a non-conflicted empty, description-less merge must stay (collapsing it
        // would follow only the first parent and disconnect the other branch).
        let revs = vec![
            rev("mrg", &["a", "b"], false, true, ""),
            rev("a", &["base"], false, false, "a"),
            rev("b", &["base"], false, false, "b"),
            rev("base", &[], false, false, "base"),
        ];
        let m = build(&revs, &[], &[]);
        assert!(!m.nodes[0].collapse);
        assert_eq!(m.nodes[0].parents, vec!["a", "b"]);
    }
}
