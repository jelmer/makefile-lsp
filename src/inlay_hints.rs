//! Inlay hints for Makefiles.
//!
//! Two kinds of hints today:
//! * Simply-expanded variable references show their resolved value.
//! * Top-level targets (graph entry points) show `depth N` when the longest
//!   path through their prerequisites is non-trivial.

use makefile_lossless::{Makefile, SyntaxKind};
use rowan::ast::AstNode;
use tower_lsp_server::ls_types::{InlayHint, InlayHintKind, InlayHintLabel, Range};

use crate::dep_graph::DependencyGraph;
use crate::position::text_range_to_lsp_range;

/// Depth threshold below which we don't emit a depth hint, to avoid spamming
/// every leaf rule. `depth 0` and `depth 1` rules are usually obvious.
const DEPTH_HINT_THRESHOLD: usize = 2;

/// Generate inlay hints for a Makefile within the given range.
pub fn get_inlay_hints(makefile: &Makefile, source_text: &str, range: Range) -> Vec<InlayHint> {
    let mut hints = Vec::new();

    hints.extend(target_depth_hints(makefile, source_text, range));

    for var_ref in makefile.variable_references() {
        let Some(name) = var_ref.name() else {
            continue;
        };

        let ref_range = text_range_to_lsp_range(source_text, var_ref.text_range());

        // Only include hints within the requested range
        if ref_range.end.line < range.start.line || ref_range.start.line > range.end.line {
            continue;
        }

        // Find the variable definition to show its value
        let Some(var_def) = makefile
            .variable_definitions()
            .find(|v| v.name().as_deref() == Some(&name))
        else {
            continue;
        };

        // Only show hints for simply-expanded variables (:= or ::=) since their
        // value is known at parse time. Recursively-expanded (=) values depend
        // on expansion context.
        let op = var_def.assignment_operator().unwrap_or_default();
        if op != ":=" && op != "::=" {
            continue;
        }

        let value = var_def
            .raw_value()
            .map(|v| v.trim().to_string())
            .unwrap_or_default();

        if value.is_empty() {
            continue;
        }

        // Truncate long values
        let display_value = if value.len() > 40 {
            format!("{}...", &value[..37])
        } else {
            value
        };

        hints.push(InlayHint {
            position: ref_range.end,
            label: InlayHintLabel::String(format!(": {}", display_value)),
            kind: Some(InlayHintKind::PARAMETER),
            text_edits: None,
            tooltip: None,
            padding_left: Some(true),
            padding_right: None,
            data: None,
        });
    }

    hints
}

