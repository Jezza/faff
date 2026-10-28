//! The log view's own state, shared by faff's Tab log body and `faff logs`: which
//! fork/merge pairs are folded, the fuzzy search and the context kept around its hits, and
//! the revision selected. Pure — it turns the engine's nodes into laid-out rows — so it is
//! tested without jj.
//!
//! A *pair* is a merge whose side parent is a plain linear branch that forked off the
//! mainline (the merge's first parent, or one of its ancestors) and has nothing else
//! hanging off it: the "fork, then merge back" shape a merge-based workflow leaves behind.
//! Folded, the whole pair reads as the merge alone, with a `(+N)` for the side commits.

use super::revops::Found;
use crate::domain::TaskId;
use crate::graph::{self, GraphNode, GraphRow};
use crate::jj::{MatchKind, RevInfo, Workspace};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32Str};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

/// Context kept around each hit when a search starts.
pub const DEFAULT_CONTEXT: usize = 1;

/// A hit must score at least this percentage of the best hit. Fuzzy matching finds a
/// query's letters scattered through almost any long description; this keeps the hits to
/// those that match about as well as the best one does. (`lanes` scores 140 against
/// "Nest graph lanes" but 58–75 scattered through other descriptions.) A `'` before the
/// query (nucleo's syntax) asks for it verbatim.
const HIT_PERCENT: u32 = 60;

/// Which part of a revision a search matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// The row's own text: the description's first line, or an agent's `#7 … :: title`.
    Row,
    /// A bookmark (local or remote) or a workspace name.
    Mark,
    Author,
    ChangeId,
    CommitId,
    Email,
    /// The whole description, body included.
    Description,
    /// A path the revision changed (found by jj, not the fuzzy match).
    File,
    /// A line the revision added or removed (likewise).
    Line,
}

impl Field {
    /// What a snippet of this field is labelled with when the row doesn't show it.
    pub fn label(self) -> &'static str {
        match self {
            Field::Row => "row",
            Field::Mark => "bookmark",
            Field::Author => "author",
            Field::ChangeId => "change",
            Field::CommitId => "commit",
            Field::Email => "email",
            Field::Description => "desc",
            Field::File => "file",
            Field::Line => "line",
        }
    }

    /// The icon a row found in its files carries, before its description.
    pub fn icon(self) -> Option<char> {
        match self {
            Field::File => Some('▤'),
            Field::Line => Some('±'),
            _ => None,
        }
    }
}

/// Shortest query searched for in the files: shorter ones match nearly every revision.
pub const FILE_QUERY_MIN: usize = 3;

/// What a search query asks jj to find in the files: the query without the fuzzy
/// matcher's own syntax (a leading `'` or `^`, a trailing `$`), once it's long enough and
/// not a negation.
pub fn file_query(query: &str) -> Option<String> {
    let q = query.trim();
    let q = q
        .strip_prefix('\'')
        .or_else(|| q.strip_prefix('^'))
        .unwrap_or(q);
    let q = q.strip_suffix('$').unwrap_or(q);
    (q.chars().count() >= FILE_QUERY_MIN && !q.starts_with('!')).then(|| q.to_string())
}

/// Where a hit matched: per field, its text and the matched character positions in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub parts: Vec<HitPart>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HitPart {
    pub field: Field,
    pub text: String,
    /// Sorted, deduplicated character positions in `text`.
    pub indices: Vec<usize>,
}

impl Hit {
    /// The matched positions in `text` when the hit matched `field` with exactly that text.
    pub fn indices_in(&self, field: Field, text: &str) -> Option<&[usize]> {
        self.parts
            .iter()
            .find(|p| p.field == field && p.text == text)
            .map(|p| p.indices.as_slice())
    }
}

/// A folded merge's node glyph, so a fold reads at the node rather than only at the `(+N)`
/// after its description. Your `@` and a conflict keep theirs.
pub const FOLDED_GLYPH: char = '⊕';

/// A fork/merge pair: the merge's index and its side commits' (newest first).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pair {
    merge: usize,
    side: Vec<usize>,
}

/// The search line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Search {
    pub query: String,
    /// Keys go into the query (until Enter locks it).
    pub typing: bool,
    /// Revisions kept above and below each hit.
    pub context: usize,
}

/// A run of revisions the search hides, drawn as one `~ N hidden ~` line before row `at`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gap {
    pub at: usize,
    pub count: usize,
}

#[derive(Default)]
pub struct LogView {
    nodes: Vec<GraphNode>,
    node_task: Vec<Option<TaskId>>,
    /// What each node is searched by, field by field, parallel to `nodes`.
    haystacks: Vec<Vec<(Field, String)>>,
    pairs: Vec<Pair>,
    /// Merges opened with `→`.
    expanded: HashSet<String>,
    pub selected: Option<String>,
    /// What jj found in the files for a search: hits alongside the fuzzy ones while its
    /// query is the search's.
    pub found: Option<Found>,
    /// Older history is left to load, so `↓` on the last row stays put rather than
    /// wrapping to the top.
    pub more: bool,
    search: Option<Search>,
    matcher: RefCell<Matcher>,

    // Derived by `layout`.
    rows: Vec<GraphRow>,
    row_task: Vec<Option<TaskId>>,
    gaps: Vec<Gap>,
    /// Matching revisions, in display order.
    hits: Vec<String>,
    /// The best-scoring hit, where a new query lands the selection.
    best: Option<String>,
    /// What each hit matched, by change id.
    matched: HashMap<String, Hit>,
    /// Folded merges and how many side commits each hides.
    folded: HashMap<String, usize>,
}

