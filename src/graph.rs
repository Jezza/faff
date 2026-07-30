//! Pure DAG → text layout. We build and render our own commit graph (spec §11)
//! rather than consuming jj's rendered graph, so we control multi-line nodes,
//! fork-point collapsing, and the row→node map used for selection.
//!
//! Input nodes are in display order (top→bottom = child→parent, as jj log emits).
//! Output rows are: commit rows (carry a `node_index`), continuation rows (extra
//! content lines of a node), and link rows (the `├─╯` connectors). The renderer
//! handles the shape faf produces — a mostly-linear HEAD trunk with short task
//! branches merging back in — and degrades gracefully on deeper graphs.

use std::collections::HashMap;

/// One node to lay out. `glyph` is drawn at the node's column (e.g. `@`, `●`, `○`).
/// `lines` is its content (≥1 line). `collapse` folds empty fork-points out.
#[derive(Debug, Clone)]
pub struct GraphNode {
    pub change_id: String,
    pub parents: Vec<String>,
    pub glyph: char,
    pub lines: Vec<String>,
    pub collapse: bool,
}

/// One laid-out row. `gutter` is the graph column; `content` the text to the right
/// (empty for link rows). `node_index` is `Some(original_index)` on a node's first row.
/// On that same first row, `change_id` carries the node's id (the caller looks up its
/// display prefix/rest from jj).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphRow {
    pub gutter: String,
    pub content: String,
    pub node_index: Option<usize>,
    pub change_id: Option<String>,
}

/// A node after collapsing: keeps its original index and resolved parents.
struct SNode {
    orig: usize,
    change_id: String,
    glyph: char,
    lines: Vec<String>,
    parents: Vec<String>,
}

/// Render nodes into laid-out rows.
pub fn render(nodes: &[GraphNode]) -> Vec<GraphRow> {
    layout(&splice_collapsed(nodes))
}

/// Remove `collapse` nodes, redirecting references to them onto their nearest
/// non-collapsed ancestor. Parents outside the input set are dropped (bounded revset).
fn splice_collapsed(nodes: &[GraphNode]) -> Vec<SNode> {
    let idx: HashMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.change_id.as_str(), i))
        .collect();

    // Nearest non-collapsed ancestor of `cid` (following first parents), or None.
    let resolve = |start: &str| -> Option<String> {
        let mut cur = start;
        let mut guard = 0;
        loop {
            guard += 1;
            if guard > nodes.len() + 1 {
                return None; // cycle guard (shouldn't happen on a DAG)
            }
            match idx.get(cur) {
                None => return None, // outside the set → drop
                Some(&i) if !nodes[i].collapse => return Some(cur.to_string()),
                Some(&i) => match nodes[i].parents.first() {
                    Some(p) => cur = p.as_str(),
                    None => return None,
                },
            }
        }
    };

    let mut out = Vec::new();
    for (i, n) in nodes.iter().enumerate() {
        if n.collapse {
            continue;
        }
        let mut parents = Vec::new();
        for p in &n.parents {
            if let Some(r) = resolve(p)
                && r != n.change_id
                && !parents.contains(&r)
            {
                parents.push(r);
            }
        }
        out.push(SNode {
            orig: i,
            change_id: n.change_id.clone(),
            glyph: n.glyph,
            lines: if n.lines.is_empty() {
                vec![String::new()]
            } else {
                n.lines.clone()
            },
            parents,
        });
    }
    out
}

