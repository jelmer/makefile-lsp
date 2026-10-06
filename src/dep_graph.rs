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
use rowan::ast::AstNode;
use text_size::TextRange;

use crate::conditionals::{combine, conditional_branches, Branches};

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

/// Conventional names for top-level targets that are meant to be invoked from
/// the command line. Diagnostics and code actions that key off "nothing depends
/// on this" should leave these alone — they're entry points by intent.
const CONVENTIONAL_ENTRY_POINTS: &[&str] = &[
    "all",
    "build",
    "check",
    "clean",
    "default",
    "dist",
    "distclean",
    "doc",
    "docs",
    "help",
    "install",
    "lint",
    "release",
    "run",
    "test",
    "tests",
    "uninstall",
];

/// Is `name` a conventional command-line entry-point target?
pub fn is_conventional_entry_point(name: &str) -> bool {
    CONVENTIONAL_ENTRY_POINTS.contains(&name)
}

/// A prerequisite edge, with the conditional branches of the rule that
/// added it.
#[derive(Debug, Clone, PartialEq)]
struct Edge {
    to: String,
    branches: Branches,
}

/// Directed graph of target → prerequisites built from a parsed Makefile.
///
/// Rules inside conditionals only add edges in their branch. Paths that
/// would need edges from mutually exclusive branches of a conditional (e.g.
/// one from an `ifdef` and one from its `else`) are never followed, since
/// make can never see both rules.
#[derive(Debug, Default, Clone)]
pub struct DependencyGraph {
    edges: HashMap<String, Vec<Edge>>,
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

