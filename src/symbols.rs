//! Document symbol generation for Makefiles.

use makefile_lossless::Makefile;
use rowan::ast::AstNode;
use tower_lsp_server::ls_types::{DocumentSymbol, SymbolKind};

use crate::position::text_range_to_lsp_range;

/// Generate document symbols for a Makefile.
///
/// Returns rules as Function symbols and variable definitions as Variable symbols.
#[allow(deprecated)] // DocumentSymbol::deprecated field
pub fn generate_document_symbols(makefile: &Makefile, source_text: &str) -> Vec<DocumentSymbol> {
    let mut symbols = Vec::new();

    for rule in makefile.rules() {
        let targets: Vec<String> = rule.targets().collect();
        let name = targets.join(", ");
        if name.is_empty() {
            continue;
        }

        let range = text_range_to_lsp_range(source_text, rule.syntax().text_range());

        // Use the targets portion for the selection range
        let selection_range = range;

        symbols.push(DocumentSymbol {
            name,
            detail: None,
            kind: SymbolKind::FUNCTION,
            tags: None,
            deprecated: None,
            range,
            selection_range,
            children: None,
        });
    }

    for var in makefile.variable_definitions() {
        let Some(name) = var.name() else {
            continue;
        };

        let range = text_range_to_lsp_range(source_text, var.syntax().text_range());
        let detail = var.raw_value().map(|v| v.trim().to_string());

        symbols.push(DocumentSymbol {
            name,
            detail,
            kind: SymbolKind::VARIABLE,
            tags: None,
            deprecated: None,
            range,
            selection_range: range,
            children: None,
        });
    }

    symbols
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_symbols_rules() {
        let text = "all: build\n\techo all\n\nclean:\n\trm -rf build\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let symbols = generate_document_symbols(&makefile, text);

        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"all"));
        assert!(names.contains(&"clean"));
    }

    #[test]
    fn test_symbols_variables() {
        let text = "CC = gcc\nCFLAGS = -Wall -O2\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let symbols = generate_document_symbols(&makefile, text);

        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].name, "CC");
        assert_eq!(symbols[0].kind, SymbolKind::VARIABLE);
        assert_eq!(symbols[1].name, "CFLAGS");
    }

    #[test]
    fn test_symbols_empty() {
        let text = "";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let symbols = generate_document_symbols(&makefile, text);
        assert!(symbols.is_empty());
    }

    #[test]
    fn test_symbols_mixed() {
        let text = "CC = gcc\n\nall: main.o\n\t$(CC) -o $@ $^\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let symbols = generate_document_symbols(&makefile, text);

        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].kind, SymbolKind::FUNCTION); // rule
        assert_eq!(symbols[1].kind, SymbolKind::VARIABLE); // CC
    }
}
