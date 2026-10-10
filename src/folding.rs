//! Folding range generation for Makefiles.

use makefile_lossless::{Makefile, TextRange, TextSize};
use tower_lsp_server::ls_types::{FoldingRange, FoldingRangeKind};

use crate::position::offset_to_position;

/// Create a folding range from a text range, returning `None` if it's a single line.
///
/// Node ranges include the terminating newline (and for rules, trailing blank
/// lines), which would put the end on the following line. LSP treats
/// `end_line` as inclusive, so the range is trimmed to its last non-blank line.
fn make_folding_range(
    source_text: &str,
    text_range: TextRange,
    kind: FoldingRangeKind,
) -> Option<FoldingRange> {
    let content = source_text[text_range].trim_end();
    let start = offset_to_position(source_text, text_range.start());
    let end = offset_to_position(source_text, text_range.start() + TextSize::of(content));

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
            rule.text_range(),
            FoldingRangeKind::Region,
        ));
    }

    for cond in makefile.all_conditionals() {
        ranges.extend(make_folding_range(
            source_text,
            cond.text_range(),
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
        assert_eq!(
            get_ranges("all: build\n\techo step1\n\techo step2\n"),
            vec![FoldingRange {
                start_line: 0,
                start_character: Some(0),
                end_line: 2,
                end_character: Some(11),
                kind: Some(FoldingRangeKind::Region),
                collapsed_text: None,
            }]
        );
    }

    fn lines(text: &str) -> Vec<(u32, u32)> {
        get_ranges(text)
            .iter()
            .map(|r| (r.start_line, r.end_line))
            .collect()
    }

    #[test]
    fn test_single_line_rule_no_folding() {
        assert_eq!(lines("all:\nfoo:\n"), vec![]);
    }

    #[test]
    fn test_rule_folding_excludes_trailing_blank_lines() {
        assert_eq!(
            lines("all: build\n\techo step1\n\techo step2\n\n\nfoo:\n"),
            vec![(0, 2)]
        );
    }

    #[test]
    fn test_rule_folding_no_trailing_newline() {
        let ranges = get_ranges("all: build\n\techo step1\n\techo step2");
        assert_eq!(
            ranges
                .iter()
                .map(|r| (r.start_line, r.end_line, r.end_character))
                .collect::<Vec<_>>(),
            vec![(0, 2, Some(11))]
        );
    }

    #[test]
    fn test_rule_folding_crlf() {
        let ranges = get_ranges("all: build\r\n\techo step1\r\n\techo step2\r\n\r\nfoo:\r\n");
        assert_eq!(
            ranges
                .iter()
                .map(|r| (r.start_line, r.end_line, r.end_character))
                .collect::<Vec<_>>(),
            vec![(0, 2, Some(11))]
        );
    }

    #[test]
    fn test_conditional_folding() {
        let ranges = get_ranges("ifdef X\nA = 1\nB = 2\nC = 3\nendif\nall:\n");
        assert_eq!(
            ranges
                .iter()
                .map(|r| (r.start_line, r.end_line, r.end_character))
                .collect::<Vec<_>>(),
            vec![(0, 4, Some(5))]
        );
    }

    #[test]
    fn test_conditional_folding_with_else() {
        assert_eq!(lines("ifdef X\nA = 1\nelse\nB = 2\nendif\n"), vec![(0, 4)]);
    }

    #[test]
    fn test_conditional_folding_crlf() {
        assert_eq!(
            lines("ifdef X\r\nA = 1\r\nendif\r\nB = 2\r\n"),
            vec![(0, 2)]
        );
    }

    #[test]
    fn test_conditional_folding_no_trailing_newline() {
        assert_eq!(lines("ifdef X\nA = 1\nendif"), vec![(0, 2)]);
    }

    #[test]
    fn test_unterminated_conditional_folding() {
        assert_eq!(lines("ifdef X\nA = 1\n"), vec![(0, 1)]);
    }

    #[test]
    fn test_bsd_conditional_folding() {
        assert_eq!(
            lines(".if defined(X)\nA = 1\n.endif\nB = 2\n"),
            vec![(0, 2)]
        );
    }

    #[test]
    fn test_rule_inside_conditional_folding() {
        assert_eq!(
            lines("ifdef X\nall:\n\techo a\nendif\n"),
            vec![(1, 2), (0, 3)]
        );
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
    fn test_nested_conditional_folding() {
        let text = "ifdef A\nifdef B\nX = 1\nendif\nendif\nall:\nifdef C\n\techo c\nendif\n";
        assert_eq!(lines(text), vec![(5, 8), (0, 4), (1, 3), (6, 8)]);
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
