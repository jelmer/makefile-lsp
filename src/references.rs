//! Find references for Makefiles.

use makefile_lossless::{
    is_in_prerequisites, word_at_offset, Lang, Makefile, Recipe, SyntaxKind, TextRange,
    VariableDefinition, VariableReference,
};
use rowan::ast::AstNode;
use rowan::WalkEvent;
use text_size::TextSize;
use tower_lsp_server::ls_types::{Location, Position, Range, Uri};

use crate::position::{text_range_to_lsp_range, try_position_to_offset};
use crate::targets::{target_at_offset, targets_with_ranges};
use crate::workspace::FileSet;

/// A target or variable name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Symbol {
    Variable(String),
    Target(String),
}

/// Identify the symbol at `byte_offset`: a variable reference, a prerequisite,
/// or the name in a target or variable definition.
pub fn symbol_at(makefile: &Makefile, source_text: &str, byte_offset: usize) -> Option<Symbol> {
    let offset = TextSize::from(byte_offset as u32);
    // The innermost reference, for nested ones such as `$(FOO.$(BAR))`.
    let reference = variable_references(makefile)
        .into_iter()
        .filter(|(_, range)| range.start() <= offset && offset <= range.end())
        .min_by_key(|(_, range)| range.len());
    if let Some((name, _)) = reference {
        return Some(Symbol::Variable(name));
    }

    let word = word_at_offset(source_text, byte_offset)?;
    if is_in_prerequisites(source_text, byte_offset) {
        return Some(Symbol::Target(word.to_string()));
    }

    if let Some((target, _)) = makefile
        .rules()
        .find_map(|r| target_at_offset(&r, byte_offset))
    {
        return Some(Symbol::Target(target));
    }

    makefile.variable_definitions().find_map(|v| {
        let range = v.name_range()?;
        if byte_offset < usize::from(range.start()) || byte_offset >= usize::from(range.end()) {
            return None;
        }
        v.name().map(Symbol::Variable)
    })
}

/// Find all occurrences of `symbol` in one document.
pub fn symbol_locations(
    makefile: &Makefile,
    source_text: &str,
    uri: &Uri,
    symbol: &Symbol,
    include_declaration: bool,
) -> Vec<Location> {
    match symbol {
        Symbol::Variable(name) => {
            find_variable_references(makefile, source_text, name, uri, include_declaration)
        }
        Symbol::Target(name) => {
            find_target_references(makefile, source_text, name, uri, include_declaration)
        }
    }
}

/// Find all references to the symbol at the given position, in all
/// documents of the file set.
pub fn find_references(
    files: &FileSet,
    position: Position,
    include_declaration: bool,
) -> Vec<Location> {
    let current = files.current();
    let Some(offset) = try_position_to_offset(current.text(), position) else {
        return vec![];
    };
    let Some(symbol) = symbol_at(&current.makefile(), current.text(), offset.into()) else {
        return vec![];
    };
    files
        .docs()
        .flat_map(|doc| {
            symbol_locations(
                &doc.makefile(),
                doc.text(),
                doc.uri(),
                &symbol,
                include_declaration,
            )
        })
        .collect()
}

/// Find all references to the symbol at the given position within one
/// document.
pub fn find_document_references(
    makefile: &Makefile,
    source_text: &str,
    position: Position,
    uri: &Uri,
    include_declaration: bool,
) -> Vec<Location> {
    let Some(offset) = try_position_to_offset(source_text, position) else {
        return vec![];
    };
    let Some(symbol) = symbol_at(makefile, source_text, offset.into()) else {
        return vec![];
    };
    symbol_locations(makefile, source_text, uri, &symbol, include_declaration)
}

