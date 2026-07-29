//! Turn jj revisions + workspaces + task state into graph nodes for the renderer,
//! plus a parallel node→task map for selection. Pure and unit-tested. See spec §11.

use crate::domain::{Task, TaskId, TaskStatus};
use crate::graph::{GraphNode, GraphRow};
use crate::jj::{RevInfo, Workspace};

/// Pin HEAD's line to the front of the revision list, so the log always leads with
/// your `@` on the leftmost lane and agent branches sit alongside below.
///
/// The "line" is HEAD's `@` **plus any descendant of it in the set** — a fork rebased
/// *onto* HEAD (HEAD becomes its parent). We must move the whole line, not just `@`:
/// hoisting `@` above its own descendant would violate the child-before-parent invariant
/// the layout relies on, and that descendant would then draw a disconnected line.
///
/// A stable partition keeps HEAD's line (in its original child→parent order, so the
/// descendant stays above `@`) ahead of everything else, which keeps its relative order
/// too. Since the line is HEAD plus *all* its descendants, no node outside it is a
/// descendant of one inside it, so the move never reorders a child below its parent.
pub fn pin_current_wc_first(revs: &mut [RevInfo]) {
    let Some(head_id) = revs
        .iter()
        .find(|r| r.is_current_wc)
        .map(|r| r.change_id.clone())
    else {
        return;
    };
    // `reaches[id]` = the node is HEAD or reaches HEAD via parent links (i.e. is a
    // descendant of it). Computed parent→child (reverse of jj's order) so each node's
    // parents are resolved before it.
    let mut reaches: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    for r in revs.iter().rev() {
        let on_line = r.change_id == head_id
            || r.parents
                .iter()
                .any(|p| reaches.get(p).copied().unwrap_or(false));
        reaches.insert(r.change_id.clone(), on_line);
    }
    // Stable sort: HEAD's line first (key false), everything else after (key true).
    revs.sort_by_key(|r| !reaches.get(&r.change_id).copied().unwrap_or(false));
}

/// Order revisions for the flat lane layout: the trunk — HEAD's first-parent chain — stays
/// in child→parent order, and every other revision is lifted to sit directly above the
/// trunk revision it forked from. Each agent then renders as a one-row `├─●` stub anchored
/// to its fork point, and no lane is ever held open across an unrelated node.
///
/// Agents sharing a fork point are ordered by task id, newest (highest) first — a stable
/// order that doesn't shuffle as agents become active, unlike jj log's recency order.
/// `workspaces`/`tasks` supply the change_id→task-id mapping this needs.
///
/// Falls back to [`pin_current_wc_first`] when there is no working copy in the set, or when
/// some revision forks off a non-trunk node — shapes this flat anchoring isn't meant for,
/// where the conservative "HEAD's line first, rest untouched" pin stays correct.
pub fn order_by_fork_point(revs: &mut [RevInfo], workspaces: &[Workspace], tasks: &[Task]) {
    let Some(head_id) = revs
        .iter()
        .find(|r| r.is_current_wc)
        .map(|r| r.change_id.clone())
    else {
        return;
    };

    // Trunk (lane 0): HEAD, then its first parent, then that node's first parent, … while
    // each is present in the set.
    let by_id: std::collections::HashMap<&str, &RevInfo> =
        revs.iter().map(|r| (r.change_id.as_str(), r)).collect();
    let mut trunk: Vec<String> = Vec::new();
    let mut on_trunk: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut cur = Some(head_id);
    while let Some(id) = cur {
        if !on_trunk.insert(id.clone()) {
            break; // cycle guard — never on a DAG
        }
        let Some(r) = by_id.get(id.as_str()) else { break };
        cur = r.parents.first().cloned();
        trunk.push(id);
    }

    // Every non-trunk revision must fork off a trunk node (its first parent). If any forks
    // off something else, this isn't the flat shape — leave it to the conservative pin.
    let clean = revs.iter().all(|r| {
        on_trunk.contains(&r.change_id) || r.parents.first().is_some_and(|p| on_trunk.contains(p))
    });
    drop(by_id);
    if !clean {
        pin_current_wc_first(revs);
        return;
    }

    // Bucket the non-trunk revisions under their fork-base trunk node.
    let mut children: std::collections::HashMap<String, Vec<usize>> =
        std::collections::HashMap::new();
    let mut trunk_pos: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (i, r) in revs.iter().enumerate() {
        if on_trunk.contains(&r.change_id) {
            trunk_pos.insert(r.change_id.clone(), i);
        } else {
            let base = r.parents.first().cloned().unwrap(); // clean ⇒ Some, and in trunk
            children.entry(base).or_default().push(i);
        }
    }

    // Order each fork-point bucket by task id — newest (highest) first — so the list is
    // stable: an agent no longer jumps position when it becomes active (jj log's recency
    // order did that). change_id breaks ties, keeping the order total and deterministic;
    // a bucketed rev with no faff task (a stray workspace) sorts last.
    let task_id_of = |cid: &str| -> Option<i64> {
        workspaces
            .iter()
            .filter(|w| w.change_id == cid)
            .find_map(|w| tasks.iter().find(|t| t.ws_name.as_deref() == Some(&w.name)))
            .map(|t| t.id.0)
    };
    for kids in children.values_mut() {
        kids.sort_by(|&a, &b| {
            let (ka, kb) = (task_id_of(&revs[a].change_id), task_id_of(&revs[b].change_id));
            kb.cmp(&ka)
                .then_with(|| revs[a].change_id.cmp(&revs[b].change_id))
        });
    }

    // Emit each trunk node top→bottom, with its forked children lifted just above it.
    let mut order: Vec<usize> = Vec::with_capacity(revs.len());
    for t in &trunk {
        if let Some(kids) = children.get(t) {
            order.extend(kids.iter().copied());
        }
        order.push(trunk_pos[t]);
    }

    let reordered: Vec<RevInfo> = order.iter().map(|&i| revs[i].clone()).collect();
    revs.clone_from_slice(&reordered);
}