impl LogView {
    /// Take a fresh model from the engine: the log's nodes (jj order), the task on each,
    /// every revision's details by change id, and the workspaces.
    pub fn set_model(
        &mut self,
        nodes: Vec<GraphNode>,
        node_task: Vec<Option<TaskId>>,
        meta: &HashMap<String, RevInfo>,
        workspaces: &[Workspace],
    ) {
        let ws_at: HashSet<&str> = workspaces.iter().map(|w| w.change_id.as_str()).collect();
        let protected: Vec<bool> = nodes
            .iter()
            .enumerate()
            .map(|(i, n)| {
                n.glyph == '@'
                    || ws_at.contains(n.change_id.as_str())
                    || node_task.get(i).copied().flatten().is_some()
                    || meta
                        .get(&n.change_id)
                        .is_some_and(|r| !r.bookmarks.is_empty())
            })
            .collect();
        self.haystacks = nodes
            .iter()
            .map(|n| haystack(n, meta.get(&n.change_id), workspaces))
            .collect();
        self.pairs = find_pairs(&nodes, &protected);
        self.nodes = nodes;
        self.node_task = node_task;
        self.layout();
    }

    pub fn rows(&self) -> &[GraphRow] {
        &self.rows
    }

    pub fn row_task(&self) -> &[Option<TaskId>] {
        &self.row_task
    }

    pub fn gaps(&self) -> &[Gap] {
        &self.gaps
    }

    pub fn search(&self) -> Option<&Search> {
        self.search.as_ref()
    }

    pub fn hits(&self) -> &[String] {
        &self.hits
    }

    /// Side commits a folded merge hides, for its `(+N)`.
    pub fn folded(&self, change_id: &str) -> Option<usize> {
        self.folded.get(change_id).copied()
    }

    /// The loaded node for `change_id`, if any.
    pub fn node(&self, change_id: &str) -> Option<&GraphNode> {
        self.nodes.iter().find(|n| n.change_id == change_id)
    }

    /// Whether `change_id` is loaded at all (shown or not).
    pub fn has(&self, change_id: &str) -> bool {
        self.node(change_id).is_some()
    }

    /// The selectable revisions, top to bottom: one per node row.
    pub fn ids(&self) -> Vec<&str> {
        self.rows
            .iter()
            .filter_map(|r| r.change_id.as_deref())
            .collect()
    }

    fn selected_pos(&self) -> Option<usize> {
        let sel = self.selected.as_deref()?;
        self.ids().iter().position(|id| *id == sel)
    }

    /// Keep the selection on a revision that's showing: the same one when it still is,
    /// else whatever now sits where it was, else `fallback` (the working copy on a first
    /// view), else the top.
    pub fn settle_selection(&mut self, prev_pos: Option<usize>, fallback: Option<&str>) {
        let ids: Vec<String> = self.ids().into_iter().map(str::to_string).collect();
        if self.selected.as_ref().is_some_and(|s| ids.contains(s)) {
            return;
        }
        self.selected = match prev_pos {
            Some(p) if self.selected.is_some() => {
                ids.get(p.min(ids.len().saturating_sub(1))).cloned()
            }
            _ => fallback
                .filter(|f| ids.iter().any(|i| i == f))
                .map(str::to_string)
                .or_else(|| ids.first().cloned()),
        };
    }

    /// The selected revision's position among the showing ones (to settle by after a
    /// refresh).
    pub fn position(&self) -> Option<usize> {
        self.selected_pos()
    }

    /// `↑`/`↓`: a row at a time, or a hit at a time while a search is filtering.
    pub fn step(&mut self, delta: isize) {
        if self.filtering() {
            self.step_hits(delta);
            return;
        }
        let ids = self.ids();
        if ids.is_empty() {
            return;
        }
        let n = ids.len() as isize;
        let cur = self.selected_pos().unwrap_or(0) as isize;
        let to = if self.more && cur + delta >= n {
            n - 1
        } else {
            (cur + delta).rem_euclid(n)
        };
        self.selected = Some(ids[to as usize].to_string());
    }

    fn step_hits(&mut self, delta: isize) {
        if self.hits.is_empty() {
            return;
        }
        let ids = self.ids();
        let pos_of = |id: &str| ids.iter().position(|x| *x == id);
        let cur = self.selected_pos();
        let next = match (cur, delta.signum()) {
            (Some(c), 1) => self.hits.iter().find(|h| pos_of(h).is_some_and(|p| p > c)),
            (Some(c), -1) => self
                .hits
                .iter()
                .rev()
                .find(|h| pos_of(h).is_some_and(|p| p < c)),
            _ => None,
        };
        // Past either end, wrap around, as row-by-row movement does.
        let next = next.or(if delta > 0 {
            self.hits.first()
        } else {
            self.hits.last()
        });
        self.selected = next.cloned();
    }

    // ---- folding ----

    fn pair_of_merge(&self, change_id: &str) -> Option<&Pair> {
        self.pairs
            .iter()
            .find(|p| self.nodes[p.merge].change_id == change_id)
    }

    fn pair_of_side(&self, change_id: &str) -> Option<&Pair> {
        self.pairs
            .iter()
            .find(|p| p.side.iter().any(|&s| self.nodes[s].change_id == change_id))
    }

    /// `→`: open the selected folded merge. Returns whether anything changed.
    pub fn expand(&mut self) -> bool {
        let Some(sel) = self.selected.clone() else {
            return false;
        };
        if !self.folded.contains_key(&sel) {
            return false;
        }
        self.expanded.insert(sel);
        self.layout();
        true
    }

