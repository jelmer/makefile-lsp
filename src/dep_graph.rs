//! Target dependency graph for a Makefile.
//!
//! Builds a directed graph from target names to their prerequisites, folding
//! together multiple rules for the same target (make accumulates their
//! prerequisites). Pattern rules (`%.o`) and special accumulating targets
//! (`.PHONY`, `.PRECIOUS`, …) are excluded — their semantics aren't a plain
//! dependency edge. Edges are only added for prerequisites that are themselves
//! defined as targets; undefined names are files on disk and aren't part of
//! the in-Makefile graph.
//!
//! Intended to back features like cycle detection, unreachable-target checks,
//! call hierarchy, and dependency visualization.

use std::collections::{HashMap, HashSet};

use makefile_lossless::Makefile;

/// Special targets where multiple definitions accumulate prerequisites rather
/// than redefining the rule.
const ACCUMULATING_TARGETS: &[&str] = &[
    ".PHONY",
    ".SUFFIXES",
    ".PRECIOUS",
    ".INTERMEDIATE",
    ".SECONDARY",
    ".IGNORE",
    ".SILENT",
    ".NOTPARALLEL",
];

/// Should this target name participate in the dependency graph?
///
/// Public so callers can filter consistently when matching graph nodes against
/// other target lists.
pub fn is_graph_target(name: &str) -> bool {
    !(name.contains('%') || (name.starts_with('.') && ACCUMULATING_TARGETS.contains(&name)))
}

/// Directed graph of target → prerequisites built from a parsed Makefile.
#[derive(Debug, Default, Clone)]
pub struct DependencyGraph {
    edges: HashMap<String, HashSet<String>>,
}

impl DependencyGraph {
    /// Build the graph from a parsed Makefile. See the module docs for the
    /// filtering rules applied to targets and prerequisites.
    pub fn from_makefile(makefile: &Makefile) -> Self {
        let mut defined_targets: HashSet<String> = HashSet::new();
        for rule in makefile.rules() {
            for target in rule.targets() {
                if is_graph_target(&target) {
                    defined_targets.insert(target);
                }
            }
        }

        let mut edges: HashMap<String, HashSet<String>> = HashMap::new();
        for rule in makefile.rules() {
            let prereqs: Vec<String> = rule.prerequisites().collect();
            for target in rule.targets() {
                if !is_graph_target(&target) {
                    continue;
                }
                let entry = edges.entry(target.clone()).or_default();
                for prereq in &prereqs {
                    if prereq != &target && defined_targets.contains(prereq) {
                        entry.insert(prereq.clone());
                    }
                }
            }
        }

        Self { edges }
    }

    /// Iterate over the targets that have an entry in the graph, in
    /// deterministic (sorted) order.
    pub fn targets(&self) -> impl Iterator<Item = &str> {
        let mut names: Vec<&str> = self.edges.keys().map(String::as_str).collect();
        names.sort();
        names.into_iter()
    }

    /// Direct prerequisites of `target` that are themselves targets, in sorted
    /// order. Returns an empty iterator for unknown or leaf targets.
    pub fn prerequisites(&self, target: &str) -> impl Iterator<Item = &str> {
        let mut names: Vec<&str> = self
            .edges
            .get(target)
            .map(|s| s.iter().map(String::as_str).collect())
            .unwrap_or_default();
        names.sort();
        names.into_iter()
    }

    /// Targets that list `target` as a prerequisite, in sorted order. Useful
    /// for reverse-reachability queries (e.g. "is anything depending on me?").
    pub fn referrers(&self, target: &str) -> impl Iterator<Item = &str> {
        let mut names: Vec<&str> = self
            .edges
            .iter()
            .filter_map(|(k, v)| v.contains(target).then_some(k.as_str()))
            .collect();
        names.sort();
        names.into_iter()
    }

