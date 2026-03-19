//! Diagnostics for Makefile files.

use makefile_lossless::Parse;
use tower_lsp_server::ls_types::{Diagnostic, DiagnosticSeverity, NumberOrString};

/// Collect diagnostics from parse errors.
pub fn get_diagnostics(
    _source_text: &str,
    parsed: &Parse<makefile_lossless::Makefile>,
) -> Vec<Diagnostic> {
    parsed
        .errors()
        .iter()
        .map(|error| {
            // Compute a range from the error's line number
            let line = error.line.saturating_sub(1) as u32;
            let range = tower_lsp_server::ls_types::Range {
                start: tower_lsp_server::ls_types::Position::new(line, 0),
                end: tower_lsp_server::ls_types::Position::new(line, error.context.len() as u32),
            };

            Diagnostic {
                range,
                severity: Some(DiagnosticSeverity::ERROR),
                code: Some(NumberOrString::String("parse-error".to_string())),
                source: Some("makefile-lsp".to_string()),
                message: error.message.clone(),
                ..Default::default()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use makefile_lossless::Makefile;

    #[test]
    fn test_valid_makefile_no_diagnostics() {
        let text = "all: build\n\techo done\n";
        let parsed = Makefile::parse(text);
        let diagnostics = get_diagnostics(text, &parsed);
        assert!(diagnostics.is_empty());
    }
}
