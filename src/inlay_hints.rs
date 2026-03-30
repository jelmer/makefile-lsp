//! Inlay hints for Makefiles.
//!
//! Shows the assignment operator flavor for variable definitions as a hint,
//! helping distinguish between recursively-expanded (=), simply-expanded (:=),
//! conditional (?=), and append (+=) assignments.

use makefile_lossless::Makefile;
use tower_lsp_server::ls_types::{InlayHint, InlayHintKind, InlayHintLabel, Range};

use crate::position::text_range_to_lsp_range;

/// Generate inlay hints for a Makefile within the given range.
pub fn get_inlay_hints(makefile: &Makefile, source_text: &str, range: Range) -> Vec<InlayHint> {
    let mut hints = Vec::new();

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
            format!("{}…", &value[..39])
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

    #[test]
    fn test_hint_truncation() {
        let long_value = "a".repeat(60);
        let text = format!("VAR := {}\nOTHER := $(VAR)\n", long_value);
        let hints = get_hints(&text);
        assert_eq!(hints.len(), 1);
        if let InlayHintLabel::String(s) = &hints[0].label {
            assert!(s.contains('…'));
            assert!(s.len() < 50);
        }
    }
}