    /// Find all simple cycles of length ≥ 2 in the graph.
    ///
    /// Each cycle is returned as the list of target names visited, with the
    /// smallest name first (canonical rotation) so equivalent rotations dedupe.
    /// Self-loops are not returned — callers that care about them should check
    /// for `prereq == target` separately. Cycles are returned in the order a
    /// depth-first walk over sorted nodes discovers them.
    pub fn find_cycles(&self) -> Vec<Vec<String>> {
        #[derive(Clone, Copy, PartialEq)]
        enum State {
            Unvisited,
            OnStack,
            Done,
        }

        let mut state: HashMap<&str, State> = self
            .edges
            .keys()
            .map(|k| (k.as_str(), State::Unvisited))
            .collect();
        let mut reported: HashSet<Vec<String>> = HashSet::new();
        let mut cycles: Vec<Vec<String>> = Vec::new();

        let nodes: Vec<&str> = self.targets().collect();

        for start in nodes {
            if state.get(start).copied() != Some(State::Unvisited) {
                continue;
            }
            let mut path: Vec<&str> = vec![start];
            let mut stack: Vec<(&str, std::vec::IntoIter<&str>)> = Vec::new();
            let succs: Vec<&str> = self.prerequisites(start).collect();
            state.insert(start, State::OnStack);
            stack.push((start, succs.into_iter()));

            while let Some((_node, iter)) = stack.last_mut() {
                if let Some(next) = iter.next() {
                    match state.get(next).copied().unwrap_or(State::Done) {
                        State::Unvisited => {
                            let next_succs: Vec<&str> = self.prerequisites(next).collect();
                            state.insert(next, State::OnStack);
                            path.push(next);
                            stack.push((next, next_succs.into_iter()));
                        }
                        State::OnStack => {
                            let idx = path.iter().position(|n| *n == next).unwrap();
                            let cycle: Vec<String> =
                                path[idx..].iter().map(|s| s.to_string()).collect();
                            let min_pos =
                                cycle.iter().enumerate().min_by_key(|(_, n)| *n).unwrap().0;
                            let mut canon: Vec<String> = cycle[min_pos..].to_vec();
                            canon.extend_from_slice(&cycle[..min_pos]);
                            if reported.insert(canon.clone()) {
                                cycles.push(canon);
                            }
                        }
                        State::Done => {}
                    }
                } else {
                    let (node, _) = stack.pop().unwrap();
                    state.insert(node, State::Done);
                    path.pop();
                }
            }
        }

        cycles
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use makefile_lossless::Parse;

    fn graph(text: &str) -> DependencyGraph {
        let parsed: Parse<Makefile> = Makefile::parse(text);
        DependencyGraph::from_makefile(&parsed.tree())
    }

    #[test]
    fn empty_makefile_has_no_edges() {
        let g = graph("");
        assert_eq!(g.targets().count(), 0);
        assert!(g.find_cycles().is_empty());
    }

    #[test]
    fn prerequisites_only_include_defined_targets() {
        let g = graph("a: b missing\n\t@:\nb:\n\t@:\n");
        let prereqs: Vec<&str> = g.prerequisites("a").collect();
        assert_eq!(prereqs, vec!["b"]);
    }

    #[test]
    fn accumulates_prereqs_across_rules() {
        let g = graph("a: b\n\t@:\nb:\n\t@:\na: c\nc:\n\t@:\n");
        let mut prereqs: Vec<&str> = g.prerequisites("a").collect();
        prereqs.sort();
        assert_eq!(prereqs, vec!["b", "c"]);
    }

    #[test]
    fn pattern_rules_excluded() {
        let g = graph("%.o: %.c\n\t@:\nfoo.c:\n\t@:\n");
        let targets: Vec<&str> = g.targets().collect();
        assert_eq!(targets, vec!["foo.c"]);
    }

    #[test]
    fn special_targets_excluded() {
        let g = graph(".PHONY: clean\nclean:\n\t@:\n");
        let targets: Vec<&str> = g.targets().collect();
        assert_eq!(targets, vec!["clean"]);
    }

    #[test]
    fn referrers_lists_incoming_edges() {
        let g = graph("all: a b\n\t@:\na: b\n\t@:\nb:\n\t@:\n");
        let mut r: Vec<&str> = g.referrers("b").collect();
        r.sort();
        assert_eq!(r, vec!["a", "all"]);
        assert_eq!(g.referrers("all").count(), 0);
        assert_eq!(g.referrers("missing").count(), 0);
    }

    #[test]
    fn finds_two_node_cycle() {
        let g = graph("a: b\n\t@:\nb: a\n\t@:\n");
        let cycles = g.find_cycles();
        assert_eq!(cycles, vec![vec!["a".to_string(), "b".to_string()]]);
    }

    #[test]
    fn finds_three_node_cycle() {
        let g = graph("a: b\n\t@:\nb: c\n\t@:\nc: a\n\t@:\n");
        let cycles = g.find_cycles();
        assert_eq!(
            cycles,
            vec![vec!["a".to_string(), "b".to_string(), "c".to_string()]]
        );
    }

    #[test]
    fn ignores_self_loops() {
        let g = graph("a: a\n\t@:\n");
        assert!(g.find_cycles().is_empty());
    }

    #[test]
    fn dedupes_cycle_rotations() {
        let g = graph("a: b\n\t@:\nb: c\n\t@:\nc: a\n\t@:\nentry: a b c\n\t@:\n");
        assert_eq!(g.find_cycles().len(), 1);
    }

    #[test]
    fn finds_disjoint_cycles() {
        let g = graph("a: b\n\t@:\nb: a\n\t@:\nx: y\n\t@:\ny: x\n\t@:\n");
        assert_eq!(g.find_cycles().len(), 2);
    }
}