/// Nodes to render, plus which task (if any) each node represents.
pub struct GraphModel {
    pub nodes: Vec<GraphNode>,
    pub task_of: Vec<Option<TaskId>>,
}

/// Map each rendered row to the task it represents, for selection highlight and the
/// docked-session `▶` marker. Parallel to `rows`.
///
/// A task normally rides its node's commit row. The exception is the combined node —
/// where HEAD (`@`) is parked on an agent's revision (`jj edit`), so the commit row is
/// the HEAD header (its own description) and the agent hangs beneath it. There the task
/// rides the agent's own line (the first continuation row), so selecting the task
/// highlights the agent, not the shared HEAD header. `nodes`/`node_task` are the model's
/// nodes and per-node task map; `node_index` on a row indexes into them.
pub fn row_tasks(
    rows: &[GraphRow],
    nodes: &[GraphNode],
    node_task: &[Option<TaskId>],
) -> Vec<Option<TaskId>> {
    let mut out = vec![None; rows.len()];
    for (i, row) in rows.iter().enumerate() {
        let Some(ni) = row.node_index else { continue };
        let Some(tid) = node_task.get(ni).copied().flatten() else {
            continue;
        };
        // Combined node: a task on the `@` (HEAD) node means HEAD is parked on that
        // agent's revision. Hang the task on the agent line (the first continuation
        // row) so the highlight lands there, not on the shared HEAD header.
        if nodes.get(ni).map(|n| n.glyph) == Some('@') {
            out[(i + 1).min(rows.len() - 1)] = Some(tid);
        } else {
            out[i] = Some(tid);
        }
    }
    out
}

/// Status icon + human label for the annotation line.
pub fn status_label(status: TaskStatus) -> (&'static str, &'static str) {
    match status {
        TaskStatus::Working => ("⚙", "working"),
        TaskStatus::NeedsInput => ("🔔", "needs you"),
        TaskStatus::Idle => ("✓", "review-ready"),
    }
}

/// The working-copy (`@`) label: the revision's own description, or "(no description
/// set)" when it has none. Mirrors how jj log shows your commit; the green `@` glyph
/// (drawn by the renderer) is the "you are here" marker.
fn head_label(rev: &RevInfo) -> String {
    if rev.description.is_empty() {
        "(no description set)".to_string()
    } else {
        rev.description.clone()
    }
}