    /// `←`: fold the selected merge, or the pair the selected side commit belongs to
    /// (selecting its merge). A pair a search hit holds open stays open.
    pub fn collapse(&mut self) -> bool {
        let Some(sel) = self.selected.clone() else {
            return false;
        };
        let merge = match self.pair_of_merge(&sel).or_else(|| self.pair_of_side(&sel)) {
            Some(p) => self.nodes[p.merge].change_id.clone(),
            None => return false,
        };
        if !self.expanded.remove(&merge) {
            return false;
        }
        self.layout();
        if self.ids().contains(&merge.as_str()) {
            self.selected = Some(merge);
        }
        true
    }

    // ---- search ----

    /// Whether a search is narrowing what's shown.
    pub fn filtering(&self) -> bool {
        self.search.as_ref().is_some_and(|s| !s.query.is_empty())
    }

    /// `/`: start a search, or go back to editing the current one.
    pub fn open_search(&mut self) {
        match &mut self.search {
            Some(s) => s.typing = true,
            None => {
                self.search = Some(Search {
                    query: String::new(),
                    typing: true,
                    context: DEFAULT_CONTEXT,
                });
            }
        }
    }

    /// Enter: stop typing, keep the filter.
    pub fn lock_search(&mut self) {
        if let Some(s) = &mut self.search {
            s.typing = false;
        }
        if !self.filtering() {
            self.clear_search();
        }
    }

    /// Esc: drop the search and show everything again, keeping the selection — and the
    /// pair it sits in open, so the revision found stays in view.
    pub fn clear_search(&mut self) {
        self.search = None;
        if let Some(merge) = self
            .selected
            .as_deref()
            .and_then(|s| self.pair_of_side(s))
            .map(|p| self.nodes[p.merge].change_id.clone())
        {
            self.expanded.insert(merge);
        }
        self.layout();
    }

    /// The query changed: re-filter and land on the best hit.
    pub fn set_query(&mut self, query: String) {
        let Some(s) = &mut self.search else { return };
        s.query = query;
        self.layout();
        if let Some(best) = self.best.clone() {
            self.selected = Some(best);
        }
    }

    /// While filtering, move a selection that isn't on a hit onto the best one.
    pub fn land_on_hit(&mut self) {
        if !self.filtering()
            || self
                .selected
                .as_ref()
                .is_some_and(|s| self.hits.contains(s))
        {
            return;
        }
        if let Some(best) = self.best.clone() {
            self.selected = Some(best);
        }
    }

    /// `+`/`-`: widen or narrow the context around each hit.
    pub fn adjust_context(&mut self, delta: isize) {
        if let Some(s) = &mut self.search {
            s.context = s.context.saturating_add_signed(delta);
            self.layout();
        }
    }

    fn pattern(&self) -> Option<Pattern> {
        let s = self.search.as_ref()?;
        if s.query.is_empty() {
            return None;
        }
        Some(Pattern::parse(
            &s.query,
            CaseMatching::Smart,
            Normalization::Smart,
        ))
    }

    /// What the search matched in revision `change_id`, when it's a hit.
    pub fn hit(&self, change_id: &str) -> Option<&Hit> {
        self.matched.get(change_id)
    }

