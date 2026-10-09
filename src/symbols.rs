//! Document and workspace symbol generation for Makefiles.

use makefile_lossless::Makefile;
use tower_lsp_server::ls_types::{DocumentSymbol, Location, OneOf, SymbolKind, WorkspaceSymbol};

use crate::builtins::find_special_target;
use crate::position::text_range_to_lsp_range;
use crate::targets::targets_with_ranges;
use crate::workspace::Document;

/// Upper bound on the number of symbols returned for a workspace query.
const MAX_WORKSPACE_SYMBOLS: usize = 1000;

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

        let range = text_range_to_lsp_range(source_text, rule.text_range());
        let selection_range = rule
            .target_ranges()
            .reduce(|a, b| a.cover(b))
            .map_or(range, |r| text_range_to_lsp_range(source_text, r));

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

        let range = text_range_to_lsp_range(source_text, var.text_range());
        let selection_range = var
            .name_range()
            .map_or(range, |r| text_range_to_lsp_range(source_text, r));
        let detail = var.raw_value().map(|v| v.trim().to_string());

        symbols.push(DocumentSymbol {
            name,
            detail,
            kind: SymbolKind::VARIABLE,
            tags: None,
            deprecated: None,
            range,
            selection_range,
            children: None,
        });
    }

    symbols
}

/// How well a symbol name matches a query; lower is better.
fn match_rank(name: &str, query: &str) -> Option<u8> {
    let name = name.to_lowercase();
    let query = query.to_lowercase();
    if name == query {
        Some(0)
    } else if name.starts_with(&query) {
        Some(1)
    } else if name.contains(&query) {
        Some(2)
    } else {
        let mut chars = name.chars();
        query.chars().all(|q| chars.any(|c| c == q)).then_some(3)
    }
}

