//! Folding range generation for Makefiles.

use makefile_lossless::Makefile;
use rowan::ast::AstNode;
use tower_lsp_server::ls_types::{FoldingRange, FoldingRangeKind};

use crate::position::offset_to_position;

/// Create a folding range from a text range, returning `None` if it's a single line.
fn make_folding_range(
    source_text: &str,
    text_range: rowan::TextRange,
    kind: FoldingRangeKind,
) -> Option<FoldingRange> {
    let start = offset_to_position(source_text, text_range.start());
    let end = offset_to_position(source_text, text_range.end());

    (end.line > start.line).then_some(FoldingRange {
        start_line: start.line,
        start_character: Some(start.character),
        end_line: end.line,
        end_character: Some(end.character),
        kind: Some(kind),
        collapsed_text: None,
    })
}

/// Generate folding ranges for a Makefile.
///
/// Foldable regions: multi-line rules, conditionals, and consecutive comment blocks.
pub fn generate_folding_ranges(makefile: &Makefile, source_text: &str) -> Vec<FoldingRange> {
    let mut ranges = Vec::new();

    for rule in makefile.rules() {
        ranges.extend(make_folding_range(
            source_text,
            rule.syntax().text_range(),
            FoldingRangeKind::Region,
        ));
    }

    for cond in makefile.conditionals() {
        ranges.extend(make_folding_range(
            source_text,
            cond.syntax().text_range(),
            FoldingRangeKind::Region,
        ));
    }

    for block_range in makefile.comment_blocks() {
        ranges.extend(make_folding_range(
            source_text,
            block_range,
            FoldingRangeKind::Comment,
        ));
    }

    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get_ranges(text: &str) -> Vec<FoldingRange> {
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        generate_folding_ranges(&makefile, text)
    }

    #[test]
    fn test_rule_folding() {
        let ranges = get_ranges("all: build\n\techo step1\n\techo step2\n");
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].start_line, 0);
        assert_eq!(ranges[0].kind, Some(FoldingRangeKind::Region));
    }

    #[test]
    fn test_no_folding_single_line() {
        assert!(get_ranges("CC = gcc\n").is_empty());
    }

    #[test]
    fn test_empty_file() {
        assert!(get_ranges("").is_empty());
    }

    #[test]
    fn test_comment_block_folding() {
        let text = "# This is a\n# multi-line comment\n# block\nall:\n\techo done\n";
        let ranges = get_ranges(text);
        let comments: Vec<_> = ranges
            .iter()
            .filter(|r| r.kind == Some(FoldingRangeKind::Comment))
            .collect();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].start_line, 0);
        assert_eq!(comments[0].end_line, 2);
    }

    #[test]
    fn test_single_comment_no_folding() {
        let text = "# just one comment\nall:\n\techo done\n";
        let ranges = get_ranges(text);
        let comments: Vec<_> = ranges
            .iter()
            .filter(|r| r.kind == Some(FoldingRangeKind::Comment))
            .collect();
        assert!(comments.is_empty());
    }

    #[test]
    fn test_multiple_comment_blocks() {
        let text = "# block 1\n# block 1\n\nCC = gcc\n\n# block 2\n# block 2\n";
        let ranges = get_ranges(text);
        let comments: Vec<_> = ranges
            .iter()
            .filter(|r| r.kind == Some(FoldingRangeKind::Comment))
            .collect();
        assert_eq!(comments.len(), 2);
    }
}
