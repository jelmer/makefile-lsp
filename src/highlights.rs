//! Document highlights for Makefiles.
//!
//! Highlights all occurrences of the symbol under the cursor within the same file.

use makefile_lossless::Makefile;
use tower_lsp_server::ls_types::{DocumentHighlight, DocumentHighlightKind, Position, Uri};

use crate::references::find_document_references;

/// Find all highlights for the symbol at the given position.
///
/// Returns document highlights with `Write` kind for definitions and
/// `Read` kind for usages.
pub fn get_highlights(
    makefile: &Makefile,
    source_text: &str,
    position: Position,
    uri: &Uri,
) -> Vec<DocumentHighlight> {
    let locations = find_document_references(makefile, source_text, position, uri, true);

    locations
        .into_iter()
        .enumerate()
        .map(|(i, loc)| {
            // First location is typically the definition
            let kind = if i == 0 {
                DocumentHighlightKind::WRITE
            } else {
                DocumentHighlightKind::READ
            };
            DocumentHighlight {
                range: loc.range,
                kind: Some(kind),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::ls_types::Range;

    fn get_hl(text: &str, pos: Position) -> Vec<DocumentHighlight> {
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let uri: Uri = "file:///test/Makefile".parse().unwrap();
        get_highlights(&makefile, text, pos, &uri)
    }

    #[test]
    fn test_highlight_variable() {
        let text = "CC = gcc\nall:\n\t$(CC) main.c\n";
        let highlights = get_hl(text, Position::new(0, 0));
        assert_eq!(highlights.len(), 2);
        assert_eq!(highlights[0].kind, Some(DocumentHighlightKind::WRITE));
        assert_eq!(highlights[1].kind, Some(DocumentHighlightKind::READ));
    }

    #[test]
    fn test_highlight_target() {
        let text = "all: build\n\nbuild:\n\techo ok\n";
        let highlights = get_hl(text, Position::new(0, 5));
        assert_eq!(highlights.len(), 2);
    }

    #[test]
    fn test_highlight_second_target_of_rule() {
        let text = "all: b\na b: c\n";
        let ranges: Vec<Range> = get_hl(text, Position::new(1, 2))
            .into_iter()
            .map(|h| h.range)
            .collect();
        assert_eq!(
            ranges,
            vec![
                Range::new(Position::new(0, 5), Position::new(0, 6)),
                Range::new(Position::new(1, 2), Position::new(1, 3)),
            ]
        );
    }

    #[test]
    fn test_highlight_substitution_reference() {
        let text = "FOO = a.c\nX = $(FOO:.c=.o)\n";
        let ranges: Vec<Range> = get_hl(text, Position::new(1, 7))
            .into_iter()
            .map(|h| h.range)
            .collect();
        assert_eq!(
            ranges,
            vec![
                Range::new(Position::new(0, 0), Position::new(0, 3)),
                Range::new(Position::new(1, 6), Position::new(1, 9)),
            ]
        );
    }

    #[test]
    fn test_highlight_escaped_prerequisite() {
        let text = "all: a\\#b\na\\#b:\n";
        let ranges: Vec<Range> = get_hl(text, Position::new(0, 8))
            .into_iter()
            .map(|h| h.range)
            .collect();
        assert_eq!(
            ranges,
            vec![
                Range::new(Position::new(0, 5), Position::new(0, 9)),
                Range::new(Position::new(1, 0), Position::new(1, 4)),
            ]
        );
    }

    #[test]
    fn test_highlight_nothing() {
        let text = "all:\n\techo hello\n";
        let highlights = get_hl(text, Position::new(1, 2));
        assert!(highlights.is_empty());
    }
}
