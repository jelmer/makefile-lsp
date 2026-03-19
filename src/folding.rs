//! Folding range generation for Makefiles.

use makefile_lossless::Makefile;
use rowan::ast::AstNode;
use tower_lsp_server::ls_types::{FoldingRange, FoldingRangeKind};

use crate::position::offset_to_position;

/// Generate folding ranges for a Makefile.
///
/// Rules with multiple recipe lines and conditionals are foldable.
pub fn generate_folding_ranges(makefile: &Makefile, source_text: &str) -> Vec<FoldingRange> {
    let mut ranges = Vec::new();

    for rule in makefile.rules() {
        let text_range = rule.syntax().text_range();
        let start = offset_to_position(source_text, text_range.start());
        let end = offset_to_position(source_text, text_range.end());

        // Only fold if the rule spans multiple lines
        if end.line > start.line {
            ranges.push(FoldingRange {
                start_line: start.line,
                start_character: Some(start.character),
                end_line: end.line,
                end_character: Some(end.character),
                kind: Some(FoldingRangeKind::Region),
                collapsed_text: None,
            });
        }
    }

    for cond in makefile.conditionals() {
        let text_range = cond.syntax().text_range();
        let start = offset_to_position(source_text, text_range.start());
        let end = offset_to_position(source_text, text_range.end());

        if end.line > start.line {
            ranges.push(FoldingRange {
                start_line: start.line,
                start_character: Some(start.character),
                end_line: end.line,
                end_character: Some(end.character),
                kind: Some(FoldingRangeKind::Region),
                collapsed_text: None,
            });
        }
    }

    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rule_folding() {
        let text = "all: build\n\techo step1\n\techo step2\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let ranges = generate_folding_ranges(&makefile, text);

        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].start_line, 0);
    }

    #[test]
    fn test_no_folding_single_line() {
        let text = "CC = gcc\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let ranges = generate_folding_ranges(&makefile, text);
        assert!(ranges.is_empty());
    }

    #[test]
    fn test_empty_file() {
        let text = "";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let ranges = generate_folding_ranges(&makefile, text);
        assert!(ranges.is_empty());
    }
}
