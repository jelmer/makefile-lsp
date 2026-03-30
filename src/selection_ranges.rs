//! Selection range support for Makefiles.
//!
//! Provides smart expand/shrink selection: from word to enclosing expression,
//! to enclosing item (rule/variable/conditional), to the whole file.

use makefile_lossless::Makefile;
use rowan::ast::AstNode;
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
    let root_range = text_range_to_lsp_range(source_text, makefile.syntax().text_range());
    let mut result = SelectionRange {
        range: root_range,
        parent: None,
    };

    let Some(offset) = try_position_to_offset(source_text, position) else {
        return result;
    };

    // Walk the syntax tree from root to leaf, collecting enclosing nodes
    let mut ranges = vec![makefile.syntax().text_range()];

    let mut node = makefile.syntax().clone();
    loop {
        let child = node
            .children()
            .find(|c| c.text_range().contains_inclusive(offset));
        match child {
            Some(c) => {
                if c.text_range() != node.text_range() {
                    ranges.push(c.text_range());
                }
                node = c;
            }
            None => break,
        }
    }

    // Also try to find a token at the offset for the tightest range
    if let Some(token) = makefile.syntax().token_at_offset(offset).right_biased() {
        if token.text_range() != *ranges.last().unwrap() {
            ranges.push(token.text_range());
        }
    }

    // Build the chain from innermost to outermost
    for text_range in ranges.into_iter() {
        let lsp_range = text_range_to_lsp_range(source_text, text_range);
        result = SelectionRange {
            range: lsp_range,
            parent: Some(Box::new(result)),
        };
    }

    result
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

    fn chain_depth(sel: &SelectionRange) -> usize {
        let mut depth = 1;
        let mut current = sel;
        while let Some(parent) = &current.parent {
            depth += 1;
            current = parent;
        }
        depth
    }

    #[test]
    fn test_selection_range_in_rule() {
        let text = "all: build\n\techo done\n";
        let sel = get_sel(text, Position::new(0, 0));
        // Should have multiple levels: token → targets → rule → root
        assert!(chain_depth(&sel) >= 3);
    }

    #[test]
    fn test_selection_range_in_variable() {
        let text = "CC = gcc\n";
        let sel = get_sel(text, Position::new(0, 0));
        assert!(chain_depth(&sel) >= 2);
    }

    #[test]
    fn test_selection_range_empty() {
        let text = "";
        let sel = get_sel(text, Position::new(0, 0));
        assert!(chain_depth(&sel) >= 1);
    }

    #[test]
    fn test_innermost_range_is_narrow() {
        let text = "CC = gcc\nall: build\n\techo done\n";
        let sel = get_sel(text, Position::new(0, 0));
        // Innermost range should be narrow (just the token "CC")
        assert_eq!(sel.range.start.line, 0);
        assert!(sel.range.end.character - sel.range.start.character <= 2);
    }
}