    /// Score `items` against the current query, best first, keeping only those that match
    /// about as well as the best one. `faff logs` uses it to find bookmarks to pull in.
    pub fn rank<'a, T>(&self, items: impl IntoIterator<Item = (&'a str, T)>) -> Vec<T> {
        let Some(p) = self.pattern() else {
            return Vec::new();
        };
        let mut matcher = self.matcher.borrow_mut();
        let mut buf = Vec::new();
        let mut scored: Vec<(u32, T)> = items
            .into_iter()
            .filter_map(|(text, item)| {
                p.score(Utf32Str::new(text, &mut buf), &mut matcher)
                    .map(|s| (s, item))
            })
            .collect();
        let best = scored.iter().map(|(s, _)| *s).max().unwrap_or(0);
        scored.retain(|(s, _)| *s * 100 >= best * HIT_PERCENT);
        scored.sort_by_key(|(s, _)| std::cmp::Reverse(*s));
        scored.into_iter().map(|(_, t)| t).collect()
    }

    // ---- layout ----

    /// Rebuild the rows from the nodes, the folds and the search.
    fn layout(&mut self) {
        // Hits come first: a hit inside a pair holds that pair open.
        let mut scores: Vec<Option<u32>> = vec![None; self.nodes.len()];
        let mut found: Vec<Option<Hit>> = vec![None; self.nodes.len()];
        if let Some(p) = self.pattern() {
            let mut matcher = self.matcher.borrow_mut();
            for (i, fields) in self.haystacks.iter().enumerate() {
                if let Some((score, hit)) = match_fields(&p, fields, &mut matcher) {
                    scores[i] = Some(score);
                    found[i] = Some(hit);
                }
            }
            let best = scores.iter().flatten().copied().max().unwrap_or(0);
            for s in &mut scores {
                if s.is_some_and(|v| v * 100 < best * HIT_PERCENT) {
                    *s = None;
                }
            }
            // jj's finds are hits too, below every fuzzy one for where a new query lands.
            for (i, part) in self.file_parts() {
                if scores[i].is_none() {
                    // Not a fuzzy hit: drop whatever it matched below the bar.
                    scores[i] = Some(0);
                    found[i] = None;
                }
                found[i]
                    .get_or_insert_with(|| Hit { parts: Vec::new() })
                    .parts
                    .push(part);
            }
        }
        self.matched = self
            .nodes
            .iter()
            .zip(found)
            .zip(&scores)
            .filter(|(_, s)| s.is_some())
            .filter_map(|((n, h), _)| Some((n.change_id.clone(), h?)))
            .collect();
        let is_hit = |i: usize| scores[i].is_some();
        let filtering = self.filtering();

        // Which pairs fold, and which nodes that hides.
        let mut gone = vec![false; self.nodes.len()];
        let mut trim: HashMap<usize, usize> = HashMap::new(); // merge -> its kept parent count
        self.folded.clear();
        for p in &self.pairs {
            let id = &self.nodes[p.merge].change_id;
            let held_open = filtering && p.side.iter().any(|&s| is_hit(s));
            if self.expanded.contains(id) || held_open {
                continue;
            }
            for &s in &p.side {
                gone[s] = true;
            }
            trim.insert(p.merge, 1);
            self.folded.insert(id.clone(), p.side.len());
        }

        // Display order, and which of it the search keeps.
        let display: Vec<usize> = (0..self.nodes.len()).filter(|&i| !gone[i]).collect();
        let mut keep = vec![!filtering; display.len()];
        if filtering {
            let ctx = self.search.as_ref().map_or(0, |s| s.context);
            for (pos, &i) in display.iter().enumerate() {
                if is_hit(i) {
                    let lo = pos.saturating_sub(ctx);
                    let hi = (pos + ctx).min(display.len() - 1);
                    keep[lo..=hi].iter_mut().for_each(|k| *k = true);
                }
            }
        }
        let mut shown = vec![false; self.nodes.len()];
        for (pos, &i) in display.iter().enumerate() {
            shown[i] = keep[pos];
        }

        // The nodes the graph lays out: folded side commits dropped outright (their merge
        // forgets them), filtered-out ones collapsed so their neighbours reconnect.
        let mut nodes = Vec::with_capacity(display.len());
        let mut tasks = Vec::with_capacity(display.len());
        for &i in &display {
            let mut n = self.nodes[i].clone();
            if let Some(&k) = trim.get(&i) {
                n.parents.truncate(k);
                if !matches!(n.glyph, '@' | '×') {
                    n.glyph = FOLDED_GLYPH;
                }
            }
            n.collapse = !shown[i];
            nodes.push(n);
            tasks.push(self.node_task[i]);
        }
        let rows = graph::render(&nodes);
        let row_task = super::model::row_tasks(&rows, &nodes, &tasks);

        // `~ N hidden ~` lines wherever the search skipped a run of the display order.
        let mut gaps = Vec::new();
        let mut last: Option<usize> = None;
        for (r, row) in rows.iter().enumerate() {
            let Some(ni) = row.node_index else { continue };
            let skipped = match last {
                Some(l) => ni - l - 1,
                None => ni,
            };
            if skipped > 0 {
                gaps.push(Gap {
                    at: r,
                    count: skipped,
                });
            }
            last = Some(ni);
        }
        let tail = match last {
            Some(l) => display.len() - l - 1,
            None => display.len(),
        };
        if filtering && tail > 0 {
            gaps.push(Gap {
                at: rows.len(),
                count: tail,
            });
        }

        self.hits = display
            .iter()
            .filter(|&&i| is_hit(i))
            .map(|&i| self.nodes[i].change_id.clone())
            .collect();
        self.best = display
            .iter()
            .filter_map(|&i| scores[i].map(|s| (s, i)))
            // Highest score; the top-most among equals.
            .max_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)))
            .map(|(_, i)| self.nodes[i].change_id.clone());
        self.rows = rows;
        self.row_task = row_task;
        self.gaps = gaps;
    }
}

impl LogView {
    /// The hit part each loaded node gets from what jj found in the files, when that's for
    /// the current query.
    fn file_parts(&self) -> Vec<(usize, HitPart)> {
        let (Some(found), Some(query)) = (
            &self.found,
            self.search.as_ref().and_then(|s| file_query(&s.query)),
        ) else {
            return Vec::new();
        };
        if found.query != query {
            return Vec::new();
        }
        let lower: Vec<char> = query.to_lowercase().chars().collect();
        let mut parts = Vec::new();
        for m in &found.matches {
            let Some(i) = self.nodes.iter().position(|n| n.change_id == m.change_id) else {
                continue;
            };
            let field = match m.kind {
                MatchKind::Path => Field::File,
                MatchKind::Line => Field::Line,
            };
            parts.push((
                i,
                HitPart {
                    field,
                    indices: find_ignoring_case(&m.text, &lower),
                    text: m.text.clone(),
                },
            ));
        }
        parts
    }
}

/// The character positions of the first place `text` contains `lower` (a lowercase
/// needle), ignoring case.
fn find_ignoring_case(text: &str, lower: &[char]) -> Vec<usize> {
    let chars: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    // Lowercasing can change a character's length; fall back to no highlight then.
    if chars.len() != text.chars().count() || lower.is_empty() {
        return Vec::new();
    }
    chars
        .windows(lower.len())
        .position(|w| w == lower)
        .map(|at| (at..at + lower.len()).collect())
        .unwrap_or_default()
}