/// Find the targets and variables in `docs` whose names match `query`.
///
/// Each document comes with the name to show as its container. Names match
/// when the query occurs in them, or failing that when its characters occur
/// in them in order, ignoring case. Results are ordered by how well they
/// match and then by the order of `docs`; special targets such as `.PHONY`
/// are left out.
pub fn workspace_symbols<'a>(
    docs: impl IntoIterator<Item = (&'a Document, String)>,
    query: &str,
) -> Vec<WorkspaceSymbol> {
    let mut found = Vec::new();
    for (doc, container) in docs {
        let text = doc.text();
        let makefile = doc.makefile();
        let targets = makefile
            .rules()
            .flat_map(|rule| targets_with_ranges(&rule))
            .filter(|(name, _)| find_special_target(name).is_none())
            .map(|(name, range)| (name, range, SymbolKind::FUNCTION));
        let variables = makefile.variable_definitions().filter_map(|var| {
            let name = var.name()?;
            let range = var.name_range()?;
            Some((name, range, SymbolKind::VARIABLE))
        });
        for (name, range, kind) in targets.chain(variables) {
            let Some(rank) = match_rank(&name, query) else {
                continue;
            };
            let location = Location::new(doc.uri().clone(), text_range_to_lsp_range(text, range));
            found.push((
                rank,
                WorkspaceSymbol {
                    name,
                    kind,
                    tags: None,
                    container_name: Some(container.clone()),
                    location: OneOf::Left(location),
                    data: None,
                },
            ));
        }
    }
    found.sort_by_key(|(rank, _)| *rank);
    found
        .into_iter()
        .take(MAX_WORKSPACE_SYMBOLS)
        .map(|(_, symbol)| symbol)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::ls_types::{Position, Range, Uri};

    fn doc(name: &str, text: &str) -> Document {
        let uri: Uri = format!("file:///src/{name}").parse().unwrap();
        Document::new(uri, text.to_string())
    }

    fn symbol(
        name: &str,
        kind: SymbolKind,
        file: &str,
        line: u32,
        cols: (u32, u32),
    ) -> WorkspaceSymbol {
        WorkspaceSymbol {
            name: name.to_string(),
            kind,
            tags: None,
            container_name: Some(file.to_string()),
            location: OneOf::Left(Location::new(
                format!("file:///src/{file}").parse().unwrap(),
                Range::new(Position::new(line, cols.0), Position::new(line, cols.1)),
            )),
            data: None,
        }
    }

    fn search(docs: &[Document], query: &str) -> Vec<WorkspaceSymbol> {
        workspace_symbols(
            docs.iter().map(|d| {
                let name = d
                    .uri()
                    .path()
                    .as_str()
                    .rsplit('/')
                    .next()
                    .unwrap()
                    .to_string();
                (d, name)
            }),
            query,
        )
    }

    #[test]
    fn test_match_rank() {
        assert_eq!(match_rank("CFLAGS", ""), Some(1));
        assert_eq!(match_rank("CFLAGS", "cflags"), Some(0));
        assert_eq!(match_rank("CFLAGS", "CF"), Some(1));
        assert_eq!(match_rank("CFLAGS", "flag"), Some(2));
        assert_eq!(match_rank("CFLAGS", "cfs"), Some(3));
        assert_eq!(match_rank("CFLAGS", "sfc"), None);
        assert_eq!(match_rank("CC", "ccc"), None);
    }

    #[test]
    fn test_workspace_symbols_all() {
        let docs = [
            doc("Makefile", "CC = gcc\nall clean: x\n\techo\n.PHONY: all\n"),
            doc("rules.mk", "define RECIPE\necho\nendef\ninstall:\n"),
        ];
        assert_eq!(
            search(&docs, ""),
            vec![
                symbol("all", SymbolKind::FUNCTION, "Makefile", 1, (0, 3)),
                symbol("clean", SymbolKind::FUNCTION, "Makefile", 1, (4, 9)),
                symbol("CC", SymbolKind::VARIABLE, "Makefile", 0, (0, 2)),
                symbol("install", SymbolKind::FUNCTION, "rules.mk", 3, (0, 7)),
                symbol("RECIPE", SymbolKind::VARIABLE, "rules.mk", 0, (7, 13)),
            ]
        );
    }

    #[test]
    fn test_workspace_symbols_ranked() {
        let docs = [
            doc("Makefile", "install-docs:\ninstall:\nINSTALL_DIR = /usr\n"),
            doc("rules.mk", "reinstall:\nint:\n"),
        ];
        assert_eq!(
            search(&docs, "install"),
            vec![
                symbol("install", SymbolKind::FUNCTION, "Makefile", 1, (0, 7)),
                symbol("install-docs", SymbolKind::FUNCTION, "Makefile", 0, (0, 12)),
                symbol("INSTALL_DIR", SymbolKind::VARIABLE, "Makefile", 2, (0, 11)),
                symbol("reinstall", SymbolKind::FUNCTION, "rules.mk", 0, (0, 9)),
            ]
        );
        assert_eq!(
            search(&docs, "inl"),
            vec![
                symbol("install-docs", SymbolKind::FUNCTION, "Makefile", 0, (0, 12)),
                symbol("install", SymbolKind::FUNCTION, "Makefile", 1, (0, 7)),
                symbol("INSTALL_DIR", SymbolKind::VARIABLE, "Makefile", 2, (0, 11)),
                symbol("reinstall", SymbolKind::FUNCTION, "rules.mk", 0, (0, 9)),
            ]
        );
    }

    #[test]
    fn test_workspace_symbols_limit() {
        let text: String = (0..MAX_WORKSPACE_SYMBOLS + 10)
            .map(|i| format!("t{i}:\n"))
            .collect();
        let docs = [doc("Makefile", &text)];
        assert_eq!(search(&docs, "t").len(), MAX_WORKSPACE_SYMBOLS);
    }

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
    fn test_symbols_rule_selection_range_covers_targets() {
        let text = "a b: c\n\techo\n";
        let makefile = Makefile::parse(text).tree();
        let symbols = generate_document_symbols(&makefile, text);
        let selections: Vec<(&str, Range)> = symbols
            .iter()
            .map(|s| (s.name.as_str(), s.selection_range))
            .collect();
        assert_eq!(
            selections,
            vec![("a, b", Range::new(Position::new(0, 0), Position::new(0, 3)))]
        );
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
    fn test_symbols_variable_selection_range_is_name() {
        let text = "export CC = gcc\n";
        let makefile = Makefile::parse(text).tree();
        let symbols = generate_document_symbols(&makefile, text);
        let selections: Vec<(&str, Range)> = symbols
            .iter()
            .map(|s| (s.name.as_str(), s.selection_range))
            .collect();
        assert_eq!(
            selections,
            vec![("CC", Range::new(Position::new(0, 7), Position::new(0, 9)))]
        );
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
