//! Selection range support for Makefiles.
//!
//! Provides smart expand/shrink selection: from word to enclosing expression,
//! to enclosing item (rule/variable/conditional), to the whole file.

use makefile_lossless::{Makefile, TextRange, TextSize};
use tower_lsp_server::ls_types::{Position, SelectionRange};

use crate::position::{text_range_to_lsp_range, try_position_to_offset};

/// Get selection ranges for the given positions.
///
/// For each position, returns a nested chain of selection ranges from the
/// most specific (innermost node) to the least specific (root).
pub fn get_selection_ranges(
    makefile: &Makefile,
    source_text: &str,
    positions: &[Position],
) -> Vec<SelectionRange> {
    positions
        .iter()
        .map(|pos| selection_range_at(makefile, source_text, *pos))
        .collect()
}

fn selection_range_at(
    makefile: &Makefile,
    source_text: &str,
    position: Position,
) -> SelectionRange {
    let mut result = SelectionRange {
        range: text_range_to_lsp_range(source_text, makefile.text_range()),
        parent: None,
    };
    let Some(offset) = try_position_to_offset(source_text, position) else {
        return result;
    };
    for text_range in enclosing_ranges(makefile, offset).into_iter().skip(1) {
        result = SelectionRange {
            range: text_range_to_lsp_range(source_text, text_range),
            parent: Some(Box::new(result)),
        };
    }
    result
}

/// The ranges of the constructs around `offset`, from the whole file in,
/// each one inside the previous one.
fn enclosing_ranges(makefile: &Makefile, offset: TextSize) -> Vec<TextRange> {
    // Items, which own their line ending, contain the offset when it is
    // before their end; parts of them, such as names, also when it is right
    // after them, where the cursor is after typing a word.
    let mut items: Vec<TextRange> = Vec::new();
    let mut parts: Vec<TextRange> = makefile.comment_ranges().collect();
    for cond in makefile.all_conditionals() {
        items.push(cond.text_range());
        for branch in cond.branches() {
            items.push(branch.text_range());
            parts.push(branch.directive_range());
            parts.extend(branch.keyword_range());
        }
        parts.extend(cond.endif_range());
    }
    for rule in makefile.rules() {
        items.push(rule.text_range());
        parts.extend(rule.target_ranges());
        parts.extend(rule.prerequisite_list_range());
        parts.extend(rule.prerequisite_ranges());
        parts.extend(rule.order_only_prerequisite_ranges());
    }
    items.extend(makefile.recipe_nodes().map(|recipe| recipe.text_range()));
    for def in makefile.variable_definitions() {
        items.push(def.text_range());
        parts.extend(def.keyword_ranges().into_iter().map(|(_, range)| range));
        parts.extend(def.name_ranges());
        parts.extend(def.value_range());
    }
    for include in makefile.includes() {
        items.push(include.text_range());
        parts.extend(include.keyword_range());
        parts.extend(include.path_ranges());
    }
    for vpath in makefile.vpaths() {
        items.push(vpath.text_range());
        parts.extend(vpath.keyword_range());
    }
    for load in makefile.loads() {
        items.push(load.text_range());
        parts.extend(load.keyword_range());
    }
    items.extend(
        makefile
            .expression_statements()
            .map(|statement| statement.text_range()),
    );
    for reference in makefile.variable_references() {
        parts.push(reference.text_range());
        parts.extend(reference.name_range());
    }

    let mut ranges: Vec<TextRange> = items
        .into_iter()
        .filter(|range| range.contains(offset))
        .chain(
            parts
                .into_iter()
                .filter(|range| range.contains_inclusive(offset)),
        )
        .collect();
    // Outer ranges first.
    ranges.sort_by_key(|range| (range.start(), std::cmp::Reverse(range.end())));
    let mut chain = vec![makefile.text_range()];
    for range in ranges {
        let last = chain[chain.len() - 1];
        if range != last && last.contains_range(range) {
            chain.push(range);
        }
    }
    chain
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get_sel(text: &str, pos: Position) -> SelectionRange {
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        get_selection_ranges(&makefile, text, &[pos])
            .into_iter()
            .next()
            .unwrap()
    }

    /// The ranges of the chain at `pos`, from the innermost one out, as
    /// (start line, start character, end line, end character).
    fn chain(text: &str, pos: Position) -> Vec<(u32, u32, u32, u32)> {
        let mut out = Vec::new();
        let mut current = Some(&get_sel(text, pos));
        while let Some(sel) = current {
            let r = sel.range;
            out.push((r.start.line, r.start.character, r.end.line, r.end.character));
            current = sel.parent.as_deref();
        }
        out
    }

    #[test]
    fn test_start_of_second_item() {
        assert_eq!(
            chain("A = 1\nB = 2\n", Position::new(1, 0)),
            vec![(1, 0, 1, 1), (1, 0, 2, 0), (0, 0, 2, 0)]
        );
    }

    #[test]
    fn test_selection_range_in_rule() {
        let text = "all: build\n\techo $(CC) done\n";
        assert_eq!(
            chain(text, Position::new(0, 0)),
            vec![(0, 0, 0, 3), (0, 0, 2, 0)]
        );
        assert_eq!(
            chain(text, Position::new(0, 6)),
            vec![(0, 5, 0, 10), (0, 4, 0, 10), (0, 0, 2, 0)]
        );
        assert_eq!(
            chain(text, Position::new(1, 8)),
            vec![(1, 8, 1, 10), (1, 6, 1, 11), (1, 0, 2, 0), (0, 0, 2, 0)]
        );
    }

    #[test]
    fn test_selection_range_in_variable() {
        let text = "export CC = gcc -O2\nX = 1\n";
        assert_eq!(
            chain(text, Position::new(0, 8)),
            vec![(0, 7, 0, 9), (0, 0, 1, 0), (0, 0, 2, 0)]
        );
        assert_eq!(
            chain(text, Position::new(0, 2)),
            vec![(0, 0, 0, 6), (0, 0, 1, 0), (0, 0, 2, 0)]
        );
        assert_eq!(
            chain(text, Position::new(0, 13)),
            vec![(0, 12, 0, 19), (0, 0, 1, 0), (0, 0, 2, 0)]
        );
    }

    #[test]
    fn test_selection_range_in_conditional() {
        let text = "ifdef A\nX = 1\nelse\nY = 2\nendif\nZ = 3\n";
        assert_eq!(
            chain(text, Position::new(3, 0)),
            vec![
                (3, 0, 3, 1),
                (3, 0, 4, 0),
                (2, 0, 4, 0),
                (0, 0, 5, 0),
                (0, 0, 6, 0)
            ]
        );
        assert_eq!(
            chain(text, Position::new(0, 2)),
            vec![
                (0, 0, 0, 5),
                (0, 0, 0, 7),
                (0, 0, 2, 0),
                (0, 0, 5, 0),
                (0, 0, 6, 0)
            ]
        );
    }

    #[test]
    fn test_selection_range_empty() {
        assert_eq!(chain("", Position::new(0, 0)), vec![(0, 0, 0, 0)]);
    }

    #[test]
    fn test_innermost_range_is_narrow() {
        let text = "CC = gcc\nall: build\n\techo done\n";
        assert_eq!(
            chain(text, Position::new(0, 0)),
            vec![(0, 0, 0, 2), (0, 0, 1, 0), (0, 0, 3, 0)]
        );
    }
}