fn layout(nodes: &[SNode]) -> Vec<GraphRow> {
    let mut rows = Vec::new();
    // Each lane holds the change_id it is currently flowing toward (a pending parent).
    let mut lanes: Vec<Option<String>> = Vec::new();

    // Reserve lane 0 for the working copy's line (the `@` node = HEAD), so ONLY its
    // line ever occupies the leftmost lane. Pre-seeding lane 0 with HEAD's id means a
    // descendant sitting above HEAD (a task rebased onto it) hits the stub case — its
    // parent, HEAD, is lane 0's pending target — and renders as a branch merging back
    // in, instead of grabbing lane 0 itself. If HEAD is the first node this is a no-op
    // (it just takes lane 0 immediately, no phantom trunk drawn above it).
    if let Some(m) = nodes.iter().find(|n| n.glyph == '@') {
        lanes.push(Some(m.change_id.clone()));
    }

    for n in nodes {
        let incoming: Vec<usize> = lanes
            .iter()
            .enumerate()
            .filter_map(|(j, l)| (l.as_deref() == Some(n.change_id.as_str())).then_some(j))
            .collect();

        // Fold: a branch whose single parent is already targeted by an open lane renders as
        // ONE row — `├─●` — instead of a branch row plus a separate `├─╯` merge row. The `├`
        // sits on the trunk (parent) lane, a short `─` reaches right to the node's glyph, and
        // the branch closes immediately, freeing its lane. So N agents forked from one point
        // render as N single rows stacked directly above their shared base, at a constant two
        // columns. (Extra content lines — rare, now that an agent is one line — hang beneath
        // as connected continuation rows.)
        //
        // This covers two shapes, both merging straight back into the trunk to its left:
        //   * a leaf branch head (nothing flows in) — it borrows a fresh stub lane; and
        //   * a node continuing a single incoming lane sitting right of the trunk — that
        //     lane IS the stub and closes here, so a side branch never runs a parallel lane
        //     down across later forks to a deferred merge (which pushed those forks out to
        //     `├─│─●` and trailed a lone `├─╯`).
        // A node left of its parent's lane, or one with several lanes flowing in, is a real
        // merge — left to the general path below.
        if n.parents.len() == 1
            && incoming.len() <= 1
            && let Some(trunk) = lanes
                .iter()
                .position(|l| l.as_deref() == Some(n.parents[0].as_str()))
            && incoming.first().is_none_or(|&j| j > trunk)
        {
            // The stub lane carrying the node's glyph, always to the right of the trunk so
            // the connector reads left-to-right (`├─●`): reuse the incoming lane when one
            // already flows in, else borrow a free lane right of the trunk.
            let stub = match incoming.first() {
                Some(&j) => {
                    lanes[j] = Some(n.change_id.clone());
                    j
                }
                None => match (trunk + 1..lanes.len()).find(|&j| lanes[j].is_none()) {
                    Some(s) => {
                        lanes[s] = Some(n.change_id.clone());
                        s
                    }
                    None => {
                        lanes.push(Some(n.change_id.clone()));
                        lanes.len() - 1
                    }
                },
            };
            // The folded row carries the node's id + first content line; the branch closes
            // on this same row.
            rows.push(GraphRow {
                gutter: fold_gutter(&lanes, trunk, stub, n.glyph),
                content: n.lines[0].clone(),
                node_index: Some(n.orig),
                change_id: Some(n.change_id.clone()),
            });
            lanes[stub] = None;
            for line in n.lines.iter().skip(1) {
                rows.push(GraphRow {
                    gutter: cont_gutter(&lanes, trunk, true),
                    content: line.clone(),
                    node_index: None,
                    change_id: None,
                });
            }
            while matches!(lanes.last(), Some(None)) {
                lanes.pop();
            }
            continue;
        }

        let col = match incoming.first() {
            Some(&c) => c,
            None => {
                // Branch head: reuse a freed lane, else append.
                match lanes.iter().position(|l| l.is_none()) {
                    Some(s) => {
                        lanes[s] = Some(n.change_id.clone());
                        s
                    }
                    None => {
                        lanes.push(Some(n.change_id.clone()));
                        lanes.len() - 1
                    }
                }
            }
        };
        lanes[col] = Some(n.change_id.clone());

        // Lanes to the right of col that also target this node merge into it.
        let merges: Vec<usize> = incoming.into_iter().filter(|&j| j != col).collect();
        if !merges.is_empty() {
            rows.push(GraphRow {
                gutter: merge_link_row(&lanes, col, &merges),
                content: String::new(),
                node_index: None,
                change_id: None,
            });
            for &j in &merges {
                lanes[j] = None;
            }
        }

        // Commit row (carries the change_id for the id column).
        rows.push(GraphRow {
            gutter: commit_gutter(&lanes, col, n.glyph),
            content: n.lines[0].clone(),
            node_index: Some(n.orig),
            change_id: Some(n.change_id.clone()),
        });

        // Continuation rows for extra content lines — gutter stays connected.
        let has_parent = !n.parents.is_empty();
        for line in n.lines.iter().skip(1) {
            rows.push(GraphRow {
                gutter: cont_gutter(&lanes, col, has_parent),
                content: line.clone(),
                node_index: None,
                change_id: None,
            });
        }

        // Advance lanes to this node's parents.
        match n.parents.first() {
            Some(p0) => {
                lanes[col] = Some(p0.clone());
                for p in n.parents.iter().skip(1) {
                    let exists = lanes.iter().any(|l| l.as_deref() == Some(p.as_str()));
                    if !exists {
                        match lanes.iter().position(|l| l.is_none()) {
                            Some(s) => lanes[s] = Some(p.clone()),
                            None => lanes.push(Some(p.clone())),
                        }
                    }
                }
            }
            None => lanes[col] = None,
        }
        while matches!(lanes.last(), Some(None)) {
            lanes.pop();
        }
    }

    // Birth of HEAD's reserved trunk lane. Pre-seeding lane 0 (above) makes it show a
    // bare `│` on every row above HEAD, which reads as "the trunk continues upward
    // off-screen". It doesn't: the trunk is born where it first does something — the
    // topmost fork merging in, else HEAD's own commit. So blank that phantom `│` on
    // the rows above the birth, and if the birth is a merge, turn its `├` T-junction
    // into a `╭` corner (nothing continues above it). Lower forks keep their `├`, since
    // for them the trunk genuinely continues up to the birth.
    if nodes.iter().any(|n| n.glyph == '@')
        && let Some(birth) = rows.iter().position(|r| !r.gutter.starts_with('│'))
    {
        for r in &mut rows[..birth] {
            if let Some(rest) = r.gutter.strip_prefix('│') {
                r.gutter = format!(" {rest}");
            }
        }
        if let Some(rest) = rows[birth].gutter.strip_prefix('├') {
            rows[birth].gutter = format!("╭{rest}");
        }
    }
    rows
}