/// Everything a revision is searched by, field by field: its row text, bookmarks and the
/// workspaces sitting on it, author, ids, email and whole description. The fields the row
/// shows come first, so a tie goes to one the user can see.
fn haystack(
    node: &GraphNode,
    rev: Option<&RevInfo>,
    workspaces: &[Workspace],
) -> Vec<(Field, String)> {
    let mut fields: Vec<(Field, String)> =
        node.lines.iter().map(|l| (Field::Row, l.clone())).collect();
    if let Some(r) = rev {
        fields.extend(r.bookmarks.iter().map(|b| (Field::Mark, b.clone())));
        fields.extend(r.remote_bookmarks.iter().map(|b| (Field::Mark, b.clone())));
    }
    fields.extend(
        workspaces
            .iter()
            .filter(|w| w.change_id == node.change_id)
            .map(|w| (Field::Mark, w.name.clone())),
    );
    if let Some(r) = rev {
        fields.push((Field::Author, author(r)));
    }
    fields.push((Field::ChangeId, node.change_id.clone()));
    if let Some(r) = rev {
        fields.push((Field::CommitId, r.commit_id.clone()));
        fields.push((Field::Email, r.email.clone()));
        fields.push((Field::Description, r.full_description.clone()));
    }
    fields.retain(|(_, t)| !t.is_empty());
    fields
}

/// Match `pattern` against a revision's fields. Each word of the query matches within one
/// field — the best-scoring one, the first among equals — so a fuzzy match never strings
/// letters together across fields. Every word must match somewhere, and a negated word
/// (`!foo`) nowhere. The score is the words' total.
fn match_fields(
    pattern: &Pattern,
    fields: &[(Field, String)],
    matcher: &mut Matcher,
) -> Option<(u32, Hit)> {
    let mut buf = Vec::new();
    let mut idx = Vec::new();
    let mut total = 0u32;
    let mut parts: Vec<HitPart> = Vec::new();
    for atom in &pattern.atoms {
        if atom.negative {
            let clear = fields
                .iter()
                .all(|(_, t)| atom.score(Utf32Str::new(t, &mut buf), matcher).is_some());
            if !clear {
                return None;
            }
            continue;
        }
        let mut best: Option<(u16, usize, Vec<u32>)> = None;
        for (f, (_, text)) in fields.iter().enumerate() {
            idx.clear();
            if let Some(score) = atom.indices(Utf32Str::new(text, &mut buf), matcher, &mut idx)
                && best.as_ref().is_none_or(|b| score > b.0)
            {
                best = Some((score, f, idx.clone()));
            }
        }
        let (score, f, found) = best?;
        total += u32::from(score);
        let (field, text) = &fields[f];
        let part = match parts
            .iter_mut()
            .find(|p| p.field == *field && p.text == *text)
        {
            Some(p) => p,
            None => {
                parts.push(HitPart {
                    field: *field,
                    text: text.clone(),
                    indices: Vec::new(),
                });
                parts.last_mut().unwrap()
            }
        };
        part.indices.extend(found.iter().map(|&i| i as usize));
        part.indices.sort_unstable();
        part.indices.dedup();
    }
    Some((total, Hit { parts }))
}

/// A one-line window onto `part`'s text around what matched, for a row that doesn't show
/// that field: `label: …text…`, at most `width` characters (no label when it's too narrow
/// for one). Returns the text and the matched positions within it.
pub fn snippet(part: &HitPart, width: usize) -> (String, Vec<usize>) {
    // Too narrow for the label and a useful window: the window alone.
    let label = format!("{}: ", part.field.label());
    let label = if width >= label.chars().count() + 8 {
        label
    } else {
        String::new()
    };
    let chars: Vec<char> = part
        .text
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    let room = width.saturating_sub(label.chars().count());
    let (Some(&first), Some(_)) = (part.indices.first(), part.indices.last()) else {
        return (String::new(), Vec::new());
    };
    if room < 4 {
        return (String::new(), Vec::new());
    }
    // A little lead-in before the first match, then as much as fits.
    let start = first
        .saturating_sub(room / 4)
        .min(chars.len().saturating_sub(room));
    let lead = usize::from(start > 0);
    let tail_room = room - lead;
    let end = (start + tail_room).min(chars.len());
    let cut = end < chars.len();
    let end = if cut { end - 1 } else { end };
    let mut text = label.clone();
    if lead == 1 {
        text.push('…');
    }
    text.extend(&chars[start..end]);
    if cut {
        text.push('…');
    }
    let offset = label.chars().count() + lead;
    let indices = part
        .indices
        .iter()
        .filter(|&&i| i >= start && i < end)
        .map(|&i| i - start + offset)
        .collect();
    (text, indices)
}

/// Every fork/merge pair among `nodes` (child→parent order). `protected` nodes — your `@`,
/// a workspace's `@`, an agent's revision, anything with a local bookmark — never fold
/// away, so a pair holding one isn't a pair.
fn find_pairs(nodes: &[GraphNode], protected: &[bool]) -> Vec<Pair> {
    let idx: HashMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.change_id.as_str(), i))
        .collect();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    for (i, n) in nodes.iter().enumerate() {
        for p in &n.parents {
            if let Some(&pi) = idx.get(p.as_str()) {
                children[pi].push(i);
            }
        }
    }
    let mut pairs = Vec::new();
    for (m, node) in nodes.iter().enumerate() {
        let [p1, p2] = node.parents.as_slice() else {
            continue;
        };
        let (Some(&p1), Some(&p2)) = (idx.get(p1.as_str()), idx.get(p2.as_str())) else {
            continue;
        };
        // The mainline: the first parent and everything loaded beneath it.
        let mut mainline = HashSet::new();
        let mut stack = vec![p1];
        while let Some(i) = stack.pop() {
            if mainline.insert(i) {
                stack.extend(nodes[i].parents.iter().filter_map(|p| idx.get(p.as_str())));
            }
        }
        // Walk the side branch down to where it meets the mainline.
        let mut side = Vec::new();
        let mut cur = p2;
        let ok = loop {
            if mainline.contains(&cur) {
                break !side.is_empty();
            }
            let n = &nodes[cur];
            let above = side.last().copied().unwrap_or(m);
            if protected[cur] || n.parents.len() != 1 || children[cur] != [above] {
                break false;
            }
            side.push(cur);
            match idx.get(n.parents[0].as_str()) {
                Some(&next) => cur = next,
                None => break false,
            }
        };
        if ok {
            pairs.push(Pair { merge: m, side });
        }
    }
    pairs
}

