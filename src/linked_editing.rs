//! Linked editing ranges for variable names.
//!
//! Editing a variable name edits the other occurrences of it in the same
//! document along with it.

use makefile_lossless::TextRange;
use tower_lsp_server::ls_types::{LinkedEditingRanges, Position};

use crate::position::{text_range_to_lsp_range, try_position_to_offset};
use crate::references::{single_char_reference_ranges, symbol_locations, Symbol};
use crate::rename::renameable_symbol;
use crate::workspace::FileSet;

/// Characters that may be typed into a linked variable name. Whitespace and
/// the characters that end a name or a reference are left out.
pub const WORD_PATTERN: &str = r"[^\s:#=$(){}\\]+";

/// The occurrences of the variable at `position` in the current document.
///
/// These are the occurrences a rename would edit, so nothing is returned
/// for variables that can't be renamed. Since only the current document is
/// edited, nothing is returned either for variables that also occur in the
/// files it includes, or that are written as `$X` somewhere, which would
/// turn into a reference to another variable once the name gets longer.
/// Computed names such as `$(P)_FLAGS` aren't linked either.
pub fn linked_editing_ranges(files: &FileSet, position: Position) -> Option<LinkedEditingRanges> {
    let symbol = renameable_symbol(files, position)?.ok()?;
    if !matches!(symbol, Symbol::Variable(_)) {
        return None;
    }
    if files
        .others()
        .any(|d| !symbol_locations(&d.makefile(), d.text(), d.uri(), &symbol, true).is_empty())
    {
        return None;
    }

    let current = files.current();
    let makefile = current.makefile();
    let text = current.text();
    let offset = |pos| try_position_to_offset(text, pos).expect("symbol location outside document");
    let ranges: Vec<TextRange> = symbol_locations(&makefile, text, current.uri(), &symbol, true)
        .into_iter()
        .map(|loc| TextRange::new(offset(loc.range.start), offset(loc.range.end)))
        .collect();
    let single_char = single_char_reference_ranges(&makefile);
    if ranges.iter().any(|r| single_char.contains(r)) {
        return None;
    }
    // A defined name containing a reference, such as `$(P)_FLAGS`, is
    // computed.
    if makefile.variable_references().any(|reference| {
        ranges
            .iter()
            .any(|r| r.contains_range(reference.text_range()))
    }) {
        return None;
    }
    // Linked ranges must all hold the same text.
    let first = &text[*ranges.first()?];
    if ranges.iter().any(|r| text[*r] != *first) {
        return None;
    }

    Some(LinkedEditingRanges {
        ranges: ranges
            .into_iter()
            .map(|r| text_range_to_lsp_range(text, r))
            .collect(),
        word_pattern: Some(WORD_PATTERN.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::tests::Fixture;
    use crate::workspace::Document;
    use tower_lsp_server::ls_types::{Range, Uri};

    fn single(text: &str) -> FileSet {
        let uri: Uri = "file:///test/Makefile".parse().unwrap();
        FileSet::single(Document::new(uri, text.to_string()))
    }

    fn ranges(text: &str, line: u32, character: u32) -> Option<Vec<Range>> {
        linked_editing_ranges(&single(text), Position::new(line, character)).map(|r| {
            assert_eq!(r.word_pattern.as_deref(), Some(WORD_PATTERN));
            r.ranges
        })
    }

    fn range(line: u32, start: u32, end: u32) -> Range {
        Range::new(Position::new(line, start), Position::new(line, end))
    }

    #[test]
    fn test_variable_from_definition() {
        let text = "CC = gcc\nall:\n\t$(CC) main.c ${CC}\n";
        assert_eq!(
            ranges(text, 0, 1),
            Some(vec![range(0, 0, 2), range(2, 3, 5), range(2, 16, 18)])
        );
    }

    #[test]
    fn test_variable_from_reference() {
        let text = "CC = gcc\nall:\n\t$(CC) main.c\n";
        assert_eq!(
            ranges(text, 2, 5),
            Some(vec![range(0, 0, 2), range(2, 3, 5)])
        );
    }

    #[test]
    fn test_ifdef_and_bsd_condition() {
        let text = "FOO = 1\nifdef FOO\nendif\n";
        assert_eq!(
            ranges(text, 1, 6),
            Some(vec![range(0, 0, 3), range(1, 6, 9)])
        );
        let text = "FOO = 1\n.if defined(FOO)\n.endif\n";
        assert_eq!(
            ranges(text, 0, 0),
            Some(vec![range(0, 0, 3), range(1, 12, 15)])
        );
    }

    #[test]
    fn test_nested_reference() {
        let text = "B = x\nA_x = 1\nall:\n\t$(A_$(B))\n";
        assert_eq!(
            ranges(text, 3, 7),
            Some(vec![range(0, 0, 1), range(3, 7, 8)])
        );
    }

    #[test]
    fn test_none_for_computed_name() {
        let text = "B = x\nA_x = 1\nall:\n\t$(A_$(B))\n";
        assert_eq!(ranges(text, 3, 3), None);
    }

    #[test]
    fn test_none_for_computed_definition() {
        let text = "P = A\n$(P)_FLAGS = 1\nall:\n\t$(A_FLAGS)\n";
        assert_eq!(ranges(text, 1, 6), None);
    }

    #[test]
    fn test_none_for_undefined_variable() {
        assert_eq!(ranges("all:\n\t$(CC) x\n", 1, 3), None);
    }

    #[test]
    fn test_none_for_automatic_variable() {
        assert_eq!(ranges("all:\n\techo $@\n", 1, 7), None);
    }

    #[test]
    fn test_none_for_call_parameter() {
        assert_eq!(ranges("f = $(1)\n", 0, 6), None);
    }

    #[test]
    fn test_none_for_target() {
        assert_eq!(ranges("all: build\nbuild:\n", 1, 0), None);
    }

    #[test]
    fn test_none_with_single_char_reference() {
        // Typing into `$X` would turn it into a reference to another variable.
        let text = "X = 1\nall:\n\techo $X $(X)\n";
        assert_eq!(ranges(text, 0, 0), None);
    }

    #[test]
    fn test_none_for_nothing() {
        assert_eq!(ranges("all:\n\techo hi\n", 1, 3), None);
    }

    #[test]
    fn test_none_for_variable_used_in_other_file() {
        let fx = Fixture::new(&[
            ("Makefile", "CC = gcc\ninclude rules.mk\nall:\n\t$(CC)\n"),
            ("rules.mk", "x:\n\t$(CC)\n"),
        ]);
        let set = fx.file_set("Makefile");
        assert_eq!(linked_editing_ranges(&set, Position::new(0, 0)), None);
    }

    #[test]
    fn test_none_for_variable_defined_in_other_file() {
        let fx = Fixture::new(&[
            ("Makefile", "include rules.mk\nall:\n\t$(CC)\n"),
            ("rules.mk", "CC = gcc\n"),
        ]);
        let set = fx.file_set("Makefile");
        assert_eq!(linked_editing_ranges(&set, Position::new(2, 3)), None);
    }

    #[test]
    fn test_variable_not_in_included_file() {
        let fx = Fixture::new(&[
            ("Makefile", "CC = gcc\ninclude rules.mk\nall:\n\t$(CC)\n"),
            ("rules.mk", "x:\n\techo\n"),
        ]);
        let set = fx.file_set("Makefile");
        assert_eq!(
            linked_editing_ranges(&set, Position::new(3, 3)).map(|r| r.ranges),
            Some(vec![range(0, 0, 2), range(3, 3, 5)])
        );
    }
}