/// Find all references to a target name.
fn find_target_references(
    makefile: &Makefile,
    source_text: &str,
    target_name: &str,
    uri: &Uri,
    include_declaration: bool,
) -> Vec<Location> {
    let mut locations = Vec::new();

    for rule in makefile.rules() {
        // Target definitions
        if include_declaration {
            for (target, range) in targets_with_ranges(&rule) {
                if target == target_name {
                    locations.push(Location {
                        uri: uri.clone(),
                        range: text_range_to_lsp_range(source_text, range),
                    });
                }
            }
        }

        // Prerequisite references
        for prereq in rule.prerequisites() {
            if prereq == target_name {
                // Find the prerequisite in the source text by scanning
                find_word_in_prerequisites(source_text, &rule, target_name, uri, &mut locations);
            }
        }
    }

    // Also find references in .PHONY and similar
    for rule in makefile.rules() {
        let targets: Vec<String> = rule.targets().collect();
        if targets.iter().any(|t| t.starts_with('.')) && targets.iter().all(|t| t != target_name) {
            for prereq in rule.prerequisites() {
                if prereq == target_name {
                    find_word_in_prerequisites(
                        source_text,
                        &rule,
                        target_name,
                        uri,
                        &mut locations,
                    );
                }
            }
        }
    }

    locations.sort_by_key(|l| (l.range.start.line, l.range.start.character));
    locations.dedup_by(|a, b| a.range == b.range);
    locations
}

/// Find occurrences of a word in the prerequisites area of a rule.
fn find_word_in_prerequisites(
    source_text: &str,
    rule: &makefile_lossless::Rule,
    word: &str,
    uri: &Uri,
    locations: &mut Vec<Location>,
) {
    let rule_range = rule.syntax().text_range();
    let rule_text = &source_text[usize::from(rule_range.start())..usize::from(rule_range.end())];
    let rule_offset: usize = rule_range.start().into();

    // Find the colon in the rule line
    if let Some(colon_pos) = rule_text.find(':') {
        let after_colon = &rule_text[colon_pos + 1..];
        // Find newline (end of prerequisites line)
        let end = after_colon.find('\n').unwrap_or(after_colon.len());
        let prereq_text = &after_colon[..end];
        let prereq_start = rule_offset + colon_pos + 1;

        for (idx, _) in prereq_text.match_indices(word) {
            let abs_offset = prereq_start + idx;
            // Verify it's a whole word match
            let before_ok = idx == 0
                || !prereq_text.as_bytes()[idx - 1].is_ascii_alphanumeric()
                    && prereq_text.as_bytes()[idx - 1] != b'_';
            let after_idx = idx + word.len();
            let after_ok = after_idx >= prereq_text.len()
                || !prereq_text.as_bytes()[after_idx].is_ascii_alphanumeric()
                    && prereq_text.as_bytes()[after_idx] != b'_';
            if before_ok && after_ok {
                let start = crate::position::offset_to_position(
                    source_text,
                    text_size::TextSize::from(abs_offset as u32),
                );
                let end = Position::new(start.line, start.character + word.len() as u32);
                locations.push(Location {
                    uri: uri.clone(),
                    range: Range::new(start, end),
                });
            }
        }
    }
}

/// Find all references to a variable name (in $(VAR) or ${VAR} patterns).
fn find_variable_references(
    makefile: &Makefile,
    source_text: &str,
    var_name: &str,
    uri: &Uri,
    include_declaration: bool,
) -> Vec<Location> {
    let mut locations = Vec::new();

    // Find the declaration
    if include_declaration {
        for var_def in makefile.variable_definitions() {
            if var_def.name().as_deref() != Some(var_name) {
                continue;
            }
            if let Some(range) = var_def.name_range() {
                locations.push(Location {
                    uri: uri.clone(),
                    range: text_range_to_lsp_range(source_text, range),
                });
            }
        }
    }

    for (name, range) in variable_references(makefile) {
        if name == var_name {
            locations.push(Location {
                uri: uri.clone(),
                range: text_range_to_lsp_range(source_text, range),
            });
        }
    }

    locations.sort_by_key(|l| (l.range.start.line, l.range.start.character));
    locations
}