/// A timestamp as a compact age: `2m`, `1h`, `3d`, `5w`, then the date.
pub fn age(now: i64, then: i64) -> String {
    let secs = (now - then).max(0);
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3_600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3_600),
        s if s < 7 * 86_400 => format!("{}d", s / 86_400),
        s if s < 52 * 7 * 86_400 => format!("{}w", s / (7 * 86_400)),
        _ => chrono::DateTime::from_timestamp(then, 0)
            .map(|d| d.format("%Y-%m").to_string())
            .unwrap_or_default(),
    }
}

/// Who to credit on a row: the author's name, else their email's local part.
pub fn author(rev: &RevInfo) -> String {
    if !rev.author.is_empty() {
        return rev.author.clone();
    }
    rev.email.split('@').next().unwrap_or_default().to_string()
}

/// One entry in a row's bookmark column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mark {
    /// A local bookmark, and the remotes that agree with it (`master @origin`).
    Local(String, Vec<String>),
    /// A remote bookmark with no local twin here (`pr-12@origin`).
    Remote(String),
    /// Another workspace's working copy (`agent-3@`).
    Workspace(String),
}

impl Mark {
    pub fn width(&self) -> usize {
        match self {
            Mark::Local(n, rs) => {
                n.chars().count() + rs.iter().map(|r| 2 + r.chars().count()).sum::<usize>()
            }
            Mark::Remote(n) => n.chars().count(),
            Mark::Workspace(n) => n.chars().count() + 1,
        }
    }
}

/// A revision's bookmark column, in the order shown.
pub fn marks(rev: &RevInfo, workspaces: &[Workspace]) -> Vec<Mark> {
    let mut out: Vec<Mark> = rev
        .bookmarks
        .iter()
        .map(|b| {
            let agreeing = rev
                .remote_bookmarks
                .iter()
                .filter_map(|r| r.strip_prefix(b.as_str())?.strip_prefix('@'))
                .map(str::to_string)
                .collect();
            Mark::Local(b.clone(), agreeing)
        })
        .collect();
    out.extend(
        rev.remote_bookmarks
            .iter()
            .filter(|r| {
                !rev.bookmarks.iter().any(|b| {
                    r.strip_prefix(b.as_str())
                        .is_some_and(|t| t.starts_with('@'))
                })
            })
            .map(|r| Mark::Remote(r.clone())),
    );
    out.extend(
        workspaces
            .iter()
            .filter(|w| w.change_id == rev.change_id && w.name != "default")
            .map(|w| Mark::Workspace(w.name.clone())),
    );
    out
}

