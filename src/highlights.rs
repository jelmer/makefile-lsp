//! Document highlights for Makefiles.
//!
//! Highlights all occurrences of the symbol under the cursor within the same file.

use makefile_lossless::Makefile;
use tower_lsp_server::ls_types::{DocumentHighlight, DocumentHighlightKind, Position, Uri};

use crate::references::find_references;

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
    let locations = find_references(makefile, source_text, position, uri, true);

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
    fn test_highlight_nothing() {
        let text = "all:\n\techo hello\n";
        let highlights = get_hl(text, Position::new(1, 2));
        assert!(highlights.is_empty());
    }
}