fn gutter_width(nlanes: usize) -> usize {
    if nlanes == 0 { 1 } else { 2 * nlanes - 1 }
}

fn commit_gutter(lanes: &[Option<String>], col: usize, glyph: char) -> String {
    let mut cells = vec![' '; gutter_width(lanes.len())];
    for (j, lane) in lanes.iter().enumerate() {
        cells[2 * j] = if j == col {
            glyph
        } else if lane.is_some() {
            '│'
        } else {
            ' '
        };
    }
    trim_end(cells)
}

fn cont_gutter(lanes: &[Option<String>], col: usize, has_parent: bool) -> String {
    let mut cells = vec![' '; gutter_width(lanes.len())];
    for (j, lane) in lanes.iter().enumerate() {
        cells[2 * j] = if j == col {
            if has_parent { '│' } else { ' ' }
        } else if lane.is_some() {
            '│'
        } else {
            ' '
        };
    }
    trim_end(cells)
}

fn merge_link_row(lanes: &[Option<String>], col: usize, merges: &[usize]) -> String {
    let mut cells = vec![' '; gutter_width(lanes.len())];
    for (j, lane) in lanes.iter().enumerate() {
        cells[2 * j] = if j == col {
            '├'
        } else if merges.contains(&j) {
            '╯'
        } else if lane.is_some() {
            '│'
        } else {
            ' '
        };
    }
    // Horizontal fill from col to the furthest merging lane.
    let max_merge = *merges.iter().max().unwrap();
    for cell in cells.iter_mut().take(2 * max_merge).skip(2 * col + 1) {
        if *cell == ' ' {
            *cell = '─';
        }
    }
    trim_end(cells)
}

/// Gutter for a folded branch: `├` on the trunk lane, the node's `glyph` on the (right-of-
/// trunk) `stub` lane, joined by `─`. One row does the work of a branch row plus its merge,
/// so an agent anchored to its fork point renders as a single `├─●`.
fn fold_gutter(lanes: &[Option<String>], trunk: usize, stub: usize, glyph: char) -> String {
    let mut cells = vec![' '; gutter_width(lanes.len())];
    for (j, lane) in lanes.iter().enumerate() {
        cells[2 * j] = if j == trunk {
            '├'
        } else if j == stub {
            glyph
        } else if lane.is_some() {
            '│'
        } else {
            ' '
        };
    }
    // Horizontal fill between the trunk tee and the glyph (stub is always right of trunk).
    for cell in cells.iter_mut().take(2 * stub).skip(2 * trunk + 1) {
        if *cell == ' ' {
            *cell = '─';
        }
    }
    trim_end(cells)
}