/// All `$(VAR)` and `${VAR}` references in the document, and `$V` outside
/// recipes and define bodies, with the range of the variable name. Function
/// calls such as `$(shell ...)` are left out, but references in their
/// arguments are included.
fn variable_references(makefile: &Makefile) -> Vec<(String, TextRange)> {
    let mut refs = Vec::new();
    let mut preorder = makefile.syntax().preorder();
    while let Some(event) = preorder.next() {
        let WalkEvent::Enter(node) = event else {
            continue;
        };
        if is_define_body(&node) {
            // Define bodies are not parsed into nested references.
            scan_variable_references(
                &node.text().to_string(),
                node.text_range().start(),
                &mut refs,
            );
            preorder.skip_subtree();
        } else if let Some(recipe) = Recipe::cast(node.clone()) {
            refs.extend(
                recipe
                    .variable_references()
                    .into_iter()
                    .map(|r| (r.name().to_string(), r.text_range())),
            );
        } else if let Some(reference) = VariableReference::cast(node) {
            if !reference.is_function_call() {
                refs.extend(reference_name(&reference));
            }
        }
    }
    refs
}

fn is_define_body(node: &rowan::SyntaxNode<Lang>) -> bool {
    node.kind() == SyntaxKind::EXPR
        && node
            .parent()
            .and_then(VariableDefinition::cast)
            .is_some_and(|v| v.is_define())
}

/// The name of `reference` and its range.
fn reference_name(reference: &VariableReference) -> Option<(String, TextRange)> {
    let name = reference.name()?;
    let mut children = reference.syntax().children_with_tokens().skip(1);
    let open = children.next()?;
    if !matches!(open.kind(), SyntaxKind::LPAREN | SyntaxKind::LBRACE) {
        return Some((name, open.text_range()));
    }
    // As in VariableReference::name, which also takes nested references
    // into the name.
    let range = children
        .take_while(|c| {
            !matches!(
                c.kind(),
                SyntaxKind::RPAREN
                    | SyntaxKind::RBRACE
                    | SyntaxKind::WHITESPACE
                    | SyntaxKind::COMMA
                    | SyntaxKind::OPERATOR
                    | SyntaxKind::NEWLINE
            )
        })
        .map(|c| c.text_range())
        .reduce(|a, b| a.cover(b))?;
    Some((name, range))
}