        let mut edges: HashMap<String, Vec<Edge>> = HashMap::new();
        for rule in makefile.rules() {
            let prereqs: Vec<String> = rule.prerequisites().collect();
            let branches = conditional_branches(rule.syntax());
            for target in rule.targets() {
                if !is_graph_target(&target) {
                    continue;
                }
                let entry = edges.entry(target.clone()).or_default();
                for prereq in &prereqs {
                    if prereq == &target || !defined_targets.contains(prereq) {
                        continue;
                    }
                    let edge = Edge {
                        to: prereq.clone(),
                        branches: branches.clone(),
                    };
                    if !entry.contains(&edge) {
                        entry.push(edge);
                    }
                }
            }
        }
        for entry in edges.values_mut() {
            entry.sort_by(|a, b| a.to.cmp(&b.to));
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

    /// Targets that list `target` as a prerequisite, in sorted order. Useful
    /// for reverse-reachability queries (e.g. "is anything depending on me?").
    pub fn referrers(&self, target: &str) -> impl Iterator<Item = &str> {
        let mut names: Vec<&str> = self
            .edges
            .iter()
            .filter_map(|(k, v)| v.iter().any(|e| e.to == target).then_some(k.as_str()))
            .collect();
        names.sort();
        names.into_iter()
    }

    /// Edges out of `node` that can be followed when the conditional
    /// branches in `context` are taken, sorted by prerequisite, each with the
    /// branches taken after following it.
    fn successors(&self, node: &str, context: &[(TextRange, usize)]) -> Vec<(&str, Branches)> {
        self.edges
            .get(node)
            .into_iter()
            .flatten()
            .filter_map(|e| Some((e.to.as_str(), combine(context, &e.branches)?)))
            .collect()
    }

    /// Targets reachable from `start` by following prerequisite edges that
    /// can be taken together with the conditional branches in `context`
    /// (e.g. those of the rule asking), not including `start` itself.
    /// Returns an empty set if `start` isn't a target. Cycle-safe.
    pub fn reachable_from(&self, start: &str, context: &[(TextRange, usize)]) -> HashSet<String> {
        let mut reached: HashSet<String> = HashSet::new();
        let mut visited: HashSet<(&str, Branches)> = HashSet::new();
        let mut stack = self.successors(start, context);
        while let Some((node, branches)) = stack.pop() {
            if !visited.insert((node, branches.clone())) {
                continue;
            }
            reached.insert(node.to_string());
            stack.extend(self.successors(node, &branches));
        }
        reached
    }

    /// Length of the longest path from `target` down through the dependency
    /// graph, counting edges. A leaf (or unknown target) returns 0; a target
    /// with one prerequisite that itself has no prerequisites returns 1.
    ///
    /// Cycle-safe: a back-edge into a node already on the DFS stack contributes
    /// nothing, so the result is well-defined even for malformed graphs.
    pub fn longest_path_length(&self, target: &str) -> usize {
        let mut memo: HashMap<(&str, Branches), usize> = HashMap::new();
        let mut on_stack: HashSet<&str> = HashSet::new();
        self.longest_from(target, Vec::new(), &mut memo, &mut on_stack)
    }

    fn longest_from<'a>(
        &'a self,
        node: &'a str,
        context: Branches,
        memo: &mut HashMap<(&'a str, Branches), usize>,
        on_stack: &mut HashSet<&'a str>,
    ) -> usize {
        if let Some(&d) = memo.get(&(node, context.clone())) {
            return d;
        }
        if !on_stack.insert(node) {
            return 0;
        }
        let best = self
            .successors(node, &context)
            .into_iter()
            .map(|(next, branches)| 1 + self.longest_from(next, branches, memo, on_stack))
            .max()
            .unwrap_or(0);
        on_stack.remove(node);
        memo.insert((node, context), best);
        best
    }

    /// Find simple cycles of length ≥ 2 in the graph.
    ///
    /// Each cycle is returned as the list of target names visited, with the
    /// smallest name first (canonical rotation) so equivalent rotations dedupe.
    /// Self-loops are not returned — callers that care about them should check
    /// for `prereq == target` separately. Cycles are returned in the order a
    /// depth-first walk over sorted nodes discovers them.
    ///
    /// The walk visits each target once per set of conditional branches it
    /// can be reached with, so a cycle is only found along edges that can be
    /// taken together.
    pub fn find_cycles(&self) -> Vec<Vec<String>> {
        let mut done: HashSet<(&str, Branches)> = HashSet::new();
        let mut reported: HashSet<Vec<String>> = HashSet::new();
        let mut cycles: Vec<Vec<String>> = Vec::new();

        for start in self.targets() {
            if done.contains(&(start, Vec::new())) {
                continue;
            }
            let mut path: Vec<&str> = vec![start];
            let mut stack = vec![(
                start,
                Branches::new(),
                self.successors(start, &[]).into_iter(),
            )];

            while let Some((_, _, iter)) = stack.last_mut() {
                let Some((next, branches)) = iter.next() else {
                    let (node, context, _) = stack.pop().unwrap();
                    done.insert((node, context));
                    path.pop();
                    continue;
                };
                if let Some(idx) = path.iter().position(|n| *n == next) {
                    let cycle: Vec<String> = path[idx..].iter().map(|s| s.to_string()).collect();
                    let min_pos = cycle.iter().enumerate().min_by_key(|(_, n)| *n).unwrap().0;
                    let mut canon: Vec<String> = cycle[min_pos..].to_vec();
                    canon.extend_from_slice(&cycle[..min_pos]);
                    if reported.insert(canon.clone()) {
                        cycles.push(canon);
                    }
                } else if !done.contains(&(next, branches.clone())) {
                    let succs = self.successors(next, &branches);
                    path.push(next);
                    stack.push((next, branches, succs.into_iter()));
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

    fn prereqs<'a>(g: &'a DependencyGraph, target: &str) -> Vec<&'a str> {
        g.successors(target, &[])
            .into_iter()
            .map(|(p, _)| p)
            .collect()
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
        assert_eq!(prereqs(&g, "a"), vec!["b"]);
    }

    #[test]
    fn accumulates_prereqs_across_rules() {
        let g = graph("a: b\n\t@:\nb:\n\t@:\na: c\nc:\n\t@:\n");
        assert_eq!(prereqs(&g, "a"), vec!["b", "c"]);
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
    fn reachable_from_returns_transitive_closure() {
        let g = graph("a: b\n\t@:\nb: c d\n\t@:\nc:\n\t@:\nd:\n\t@:\n");
        let mut reached: Vec<String> = g.reachable_from("a", &[]).into_iter().collect();
        reached.sort();
        assert_eq!(reached, vec!["b", "c", "d"]);
        assert!(g.reachable_from("c", &[]).is_empty());
        assert!(g.reachable_from("missing", &[]).is_empty());
    }

    #[test]
    fn reachable_from_handles_cycles() {
        let g = graph("a: b\n\t@:\nb: a\n\t@:\n");
        let reached = g.reachable_from("a", &[]);
        assert!(reached.contains("a"));
        assert!(reached.contains("b"));
    }

    #[test]
    fn reachable_from_respects_context() {
        let text = "ifdef X\nb: c\nelse\nall: b\nendif\nc:\n";
        let parsed: Parse<Makefile> = Makefile::parse(text);
        let makefile = parsed.tree();
        let g = DependencyGraph::from_makefile(&makefile);
        let all_rule = makefile.rules_by_target("all").next().unwrap();
        let else_branch = conditional_branches(all_rule.syntax());
        assert_eq!(g.reachable_from("b", &[]), HashSet::from(["c".to_string()]));
        assert_eq!(g.reachable_from("b", &else_branch), HashSet::new());
    }

    #[test]
    fn reachable_from_skips_paths_across_branches() {
        let g = graph("ifdef X\na: b\nelse\nb: c\nendif\nc:\n");
        assert_eq!(g.reachable_from("a", &[]), HashSet::from(["b".to_string()]));
    }

    #[test]
    fn no_cycle_across_nested_exclusive_branches() {
        let g = graph("ifdef X\nifdef Y\na: b\nendif\nelse\nb: a\nendif\n");
        assert_eq!(g.find_cycles(), Vec::<Vec<String>>::new());
    }

    #[test]
    fn cycle_across_unrelated_conditionals() {
        let g = graph("ifdef X\na: b\nendif\nifdef Y\nb: a\nendif\n");
        assert_eq!(
            g.find_cycles(),
            vec![vec!["a".to_string(), "b".to_string()]]
        );
    }

    #[test]
    fn referrers_include_all_branches() {
        let g = graph("ifdef X\na: c\nelse\nb: c\nendif\nc:\n");
        assert_eq!(g.referrers("c").collect::<Vec<_>>(), vec!["a", "b"]);
    }

    #[test]
    fn longest_path_handles_chains_and_diamonds() {
        // chain a -> b -> c -> d  ==> depth 3 from a
        let g = graph("a: b\n\t@:\nb: c\n\t@:\nc: d\n\t@:\nd:\n\t@:\n");
        assert_eq!(g.longest_path_length("a"), 3);
        assert_eq!(g.longest_path_length("d"), 0);
        // diamond a -> b,c; b -> d; c -> d -- longest is still 2
        let g = graph("a: b c\n\t@:\nb: d\n\t@:\nc: d\n\t@:\nd:\n\t@:\n");
        assert_eq!(g.longest_path_length("a"), 2);
        // unknown target -> 0
        assert_eq!(g.longest_path_length("missing"), 0);
    }

    #[test]
    fn longest_path_does_not_loop_on_cycles() {
        let g = graph("a: b\n\t@:\nb: a\n\t@:\n");
        // Just needs to terminate and return something finite.
        let _ = g.longest_path_length("a");
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