fn trim_end(cells: Vec<char>) -> String {
    let s: String = cells.into_iter().collect();
    s.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, parents: &[&str], glyph: char, lines: &[&str]) -> GraphNode {
        GraphNode {
            change_id: id.into(),
            parents: parents.iter().map(|s| s.to_string()).collect(),
            glyph,
            lines: lines.iter().map(|s| s.to_string()).collect(),
            collapse: false,
        }
    }

    fn gutters(rows: &[GraphRow]) -> Vec<String> {
        rows.iter().map(|r| r.gutter.clone()).collect()
    }

    #[test]
    fn linear_chain() {
        let nodes = vec![
            node("c", &["b"], '@', &["head"]),
            node("b", &["a"], '●', &["mid"]),
            node("a", &[], '◆', &["root"]),
        ];
        let rows = render(&nodes);
        assert_eq!(gutters(&rows), vec!["@", "●", "◆"]);
        assert_eq!(rows[0].content, "head");
        assert_eq!(
            rows.iter().map(|r| r.node_index).collect::<Vec<_>>(),
            vec![Some(0), Some(1), Some(2)]
        );
    }

    #[test]
    fn one_branch_merges_back_into_trunk() {
        // HEAD(ovmp)->yvvy ; task(kysl)->yvvy ; yvvy->zmpy ; zmpy(root)
        let nodes = vec![
            node("ovmp", &["yvvy"], '@', &["HEAD"]),
            node("kysl", &["yvvy"], '○', &["task"]),
            node("yvvy", &["zmpy"], '●', &["fork base"]),
            node("zmpy", &[], '◆', &["root"]),
        ];
        let rows = render(&nodes);
        assert_eq!(
            gutters(&rows),
            vec!["@", "├─○", "●", "◆"],
            "branch folds to a single ├─○ row anchored above its base"
        );
        // every commit row carries its node index; the fold row is itself a commit row
        let idx: Vec<_> = rows.iter().map(|r| r.node_index).collect();
        assert_eq!(idx, vec![Some(0), Some(1), Some(2), Some(3)]);
    }

    #[test]
    fn multiline_node_keeps_gutter_connected() {
        let nodes = vec![
            node("ovmp", &["yvvy"], '@', &["HEAD"]),
            node("kysl", &["yvvy"], '●', &["#7 add-auth", "⚙ working 2m"]),
            node("yvvy", &[], '◆', &["root"]),
        ];
        let rows = render(&nodes);
        // The branch folds to a single `├─●` row; an extra content line hangs beneath it
        // as a connected continuation row on the trunk.
        assert_eq!(gutters(&rows), vec!["@", "├─●", "│", "◆"]);
        assert_eq!(rows[1].content, "#7 add-auth");
        assert_eq!(rows[1].node_index, Some(1));
        assert_eq!(rows[2].content, "⚙ working 2m");
        assert_eq!(rows[2].node_index, None);
    }

    #[test]
    fn siblings_from_one_fork_point_each_fold_to_one_row() {
        // HEAD @ plus three agents all forked from the same point `fp`. Each folds to a
        // single `├─●` row stacked above the fork point — the graph stays two columns wide
        // however many agents there are, one row apiece.
        let nodes = vec![
            node("m", &["fp"], '@', &["(no description set)"]),
            node("a16", &["fp"], '●', &["#16 ⚙ :: migrate"]),
            node("a15", &["fp"], '●', &["#15 ⚙ :: startup"]),
            node("a3", &["fp"], '●', &["#3 ⚙ :: macros"]),
            node("fp", &["base"], '●', &["fork base"]),
            node("base", &[], '◆', &["base"]),
        ];
        let rows = render(&nodes);
        assert_eq!(
            gutters(&rows),
            vec![
                "@",   // HEAD trunk
                "├─●", // agent 16
                "├─●", // agent 15
                "├─●", // agent 3
                "●",   // fork point
                "◆",   // base
            ]
        );
        assert_eq!(rows[1].content, "#16 ⚙ :: migrate");
        assert_eq!(rows[1].node_index, Some(1));
        // Commit rows still map to the original node indices, in order.
        let idx: Vec<_> = rows.iter().filter_map(|r| r.node_index).collect();
        assert_eq!(idx, vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn collapsed_fork_point_is_removed_and_branch_reattaches() {
        // Empty fork-point `m` (collapse) between children and real parent `p`.
        // HEAD(mp)->m ; task(t)->m ; m(empty,collapse)->p ; p(root)
        let nodes = vec![
            node("mp", &["m"], '@', &["HEAD"]),
            node("t", &["m"], '○', &["task"]),
            GraphNode {
                collapse: true,
                ..node("m", &["p"], '●', &["fork"])
            },
            node("p", &[], '◆', &["real base"]),
        ];
        let rows = render(&nodes);
        // `m` row is gone; the task branch folds to a single row anchored at `p`.
        assert_eq!(gutters(&rows), vec!["@", "├─○", "◆"]);
        // no row references the collapsed node (orig index 2)
        assert!(rows.iter().all(|r| r.node_index != Some(2)));
        // remaining commit rows map to originals 0,1,3
        let idx: Vec<_> = rows.iter().filter_map(|r| r.node_index).collect();
        assert_eq!(idx, vec![0, 1, 3]);
    }

    #[test]
    fn freed_lane_is_reused_by_later_branch() {
        // Two tasks off the trunk at different points should both use lane 1.
        let nodes = vec![
            node("m2", &["m1"], '@', &["HEAD@"]),
            node("t2", &["m1"], '○', &["task2"]),
            node("m1", &["m0"], '●', &["HEAD work"]),
            node("t1", &["m0"], '○', &["task1"]),
            node("m0", &[], '◆', &["base"]),
        ];
        let rows = render(&nodes);
        assert_eq!(gutters(&rows), vec!["@", "├─○", "●", "├─○", "◆"]);
    }

    #[test]
    fn descendant_above_head_is_a_branch_off_lane_zero() {
        // `kmk` was rebased onto HEAD, so HEAD is its parent. With HEAD's line
        // pinned first (kmk above HEAD), lane 0 is reserved for HEAD (`@`): kmk must
        // render as a branch on lane 1 merging into HEAD, NOT take lane 0 itself. A
        // separate agent off `base` still stubs below.
        let nodes = vec![
            node("kmk", &["head"], '○', &["#3 kmk"]),
            node("head", &["base"], '@', &["(no description set)"]),
            node("agent", &["base"], '○', &["#5 agent"]),
            node("base", &[], '◆', &["base"]),
        ];
        let rows = render(&nodes);
        assert_eq!(
            gutters(&rows),
            vec![
                "╭─○", // kmk folds above HEAD; trunk born here (corner, nothing above)
                "@",   // HEAD alone on lane 0
                "├─○", // agent folds off base (trunk continues above it → ├)
                "◆",   // base
            ]
        );
        assert_eq!(rows[0].content, "#3 kmk");
        assert_eq!(rows[0].node_index, Some(0));
        assert_eq!(rows[1].content, "(no description set)");
    }

    #[test]
    fn top_fork_corners_and_lower_fork_keeps_the_junction() {
        // Two tasks rebased onto HEAD. The trunk is born at the topmost merge (`╭─╯`,
        // nothing above it); the lower fork keeps its `├─╯` because the trunk genuinely
        // continues up to that birth.
        let nodes = vec![
            node("kmk", &["head"], '○', &["#3 kmk"]),
            node("kmk2", &["head"], '○', &["#4 kmk2"]),
            node("head", &["base"], '@', &["(no description set)"]),
            node("base", &[], '◆', &["base"]),
        ];
        let rows = render(&nodes);
        assert_eq!(gutters(&rows), vec!["╭─○", "├─○", "@", "◆"]);
    }

    #[test]
    fn empty_input_is_empty() {
        assert!(render(&[]).is_empty());
    }

    #[test]
    fn branch_merging_back_folds_instead_of_holding_a_lane_across_later_forks() {
        // A side branch (`s`) merges back into the trunk node `base`, but three agents fork
        // off `base` between `s`'s row and `base`'s. `s` has a child (`c`) sitting above it,
        // so a lane already flows into it — it isn't a leaf branch head. It must still fold
        // to a single `├─○` row that cuts straight back to the trunk, freeing its lane, so
        // the three agents fold to plain `├─●` (not `├─│─●`) and no deferred `├─╯` merge row
        // trails the agents. Mirrors the screenshot: task forked off a non-trunk side commit.
        let nodes = vec![
            node("head", &["base"], '@', &["(no description set)"]),
            node("c", &["s"], '●', &["#30 :: new task"]),
            node("s", &["base"], '○', &["(no description set)"]),
            node("a28", &["base"], '●', &["#28 :: evt"]),
            node("a29", &["base"], '●', &["#29 :: sorg http"]),
            node("a27", &["base"], '●', &["#27 :: app-specs"]),
            node("base", &[], '○', &["Convert timers"]),
        ];
        assert_eq!(
            gutters(&render(&nodes)),
            vec!["@", "│ ●", "├─○", "├─●", "├─●", "├─●", "○"],
        );
    }
}