/// Find `$(VAR)` and `${VAR}` references in `text`, which starts at `base`
/// in the document. References whose name contains another reference, and
/// function calls, are left out.
fn scan_variable_references(text: &str, base: TextSize, out: &mut Vec<(String, TextRange)>) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] != b'$' {
            i += 1;
            continue;
        }
        let close = match bytes[i + 1] {
            b'(' => b')',
            b'{' => b'}',
            // `$$` is an escaped dollar sign; skip it along with `$V`.
            _ => {
                i += 2;
                continue;
            }
        };
        let start = i + 2;
        let end = bytes[start..]
            .iter()
            .position(|&b| b == close || b":\t ,\n$".contains(&b))
            .map(|n| start + n);
        if let Some(end) = end.filter(|&e| e > start && (bytes[e] == close || bytes[e] == b':')) {
            let range = TextRange::new(
                base + TextSize::from(start as u32),
                base + TextSize::from(end as u32),
            );
            out.push((text[start..end].to_string(), range));
        }
        // Continue inside the reference to find nested ones.
        i = start;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::tests::Fixture;

    fn test_uri() -> Uri {
        "file:///test/Makefile".parse().unwrap()
    }

    #[test]
    fn test_find_target_references_from_prereq() {
        let text = "all: build\n\nbuild:\n\techo ok\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        // Cursor on "build" in prerequisites (col 5)
        let refs =
            find_document_references(&makefile, text, Position::new(0, 5), &test_uri(), true);
        assert_eq!(refs.len(), 2); // declaration + prerequisite reference
    }

    #[test]
    fn test_find_target_references_from_definition() {
        let text = "all: build\n\nbuild:\n\techo ok\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        // Cursor on "build" in its definition (line 2, col 0)
        let refs =
            find_document_references(&makefile, text, Position::new(2, 0), &test_uri(), true);
        assert_eq!(refs.len(), 2);
    }

    #[test]
    fn test_find_target_references_second_target_of_rule() {
        let text = "all: b\na b: c\n";
        let makefile = Makefile::parse(text).tree();
        let ranges: Vec<Range> =
            find_document_references(&makefile, text, Position::new(0, 5), &test_uri(), true)
                .into_iter()
                .map(|l| l.range)
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
    fn test_find_target_references_no_declaration() {
        let text = "all: build\n\nbuild:\n\techo ok\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let refs =
            find_document_references(&makefile, text, Position::new(0, 5), &test_uri(), false);
        assert_eq!(refs.len(), 1); // only the prerequisite reference
    }

    #[test]
    fn test_find_variable_references() {
        let text = "CC = gcc\nall:\n\t$(CC) main.c\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        // Cursor on CC in $(CC) (line 2, col 3)
        let refs =
            find_document_references(&makefile, text, Position::new(2, 3), &test_uri(), true);
        assert_eq!(refs.len(), 2); // definition + usage
    }

    #[test]
    fn test_find_variable_references_from_definition() {
        let text = "CC = gcc\nall:\n\t$(CC) main.c\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        // Cursor on CC in definition (line 0, col 0)
        let refs =
            find_document_references(&makefile, text, Position::new(0, 0), &test_uri(), true);
        assert_eq!(refs.len(), 2);
    }

    #[test]
    fn test_find_variable_multiple_usages() {
        let text = "CC = gcc\nall:\n\t$(CC) main.c\nclean:\n\t$(CC) --version\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let refs =
            find_document_references(&makefile, text, Position::new(0, 0), &test_uri(), true);
        assert_eq!(refs.len(), 3); // definition + 2 usages
    }

    #[test]
    fn test_find_references_nothing() {
        let text = "all:\n\techo hello\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let refs =
            find_document_references(&makefile, text, Position::new(1, 2), &test_uri(), true);
        assert!(refs.is_empty());
    }

    fn range(line: u32, start: u32, end: u32) -> Range {
        Range::new(Position::new(line, start), Position::new(line, end))
    }

    /// References to `FOO`, found from its definition on the first line.
    fn foo_refs(text: &str) -> Vec<Range> {
        assert!(text.starts_with("FOO = 1\n"));
        let makefile = Makefile::parse(text).tree();
        find_document_references(&makefile, text, Position::new(0, 0), &test_uri(), true)
            .into_iter()
            .map(|l| l.range)
            .collect()
    }

    #[test]
    fn test_symbol_at_substitution_reference() {
        let text = "FOO = a.c\nX = $(FOO:.c=.o) ${FOO:%.c=%.o}\n";
        let makefile = Makefile::parse(text).tree();
        let foo = Some(Symbol::Variable("FOO".to_string()));
        assert_eq!(symbol_at(&makefile, text, 16), foo);
        assert_eq!(symbol_at(&makefile, text, 19), foo);
        assert_eq!(symbol_at(&makefile, text, 29), foo);
    }

    #[test]
    fn test_symbol_at_nested_reference() {
        let text = "X = $(subst a,b,$(FOO)) $(BAR.$(Y))\n";
        let makefile = Makefile::parse(text).tree();
        assert_eq!(
            symbol_at(&makefile, text, 19),
            Some(Symbol::Variable("FOO".to_string()))
        );
        assert_eq!(
            symbol_at(&makefile, text, 32),
            Some(Symbol::Variable("Y".to_string()))
        );
    }

    #[test]
    fn test_find_variable_references_in_values() {
        let text = "FOO = 1\nX = $(FOO:.c=.o) ${FOO} $(subst a,b,$(FOO)) $(FOO)$(FOO)\n";
        assert_eq!(
            foo_refs(text),
            vec![
                range(0, 0, 3),
                range(1, 6, 9),
                range(1, 19, 22),
                range(1, 38, 41),
                range(1, 46, 49),
                range(1, 52, 55),
            ]
        );
    }

    #[test]
    fn test_find_variable_references_skips_other_names() {
        let text = "FOO = 1\nX = $(FOOBAR) $(FOO.$(Y)) $$(FOO) $(FOO x)\n";
        assert_eq!(foo_refs(text), vec![range(0, 0, 3)]);
    }

    #[test]
    fn test_find_variable_references_in_conditional_headers() {
        let text = "FOO = 1\nifeq ($(FOO:a=b),x)\nendif\nifneq \"$(FOO)\" \"\"\nendif\n";
        assert_eq!(
            foo_refs(text),
            vec![range(0, 0, 3), range(1, 8, 11), range(3, 9, 12)]
        );
    }

    #[test]
    fn test_find_variable_references_in_define_bodies() {
        let text = "FOO = 1\ndefine A\n$(FOO) $(FOO:a=b)\nendef\n\
                    define B\n\techo $(FOO) $$(FOO)\nall: ${FOO}\nendef\n";
        assert_eq!(
            foo_refs(text),
            vec![
                range(0, 0, 3),
                range(2, 2, 5),
                range(2, 9, 12),
                range(5, 8, 11),
                range(6, 7, 10),
            ]
        );
    }

    #[test]
    fn test_find_variable_references_in_recipes() {
        let text = "FOO = 1\nall: ; echo $(FOO)\n\
                    \techo $(FOO:.c=.o) $$(FOO) $(shell ${FOO})\n\
                    ifdef X\n\techo $(FOO)\nendif\n";
        assert_eq!(
            foo_refs(text),
            vec![
                range(0, 0, 3),
                range(1, 14, 17),
                range(2, 8, 11),
                range(2, 37, 40),
                range(4, 8, 11),
            ]
        );
    }

    #[test]
    fn test_find_variable_references_in_rule_heads() {
        let text = "FOO = 1\n$(FOO): $(FOO:.c=.o)\nall: Z = $(FOO)\n";
        assert_eq!(
            foo_refs(text),
            vec![
                range(0, 0, 3),
                range(1, 2, 5),
                range(1, 10, 13),
                range(2, 11, 14),
            ]
        );
    }

    fn locations(locs: &[Location]) -> Vec<(Uri, u32, u32)> {
        locs.iter()
            .map(|l| (l.uri.clone(), l.range.start.line, l.range.start.character))
            .collect()
    }

    #[test]
    fn test_find_variable_references_across_files() {
        let fx = Fixture::new(&[
            ("Makefile", "include rules.mk\nall:\n\t$(CC) x\n"),
            ("rules.mk", "CC = gcc\nX = $(CC)\n"),
        ]);
        let refs = find_references(&fx.file_set("Makefile"), Position::new(2, 3), true);
        assert_eq!(
            locations(&refs),
            vec![
                (fx.uri("Makefile"), 2, 3),
                (fx.uri("rules.mk"), 0, 0),
                (fx.uri("rules.mk"), 1, 6),
            ]
        );
    }

    #[test]
    fn test_find_target_references_from_included_file() {
        let fx = Fixture::new(&[
            ("Makefile", "include rules.mk\nall: build\n"),
            ("rules.mk", "build:\n\techo\n"),
        ]);
        let (mut ws, makefile) = fx.open("Makefile");
        ws.file_set(&makefile).unwrap();
        let rules = fx.open_in(&mut ws, "rules.mk");
        let refs = find_references(&ws.file_set(&rules).unwrap(), Position::new(0, 1), false);
        assert_eq!(locations(&refs), vec![(makefile, 1, 5)]);
    }
}