/// Build graph nodes from the revision list (child→parent order), the workspace map,
/// and the live tasks. Empty, description-less non-workspace commits collapse away.
pub fn build(revs: &[RevInfo], workspaces: &[Workspace], tasks: &[Task]) -> GraphModel {
    let mut nodes = Vec::with_capacity(revs.len());
    let mut task_of = Vec::with_capacity(revs.len());

    // Widest task-id (in digits) across the tasks, so every `#id` field pads to a common
    // width and the status emoji that follows lines up down the stacked agent rows.
    let id_width = tasks
        .iter()
        .map(|t| t.id.0.to_string().len())
        .max()
        .unwrap_or(1);

    for rev in revs {
        let ws = workspaces.iter().find(|w| w.change_id == rev.change_id);
        // A rev can be the working copy of several workspaces at once: after
        // `jj edit <agent-rev>` HEAD's default workspace and the agent's own workspace
        // both point at it. Find the faf task among ALL matching workspaces (not just the
        // first), so the agent keeps its node instead of hiding behind HEAD and falling
        // into the detached list.
        let task = workspaces
            .iter()
            .filter(|w| w.change_id == rev.change_id)
            .find_map(|w| tasks.iter().find(|t| t.ws_name.as_deref() == Some(&w.name)));

        let (glyph, mut lines, collapse, tid) = if let (Some(t), true) = (task, rev.is_current_wc) {
            // HEAD is parked on this agent's revision (you `jj edit`ed it). Keep HEAD's
            // own line — the revision's description — on the node and hang the agent
            // beneath it as one indented line, in the same `#id emoji :: title` form the
            // standalone agent rows use, so the agent shows here — mapped to its task, not
            // detached — without conflating the two into one label.
            let mut head = head_label(rev);
            if rev.conflict {
                head.push_str("  ⚠ conflict");
            }
            let (icon, _) = status_label(t.status);
            (
                '@',
                vec![
                    head,
                    format!("↳ #{:<w$} {} :: {}", t.id.0, icon, t.label(), w = id_width),
                ],
                false,
                Some(t.id),
            )
        } else if let Some(t) = task {
            // A task node; a conflicted revision gets the × glyph, else a filled ●.
            // Prefer the change's own jj description when set (it's live and authoritative
            // — the agent's own `jj describe`), falling back to the prompt-derived label
            // before the change has been described. Status is the
            // emoji alone, inline before the title; the whole agent is one line, so the
            // renderer folds it to a single `├─●` row anchored above its fork point.
            let g = if rev.conflict { '×' } else { '●' };
            let label = if rev.description.is_empty() {
                t.label()
            } else {
                rev.description.clone()
            };
            let (icon, _) = status_label(t.status);
            (
                g,
                vec![format!("#{:<w$} {} :: {}", t.id.0, icon, label, w = id_width)],
                false,
                Some(t.id),
            )
        } else if rev.is_current_wc {
            let mut line = head_label(rev);
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
            // faf agent's revision is a filled ● (see the task branch above). No
            // description falls back to jj's "(no description set)" (as the `@` nodes do),
            // never the change id — that's already drawn in the "[abcdefgh]" id column.
            let d = if rev.description.is_empty() {
                "(no description set)".to_string()
            } else {
                rev.description.clone()
            };
            ('○', vec![d], false, None)
        };

        // Lead the label with a fill glyph — right after the id column — marking whether
        // the change has content: █ non-empty, ░ empty. Every visible node carries one;
        // collapsed graph-noise nodes are spliced out before render, so they get none.
        if !collapse && let Some(first) = lines.first_mut() {
            let fill = if rev.empty { '░' } else { '█' };
            *first = format!("{fill} {first}");
        }

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
    fn builds_head_task_forkpoint_and_base() {
        let revs = vec![
            rev("mp", &["fk"], true, true, ""), // HEAD @
            // task @ (faf-task-7) — carries a jj description, which the row prefers.
            rev("t7", &["fk"], false, true, "Add OAuth flow"),
            rev("fk", &["p"], false, true, ""), // empty fork-point -> collapse
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

        // HEAD @ — empty and no description, so the fill is ░ and the label is jj's
        // placeholder.
        assert_eq!(m.nodes[0].glyph, '@');
        assert_eq!(m.nodes[0].lines, vec!["░ (no description set)"]);
        assert_eq!(m.task_of[0], None);

        // task node: one line `#id emoji :: title`, glyph ● (a faf agent), mapped to task
        // 7. The title is the change's jj description, not the prompt.
        assert_eq!(m.nodes[1].glyph, '●');
        assert_eq!(m.nodes[1].lines, vec!["░ #7 ⚙ :: Add OAuth flow"]);
        assert_eq!(m.task_of[1], Some(TaskId(7)));

        // fork-point collapses
        assert!(m.nodes[2].collapse);
        assert_eq!(m.task_of[2], None);

        // base commit: ordinary non-agent history → hollow ○, non-empty (█), shows its
        // description
        assert_eq!(m.nodes[3].glyph, '○');
        assert_eq!(m.nodes[3].lines, vec!["█ base"]);
        assert!(!m.nodes[3].collapse);
    }

    #[test]
    fn head_editing_an_agent_rev_shows_both_and_stays_mapped() {
        // `jj edit <agent-rev>`: HEAD's default workspace AND the agent's workspace
        // both point at the same rev, which is the current wc. It must render as one
        // node showing HEAD + the agent, mapped to the task (so it isn't detached).
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
        // HEAD keeps its own line — the shared revision's description; the agent hangs
        // beneath as one indented `↳ #id emoji :: title` sub-line.
        assert_eq!(m.nodes[0].lines[0], "█ agent: did work");
        assert!(m.nodes[0].lines[1].starts_with("↳ #1 ⚙ :: "));
        assert_eq!(m.nodes[0].lines.len(), 2);
        // Mapped to the task → the refresh's detached list won't claim it.
        assert_eq!(m.task_of[0], Some(TaskId(1)));
    }

    #[test]
    fn pin_current_wc_moves_head_to_top_preserving_order() {
        // task head, then HEAD @ (no descendants), then ancestors — HEAD's line is
        // just HEAD, so it jumps to index 0.
        let mut revs = vec![
            rev("t7", &["fk"], false, true, ""),
            rev("head", &["fk"], true, true, ""),
            rev("fk", &["p"], false, true, ""),
            rev("p", &[], false, false, "base"),
        ];
        pin_current_wc_first(&mut revs);
        let order: Vec<&str> = revs.iter().map(|r| r.change_id.as_str()).collect();
        assert_eq!(order, vec!["head", "t7", "fk", "p"]);
        // no-op when already first
        pin_current_wc_first(&mut revs);
        assert_eq!(revs[0].change_id, "head");
    }

    #[test]
    fn keeps_a_fork_from_head_connected() {
        // `kmk` was rebased ONTO HEAD, so HEAD is kmk's parent (kmk descends from
        // HEAD). HEAD must NOT be hoisted above kmk — the whole HEAD line (kmk
        // then HEAD) leads, so kmk stays connected on HEAD's lane instead of drawing
        // its own. A separate agent off `base` follows.
        let mut revs = vec![
            rev("kmk", &["head"], false, false, "rebased onto HEAD"),
            rev("head", &["base"], true, false, "HEAD work"),
            rev("agent", &["base"], false, true, ""),
            rev("base", &[], false, false, "base"),
        ];
        pin_current_wc_first(&mut revs);
        let order: Vec<&str> = revs.iter().map(|r| r.change_id.as_str()).collect();
        assert_eq!(order, vec!["kmk", "head", "agent", "base"]);
    }

    #[test]
    fn pins_buried_head_line_ahead_of_an_agent_head() {
        // An agent head sorts above HEAD, which has no descendants — HEAD's line
        // (just HEAD) is pulled ahead so HEAD lands on the leftmost lane.
        let mut revs = vec![
            rev("agent", &["base"], false, true, ""),
            rev("head", &["base"], true, false, "HEAD work"),
            rev("base", &[], false, false, "base"),
        ];
        pin_current_wc_first(&mut revs);
        let order: Vec<&str> = revs.iter().map(|r| r.change_id.as_str()).collect();
        assert_eq!(order, vec!["head", "agent", "base"]);
    }

    #[test]
    fn agents_are_anchored_above_their_fork_points() {
        // @ forks from pk; #15 from pk; #7 from ur; #9 from tp. Each agent must be lifted
        // to sit directly above its fork base, with the trunk kept in child→parent order —
        // so every agent folds to a one-row stub and no lane crosses an unrelated node.
        let mut revs = vec![
            rev("head", &["pk"], true, true, ""), // @  (forks from pk)
            rev("a7", &["ur"], false, true, ""),  // #7  (forks from ur)
            rev("a15", &["pk"], false, true, ""), // #15 (forks from pk)
            rev("a9", &["tp"], false, true, ""),  // #9  (forks from tp)
            rev("pk", &["ur"], false, false, "pk"),
            rev("ur", &["tp"], false, false, "ur"),
            rev("tp", &[], false, false, "tp"),
        ];
        order_by_fork_point(&mut revs, &[], &[]);
        let order: Vec<&str> = revs.iter().map(|r| r.change_id.as_str()).collect();
        assert_eq!(order, vec!["head", "a15", "pk", "a7", "ur", "a9", "tp"]);
    }

    #[test]
    fn agents_sharing_a_fork_point_order_by_task_id_descending() {
        // #3, #7 and #12 all fork off the same trunk node (`base`). Whatever order jj
        // log hands them in (recency — the input below is deliberately unsorted), they
        // must stack newest-id-first and stay put regardless of which one is active.
        let mut revs = vec![
            rev("head", &["base"], true, true, ""), // @
            rev("a3", &["base"], false, true, ""),  // #3
            rev("a12", &["base"], false, true, ""), // #12
            rev("a7", &["base"], false, true, ""),  // #7
            rev("base", &[], false, false, "base"),
        ];
        let workspaces = vec![
            Workspace {
                name: "faf-task-3".into(),
                change_id: "a3".into(),
            },
            Workspace {
                name: "faf-task-12".into(),
                change_id: "a12".into(),
            },
            Workspace {
                name: "faf-task-7".into(),
                change_id: "a7".into(),
            },
        ];
        let tasks = vec![
            task(3, "faf-task-3", TaskStatus::Idle),
            task(12, "faf-task-12", TaskStatus::Working),
            task(7, "faf-task-7", TaskStatus::NeedsInput),
        ];
        order_by_fork_point(&mut revs, &workspaces, &tasks);
        let order: Vec<&str> = revs.iter().map(|r| r.change_id.as_str()).collect();
        // HEAD leads on lane 0; then the shared-base agents newest-id-first, then base.
        assert_eq!(order, vec!["head", "a12", "a7", "a3", "base"]);
    }

    #[test]
    fn order_by_fork_point_falls_back_when_a_branch_is_not_off_the_trunk() {
        // `a2` forked off another agent (`a1`), not a trunk node: the flat anchoring
        // doesn't apply, so it falls back to the conservative pin (HEAD's line first,
        // everything else left in place).
        let mut revs = vec![
            rev("head", &["base"], true, true, ""),
            rev("a1", &["base"], false, true, ""),
            rev("a2", &["a1"], false, true, ""), // off an agent, not the trunk
            rev("base", &[], false, false, "base"),
        ];
        order_by_fork_point(&mut revs, &[], &[]);
        let order: Vec<&str> = revs.iter().map(|r| r.change_id.as_str()).collect();
        assert_eq!(order, vec!["head", "a1", "a2", "base"]);
    }

    #[test]
    fn end_to_end_flat_fork_anchored_lanes() {
        // Three agents forked from three different trunk revisions (#15 off pk, #7 off ur,
        // #9 off tp). Ordered → built → rendered, each must fold to a single `├─●` row
        // sitting directly above its fork base, the trunk a clean vertical column.
        let mut revs = vec![
            rev("ymuz", &["pk"], true, true, ""), // @
            rev("qoyp", &["pk"], false, true, ""), // #15
            rev("xwvs", &["ur"], false, true, ""), // #7
            rev("tztq", &["tp"], false, true, ""), // #9
            rev("pk", &["ur"], false, false, "spawn: declare! child-class refs"),
            rev("ur", &["tp"], false, false, "Generate bridge clients from JSON Schema"),
            rev("tp", &["ts"], false, false, "add chatty to examples"),
            rev("ts", &[], false, false, "fix wasm lints"),
        ];
        let workspaces = vec![
            Workspace {
                name: "faf-task-15".into(),
                change_id: "qoyp".into(),
            },
            Workspace {
                name: "faf-task-7".into(),
                change_id: "xwvs".into(),
            },
            Workspace {
                name: "faf-task-9".into(),
                change_id: "tztq".into(),
            },
        ];
        let tasks = vec![
            task(15, "faf-task-15", TaskStatus::Working),
            task(7, "faf-task-7", TaskStatus::NeedsInput),
            task(9, "faf-task-9", TaskStatus::Idle),
        ];

        order_by_fork_point(&mut revs, &workspaces, &tasks);
        let m = build(&revs, &workspaces, &tasks);
        let rows = crate::graph::render(&m.nodes);
        let gutters: Vec<&str> = rows.iter().map(|r| r.gutter.as_str()).collect();
        assert_eq!(
            gutters,
            vec!["@", "├─●", "○", "├─●", "○", "├─●", "○", "○"],
            "trunk stays one clean column; each agent folds to a single ├─● row above its base"
        );
        // Agent rows lead with the ░ empty-fill (these forks are empty), then
        // `#id emoji :: title`. Ids pad to the widest (#15) so the emojis align: #7/#9
        // gain a trailing space.
        assert!(rows[1].content.starts_with("░ #15 ⚙ :: "));
        assert!(rows[3].content.starts_with("░ #7  🔔 :: "));
        assert!(rows[5].content.starts_with("░ #9  ✓ :: "));
        // Trunk rows are the user's own revisions, non-empty (█), shown by description.
        assert_eq!(rows[2].content, "█ spawn: declare! child-class refs");
        assert_eq!(rows[4].content, "█ Generate bridge clients from JSON Schema");
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
        // One line, status as the bare emoji before the `::` title separator.
        assert_eq!(m.nodes[0].lines.len(), 1);
        assert!(m.nodes[0].lines[0].contains("⚙"));
        assert!(m.nodes[0].lines[0].contains(" :: "));
    }

    #[test]
    fn task_node_falls_back_to_prompt_without_a_description() {
        // No jj description yet (e.g. before the agent runs `jj describe`): the row shows
        // the prompt-derived label, untruncated (the render step clips to the pane width).
        let revs = vec![rev("t1", &["p"], false, true, "")];
        let workspaces = vec![Workspace {
            name: "faf-task-1".into(),
            change_id: "t1".into(),
        }];
        let tasks = vec![task(1, "faf-task-1", TaskStatus::Working)];
        let m = build(&revs, &workspaces, &tasks);
        assert_eq!(m.nodes[0].lines[0], "░ #1 ⚙ :: add oauth login");
    }

    #[test]
    fn task_number_is_padded_so_status_emojis_align() {
        // Mixed-width task ids on stacked agent rows: the narrower number is right-padded
        // to the widest id's width, so every status emoji begins at the same column.
        // Without it, "#7 ⚙" and "#12 🔔" put their emojis one column apart.
        let revs = vec![
            rev("t7", &["p"], false, true, ""),
            rev("t12", &["p"], false, true, ""),
            rev("p", &[], false, false, "base"),
        ];
        let workspaces = vec![
            Workspace {
                name: "faf-task-7".into(),
                change_id: "t7".into(),
            },
            Workspace {
                name: "faf-task-12".into(),
                change_id: "t12".into(),
            },
        ];
        let tasks = vec![
            task(7, "faf-task-7", TaskStatus::Working),
            task(12, "faf-task-12", TaskStatus::NeedsInput),
        ];
        let m = build(&revs, &workspaces, &tasks);
        let l7 = &m.nodes[0].lines[0];
        let l12 = &m.nodes[1].lines[0];
        assert_eq!(l7, "░ #7  ⚙ :: add oauth login");
        assert_eq!(l12, "░ #12 🔔 :: add oauth login");
        // The emoji begins at the same char column on both rows.
        assert_eq!(
            l7.chars().position(|c| c == '⚙'),
            l12.chars().position(|c| c == '🔔'),
            "status emojis must align across differing task-number widths"
        );
    }

    #[test]
    fn ordinary_commit_without_a_description_shows_placeholder_not_change_id() {
        // A plain (non-agent, non-wc, non-conflict) commit with no description must show
        // "(no description set)" — never its change id, which is already drawn in the
        // "[abcdefgh]" id column, so repeating it as the label is noise. Non-empty so it
        // doesn't collapse; two-char id keeps the old-fallback substring easy to spot.
        let revs = vec![rev("abcd1234", &["p"], false, false, "")];
        let m = build(&revs, &[], &[]);
        assert_eq!(m.nodes[0].glyph, '○');
        assert_eq!(m.nodes[0].lines, vec!["█ (no description set)"]);
        assert!(
            !m.nodes[0].lines[0].contains("abcd"),
            "the label must not echo the change id"
        );
    }

    #[test]
    fn visible_nodes_carry_an_empty_fill_indicator() {
        // Every visible node leads its label with a fill glyph, right after the id column:
        // █ when the change has content, ░ when it is empty. Collapsed graph-noise nodes
        // never render, so they get none.
        let revs = vec![
            rev("full", &["e"], false, false, "did work"), // non-empty ordinary → █
            rev("e", &["m"], false, true, "empty wip"),    // empty, described (no collapse) → ░
            rev("m", &["a", "b"], false, true, ""),        // empty merge (2 parents, no collapse) → ░
            rev("a", &[], false, false, "a"),
            rev("b", &[], false, false, "b"),
        ];
        let m = build(&revs, &[], &[]);
        assert_eq!(m.nodes[0].lines[0], "█ did work", "non-empty → full block");
        assert_eq!(m.nodes[1].lines[0], "░ empty wip", "empty → light shade");
        assert_eq!(
            m.nodes[2].lines[0], "░ (no description set)",
            "empty merge → light shade, keeps its placeholder label"
        );
    }

    #[test]
    fn conflicted_merge_is_shown_not_collapsed() {
        // A description-less, empty, conflicted merge (e.g. `jj new HEAD agent`) must
        // stay in the graph — collapsing it used to drop a parent and split the branch.
        let mut merge = rev("cm", &["mw", "ag"], false, true, "");
        merge.conflict = true;
        let revs = vec![
            rev("m", &["cm"], true, true, ""), // HEAD @
            merge,                             // the conflicted merge
            rev("mw", &["base"], false, false, "HEAD: work"),
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
    fn conflicted_head_working_copy_is_flagged() {
        let mut wc = rev("m", &["base"], true, true, "");
        wc.conflict = true;
        let m = build(&[wc, rev("base", &[], false, false, "base")], &[], &[]);
        assert_eq!(m.nodes[0].glyph, '@');
        assert!(m.nodes[0].lines[0].contains("conflict"));
    }

    #[test]
    fn head_label_uses_description_else_falls_back() {
        // With a description, the label is exactly that description.
        let described = rev("h", &["p"], true, false, "wire up the bridge");
        assert_eq!(head_label(&described), "wire up the bridge");
        // With none, it falls back to jj's "(no description set)".
        let bare = rev("h", &["p"], true, true, "");
        assert_eq!(head_label(&bare), "(no description set)");
    }

    fn gnode(id: &str, parents: &[&str], glyph: char, lines: &[&str]) -> GraphNode {
        GraphNode {
            change_id: id.into(),
            parents: parents.iter().map(|s| s.to_string()).collect(),
            glyph,
            lines: lines.iter().map(|s| s.to_string()).collect(),
            collapse: false,
        }
    }

    #[test]
    fn combined_head_agent_node_highlights_the_agent_line() {
        // HEAD is parked on the agent's revision: one `@` node with the HEAD header on
        // top and the agent hung beneath. Selecting the task must highlight the agent
        // line (first continuation row), not the shared HEAD header.
        let nodes = vec![
            gnode(
                "x",
                &["base"],
                '@',
                &["(no description set)", "↳ #1 ⚙ :: Add OAuth"],
            ),
            gnode("base", &[], '◆', &["base"]),
        ];
        let node_task = vec![Some(TaskId(1)), None];
        let rows = crate::graph::render(&nodes);
        let rt = row_tasks(&rows, &nodes, &node_task);

        assert_eq!(rows[0].content, "(no description set)");
        assert_eq!(rt[0], None, "the HEAD header row must not carry the task");
        assert_eq!(rows[1].content, "↳ #1 ⚙ :: Add OAuth");
        assert_eq!(rt[1], Some(TaskId(1)), "the agent line carries the task");
        assert_eq!(rt[2], None);
    }

    #[test]
    fn ordinary_agent_node_keeps_the_task_on_its_own_row() {
        // A normal agent branch (glyph ●) forked from the same point as HEAD keeps its
        // task on its own commit row — untouched by the combined-node special case.
        let nodes = vec![
            gnode("h", &["fp"], '@', &["(no description set)"]),
            gnode("a", &["fp"], '●', &["#7 ⚙ :: add-auth"]),
            gnode("fp", &[], '◆', &["base"]),
        ];
        let node_task = vec![None, Some(TaskId(7)), None];
        let rows = crate::graph::render(&nodes);
        let rt = row_tasks(&rows, &nodes, &node_task);

        let agent_row = rows
            .iter()
            .position(|r| r.change_id.as_deref() == Some("a"))
            .unwrap();
        assert_eq!(rt[agent_row], Some(TaskId(7)));
        assert_eq!(
            rt.iter().filter(|t| **t == Some(TaskId(7))).count(),
            1,
            "exactly one row carries the task"
        );
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