/// Emit a `depth N` hint at the end of each top-level target's rule header.
///
/// Top-level = no incoming edges in the dependency graph (nothing depends on
/// it). Depth = length of the longest path from this target down through its
/// prerequisites. Skip rules with multiple targets (the hint would be
/// ambiguous about which target it refers to) and depths below the threshold.
fn target_depth_hints(makefile: &Makefile, source_text: &str, range: Range) -> Vec<InlayHint> {
    let mut hints = Vec::new();
    let graph = DependencyGraph::from_makefile(makefile);

    for rule in makefile.rules() {
        let targets: Vec<String> = rule.targets().collect();
        if targets.len() != 1 {
            continue;
        }
        let target = &targets[0];
        if !crate::dep_graph::is_graph_target(target) {
            continue;
        }
        if graph.referrers(target).any(|r| r != target) {
            continue;
        }
        let depth = graph.longest_path_length(target);
        if depth < DEPTH_HINT_THRESHOLD {
            continue;
        }

        let Some(targets_node) = rule
            .syntax()
            .children()
            .find(|c| c.kind() == SyntaxKind::TARGETS)
        else {
            continue;
        };
        let anchor = text_range_to_lsp_range(source_text, targets_node.text_range()).end;

        if anchor.line < range.start.line || anchor.line > range.end.line {
            continue;
        }

        hints.push(InlayHint {
            position: anchor,
            label: InlayHintLabel::String(format!("depth {}", depth)),
            kind: Some(InlayHintKind::PARAMETER),
            text_edits: None,
            tooltip: None,
            padding_left: Some(true),
            padding_right: Some(true),
            data: None,
        });
    }

    hints
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get_hints(text: &str) -> Vec<InlayHint> {
        use tower_lsp_server::ls_types::Position;
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let range = Range::new(Position::new(0, 0), Position::new(100, 0));
        get_inlay_hints(&makefile, text, range)
    }

    #[test]
    fn test_simply_expanded_hint() {
        let text = "CC := gcc\nCFLAGS := $(CC) -Wall\n";
        let hints = get_hints(text);
        // $(CC) in the CFLAGS definition should get a hint
        assert_eq!(hints.len(), 1);
        assert!(matches!(&hints[0].label, InlayHintLabel::String(s) if s.contains("gcc")));
    }

    #[test]
    fn test_no_hint_for_recursive() {
        let text = "CC = gcc\nCFLAGS = $(CC) -Wall\n";
        let hints = get_hints(text);
        // Recursively-expanded variables don't get hints
        assert!(hints.is_empty());
    }

    #[test]
    fn test_no_hint_for_undefined() {
        let text = "CFLAGS := $(UNDEFINED) -Wall\n";
        let hints = get_hints(text);
        assert!(hints.is_empty());
    }

    fn depth_hints(text: &str) -> Vec<String> {
        get_hints(text)
            .into_iter()
            .filter_map(|h| match h.label {
                InlayHintLabel::String(s) if s.starts_with("depth ") => Some(s),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn test_depth_hint_on_top_level_target() {
        // a -> b -> c -> d: a is top-level and has depth 3.
        let text = "a: b\n\t@:\nb: c\n\t@:\nc: d\n\t@:\nd:\n\t@:\n";
        let hints = depth_hints(text);
        assert_eq!(hints, vec!["depth 3".to_string()]);
    }

    #[test]
    fn test_no_depth_hint_below_threshold() {
        // a -> b: depth is only 1, below the threshold of 2.
        let text = "a: b\n\t@:\nb:\n\t@:\n";
        let hints = depth_hints(text);
        assert!(hints.is_empty());
    }

    #[test]
    fn test_no_depth_hint_for_referenced_target() {
        // 'b' is depth 1, but it's referenced by 'a', so it isn't top-level.
        // 'a' is depth 2 and *is* top-level.
        let text = "a: b\n\t@:\nb: c\n\t@:\nc: d\n\t@:\nd:\n\t@:\n";
        let hints = depth_hints(text);
        assert_eq!(hints, vec!["depth 3".to_string()]);
    }

    #[test]
    fn test_no_depth_hint_for_multi_target_rule() {
        // `a b: c` has two targets; we skip the hint to avoid ambiguity.
        let text = "a b: c\n\t@:\nc: d\n\t@:\nd:\n\t@:\n";
        let hints = depth_hints(text);
        assert!(hints.is_empty());
    }

    #[test]
    fn test_no_depth_hint_for_pattern_rule() {
        let text = "%.o: %.c\n\t@:\nfoo.c: bar.c\n\t@:\nbar.c:\n\t@:\n";
        let hints = depth_hints(text);
        // foo.c has depth 1 -> below threshold, no hint. Pattern rule excluded.
        assert!(hints.is_empty());
    }

    #[test]
    fn test_depth_hint_anchored_at_end_of_target() {
        let text = "all: build\n\t@:\nbuild: dep\n\t@:\ndep:\n\t@:\n";
        let hints = get_hints(text);
        let depth: Vec<_> = hints
            .iter()
            .filter(|h| matches!(&h.label, InlayHintLabel::String(s) if s.starts_with("depth ")))
            .collect();
        assert_eq!(depth.len(), 1);
        // 'all' is on line 0, end of "all" is column 3.
        assert_eq!(depth[0].position.line, 0);
        assert_eq!(depth[0].position.character, 3);
    }

    #[test]
    fn test_hint_truncation() {
        let long_value = "a".repeat(60);
        let text = format!("VAR := {}\nOTHER := $(VAR)\n", long_value);
        let hints = get_hints(&text);
        assert_eq!(hints.len(), 1);
        if let InlayHintLabel::String(s) = &hints[0].label {
            assert!(s.contains("..."));
            assert!(s.len() < 50);
        }
    }
}