/// The name a squash of `rev` is called by: its first local bookmark, else its first remote
/// one, else its short change id.
pub fn squash_name(rev: Option<&RevInfo>, change_id: &str) -> String {
    rev.and_then(|r| r.bookmarks.first().or(r.remote_bookmarks.first()))
        .cloned()
        .unwrap_or_else(|| change_id.chars().take(8).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, parents: &[&str], desc: &str) -> GraphNode {
        GraphNode {
            change_id: id.into(),
            parents: parents.iter().map(|s| s.to_string()).collect(),
            glyph: '◆',
            lines: vec![desc.into()],
            collapse: false,
            stub: false,
        }
    }

    fn rev(id: &str) -> RevInfo {
        RevInfo {
            change_id: id.into(),
            ..Default::default()
        }
    }

    /// main: m2 (merge of m1 + s2←s1 forked at m1's parent m0) → m1 → m0, and a later
    /// merge `top` of m2 + t1 (forked at m2).
    fn history() -> Vec<GraphNode> {
        vec![
            node("top", &["m2", "t1"], "Merge #2: nix flake"),
            node("t1", &["m2"], "add a nix flake"),
            node("m2", &["m1", "s2"], "Merge #1: lanes"),
            node("s2", &["s1"], "nest lanes"),
            node("s1", &["m0"], "draw lanes"),
            node("m1", &["m0"], "fix a typo"),
            node("m0", &[], "base"),
        ]
    }

    fn view(nodes: Vec<GraphNode>, meta: &[RevInfo]) -> LogView {
        let meta: HashMap<String, RevInfo> = meta
            .iter()
            .map(|r| (r.change_id.clone(), r.clone()))
            .collect();
        let n = nodes.len();
        let mut v = LogView::default();
        v.set_model(nodes, vec![None; n], &meta, &[]);
        v
    }

    #[test]
    fn fork_merge_pairs_fold_to_their_merge() {
        let v = view(history(), &[]);
        assert_eq!(v.ids(), vec!["top", "m2", "m1", "m0"]);
        assert_eq!(v.folded("top"), Some(1));
        assert_eq!(
            v.folded("m2"),
            Some(2),
            "forked before main moved on: still a pair"
        );
        // Folded merges draw as plain trunk rows, without a side lane.
        let merge_row = v
            .rows()
            .iter()
            .find(|r| r.change_id.as_deref() == Some("m2"))
            .unwrap();
        assert!(!merge_row.gutter.contains('╮'), "{:?}", merge_row.gutter);
        assert!(
            merge_row.gutter.contains(FOLDED_GLYPH),
            "{:?}",
            merge_row.gutter
        );
        let opened = v
            .rows()
            .iter()
            .find(|r| r.change_id.as_deref() == Some("m1"));
        assert!(
            !opened.unwrap().gutter.contains(FOLDED_GLYPH),
            "only folds get it"
        );
    }

    #[test]
    fn a_bookmark_or_working_copy_on_the_side_keeps_the_pair_open() {
        let mut bm = rev("s1");
        bm.bookmarks = vec!["feature".into()];
        let v = view(history(), &[bm]);
        assert_eq!(v.folded("m2"), None);
        assert!(v.ids().contains(&"s1"));

        let mut nodes = history();
        nodes[3].glyph = '@';
        let v = view(nodes, &[]);
        assert_eq!(v.folded("m2"), None);
    }

    #[test]
    fn a_side_commit_another_branch_forks_from_is_not_folded() {
        let mut nodes = history();
        nodes.insert(0, node("x", &["s1"], "builds on the side branch"));
        let v = view(nodes, &[]);
        assert_eq!(v.folded("m2"), None);
    }

    #[test]
    fn right_expands_and_left_folds_back_onto_the_merge() {
        let mut v = view(history(), &[]);
        v.selected = Some("m2".into());
        assert!(v.expand());
        assert_eq!(v.ids(), vec!["top", "m2", "s2", "s1", "m1", "m0"]);
        v.selected = Some("s1".into());
        assert!(v.collapse());
        assert_eq!(v.selected.as_deref(), Some("m2"));
        assert_eq!(v.folded("m2"), Some(2));
        assert!(!v.collapse(), "already folded");
    }

    #[test]
    fn a_hit_inside_a_pair_opens_it_and_context_surrounds_it() {
        let mut v = view(history(), &[]);
        v.selected = Some("top".into());
        v.open_search();
        v.set_query("nest lanes".into());
        assert_eq!(v.hits(), ["s2"]);
        assert_eq!(v.selected.as_deref(), Some("s2"), "lands on the best hit");
        // ±1 in display order: m2 above, s1 below.
        assert_eq!(v.ids(), vec!["m2", "s2", "s1"]);
        assert_eq!(
            v.gaps(),
            [
                Gap { at: 0, count: 1 },
                Gap {
                    at: v.rows().len(),
                    count: 2
                }
            ]
        );
        v.adjust_context(1);
        assert_eq!(v.ids(), vec!["top", "m2", "s2", "s1", "m1"]);
        v.adjust_context(-2);
        assert_eq!(v.ids(), vec!["s2"]);
        v.clear_search();
        assert_eq!(v.selected.as_deref(), Some("s2"), "kept");
        assert_eq!(
            v.ids(),
            vec!["top", "m2", "s2", "s1", "m1", "m0"],
            "the pair holding it stays open; the rest fold again"
        );
    }

    #[test]
    fn searching_matches_bookmarks_authors_and_descriptions() {
        let mut a = rev("m1");
        a.author = "Hamish".into();
        let mut b = rev("m0");
        b.remote_bookmarks = vec!["hh/nix-flake@origin".into()];
        let mut v = view(history(), &[a, b]);
        v.open_search();
        v.set_query("hamish".into());
        assert_eq!(v.hits(), ["m1"]);
        v.set_query("nix-flake@origin".into());
        assert_eq!(v.hits(), ["m0"]);
        v.set_query("typo".into());
        assert_eq!(v.hits(), ["m1"]);
    }

    #[test]
    fn a_fuzzy_match_never_strings_letters_across_fields() {
        // "master": `m`, `a` from the email, then `s t e r` from "Set … terminal". Joined
        // into one string that matched; field by field it can't.
        let mut a = rev("m1");
        a.email = "jezza@gmail.com".into();
        a.full_description = "Set faff's own terminal title".into();
        let mut nodes = history();
        nodes[5].lines = vec!["Set faff's own terminal title".into()];
        let mut v = view(nodes, &[a]);
        v.open_search();
        v.set_query("master".into());
        assert!(v.hits().is_empty(), "{:?}", v.hits());
    }

    #[test]
    fn a_hit_says_which_field_matched_and_where() {
        let mut a = rev("m1");
        a.email = "hamish@example.com".into();
        a.author = "Jezza".into();
        let mut v = view(history(), &[a]);
        v.open_search();
        v.set_query("example".into());
        let hit = v.hit("m1").unwrap();
        assert_eq!(hit.parts.len(), 1);
        assert_eq!(hit.parts[0].field, Field::Email);
        assert_eq!(hit.parts[0].indices, (7..14).collect::<Vec<_>>());

        // Each word finds its own field, and must find one.
        v.set_query("jezza typo".into());
        let fields: Vec<Field> = v.hit("m1").unwrap().parts.iter().map(|p| p.field).collect();
        assert_eq!(fields, [Field::Author, Field::Row]);
        v.set_query("jezza zebra".into());
        assert!(v.hits().is_empty());

        // A tie goes to the field the row shows: the first line is in both.
        v.set_query("typo".into());
        assert_eq!(v.hit("m1").unwrap().parts[0].field, Field::Row);
        // A negated word rules a revision out if any field has it.
        v.set_query("typo !hamish".into());
        assert!(v.hits().is_empty());
    }

    #[test]
    fn only_a_long_enough_plain_query_is_searched_for_in_the_files() {
        assert_eq!(file_query("ab"), None);
        assert_eq!(file_query(" revops "), Some("revops".into()));
        assert_eq!(file_query("'fn main"), Some("fn main".into()));
        assert_eq!(file_query("^src$"), Some("src".into()));
        assert_eq!(file_query("!lanes"), None);
    }

    #[test]
    fn what_jj_found_in_the_files_are_hits_for_their_own_query_alone() {
        let found = |kind, text: &str, cid: &str| crate::jj::FileMatch {
            change_id: cid.into(),
            commit_id: format!("c-{cid}"),
            kind,
            text: text.into(),
        };
        let mut v = view(history(), &[]);
        v.found = Some(Found {
            query: "lanes".into(),
            matches: vec![
                found(MatchKind::Path, "src/LANES.rs", "m1"),
                found(MatchKind::Line, "fn lanes()", "m0"),
            ],
        });
        v.open_search();
        v.set_query("lanes".into());
        // The fuzzy hits (the rows saying "lanes") and jj's finds.
        assert_eq!(v.hits(), ["m2", "s2", "s1", "m1", "m0"]);
        assert_eq!(
            v.selected.as_deref(),
            Some("m2"),
            "lands on the best fuzzy hit"
        );
        let part = &v.hit("m1").unwrap().parts[0];
        assert_eq!((part.field, part.field.icon()), (Field::File, Some('▤')));
        assert_eq!(part.indices, (4..9).collect::<Vec<_>>(), "case ignored");
        assert_eq!(v.hit("m0").unwrap().parts[0].field, Field::Line);

        // A query the finds weren't for doesn't show them.
        v.set_query("lanes!".into());
        assert!(v.hit("m1").is_none() && v.hit("m0").is_none());
    }

    #[test]
    fn a_snippet_frames_the_match_and_maps_its_positions() {
        let part = HitPart {
            field: Field::Email,
            text: "someone.with.a.long.name@example.com".into(),
            indices: vec![25, 26, 27],
        };
        let (text, idx) = snippet(&part, 24);
        assert_eq!(text.chars().count(), 24, "{text}");
        assert!(text.starts_with("email: …"), "{text}");
        let picked: String = idx.iter().map(|&i| text.chars().nth(i).unwrap()).collect();
        assert_eq!(picked, "exa", "{text} {idx:?}");

        let part = HitPart {
            field: Field::Description,
            text: "fix\nthe typo".into(),
            indices: vec![0],
        };
        let (text, idx) = snippet(&part, 40);
        assert_eq!(text, "desc: fix the typo", "whitespace flattens");
        assert_eq!(idx, [6]);
        let (text, idx) = snippet(&part, 10);
        assert_eq!(text, "fix the t…", "too narrow for the label");
        assert_eq!(idx, [0]);
    }

    #[test]
    fn stepping_while_filtered_jumps_between_hits() {
        let mut v = view(history(), &[]);
        v.open_search();
        v.set_query("lanes".into());
        let hits: Vec<String> = v.hits().to_vec();
        assert!(hits.len() >= 2, "{hits:?}");
        v.selected = Some(hits[0].clone());
        v.step(1);
        assert_eq!(v.selected.as_ref(), Some(&hits[1]));
        v.step(-1);
        assert_eq!(v.selected.as_ref(), Some(&hits[0]));
        v.step(-1);
        assert_eq!(v.selected.as_ref(), hits.last(), "wraps");
    }

    #[test]
    fn an_empty_query_filters_nothing() {
        let mut v = view(history(), &[]);
        v.open_search();
        assert!(!v.filtering());
        assert_eq!(v.ids(), vec!["top", "m2", "m1", "m0"]);
        v.lock_search();
        assert!(
            v.search().is_none(),
            "locking an empty search just closes it"
        );
    }

    #[test]
    fn marks_pair_local_bookmarks_with_their_remotes() {
        let mut r = rev("a");
        r.bookmarks = vec!["master".into()];
        r.remote_bookmarks = vec!["master@origin".into(), "pr-12@origin".into()];
        let ws = vec![
            Workspace {
                name: "default".into(),
                change_id: "a".into(),
            },
            Workspace {
                name: "agent-3".into(),
                change_id: "a".into(),
            },
        ];
        assert_eq!(
            marks(&r, &ws),
            vec![
                Mark::Local("master".into(), vec!["origin".into()]),
                Mark::Remote("pr-12@origin".into()),
                Mark::Workspace("agent-3".into()),
            ]
        );
        assert_eq!(
            Mark::Local("master".into(), vec!["origin".into()]).width(),
            14
        );
    }

    #[test]
    fn ages_are_compact() {
        assert_eq!(age(1000, 990), "10s");
        assert_eq!(age(10_000, 10_000 - 120), "2m");
        assert_eq!(age(100_000, 100_000 - 7_200), "2h");
        assert_eq!(age(1_000_000, 1_000_000 - 3 * 86_400), "3d");
        assert_eq!(age(10_000_000, 10_000_000 - 21 * 86_400), "3w");
        assert_eq!(age(1_800_000_000, 1_700_000_000), "2023-11");
    }

    #[test]
    fn a_squash_is_named_for_its_bookmark() {
        let mut r = rev("abcdefghij");
        assert_eq!(squash_name(Some(&r), "abcdefghij"), "abcdefgh");
        r.remote_bookmarks = vec!["pr@origin".into()];
        assert_eq!(squash_name(Some(&r), "abcdefghij"), "pr@origin");
        r.bookmarks = vec!["mine".into()];
        assert_eq!(squash_name(Some(&r), "abcdefghij"), "mine");
    }
}
