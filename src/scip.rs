//! SCIP index generation for Makefiles.
//!
//! Produces a [SCIP](https://github.com/sourcegraph/scip) index covering rule
//! targets (definitions and prerequisite references) and variables (definitions
//! and `$(VAR)`/`${VAR}` references). Symbol positions are emitted as UTF-8 byte
//! offsets from the start of the line, matching `PositionEncoding::UTF8`.

use std::collections::BTreeMap;

use makefile_lossless::Makefile;
use rowan::ast::AstNode;
use scip::types::{
    descriptor, symbol_information, Descriptor, Document, Index, Metadata, Occurrence, Package,
    PositionEncoding, ProtocolVersion, Symbol, SymbolInformation, SymbolRole, TextEncoding,
    ToolInfo,
};

const SCHEME: &str = "scip-makefile";

/// Build a SCIP index for a set of Makefiles.
///
/// `project_root` is a URI-encoded absolute path; each `(relative_path, text)`
/// pair is a source file relative to that root.
pub fn build_index(project_root: &str, files: &[(String, String)]) -> Index {
    let documents = files
        .iter()
        .map(|(path, text)| build_document(path, text))
        .collect();

    Index {
        metadata: Some(Metadata {
            version: ProtocolVersion::UnspecifiedProtocolVersion.into(),
            tool_info: Some(ToolInfo {
                name: "makefile-lsp".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                arguments: Vec::new(),
                ..Default::default()
            })
            .into(),
            project_root: project_root.to_string(),
            text_document_encoding: TextEncoding::UTF8.into(),
            ..Default::default()
        })
        .into(),
        documents,
        external_symbols: Vec::new(),
        ..Default::default()
    }
}

/// A single occurrence of a symbol, before conversion to SCIP coordinates.
struct RawOccurrence {
    symbol: String,
    /// Byte offset of the start of the name.
    start: usize,
    /// Byte length of the name.
    len: usize,
    is_definition: bool,
}

fn build_document(relative_path: &str, text: &str) -> Document {
    let parsed = Makefile::parse(text);
    let makefile = parsed.tree();

    let mut occurrences = Vec::new();
    // Track which symbols have definitions, with display name and kind, so the
    // document's symbol table only lists symbols defined here.
    let mut defined: BTreeMap<String, (String, symbol_information::Kind)> = BTreeMap::new();

    for raw in collect_targets(&makefile, text)
        .into_iter()
        .chain(collect_variables(&makefile, text))
    {
        let mut roles = 0;
        if raw.is_definition {
            roles |= SymbolRole::Definition as i32;
        }
        occurrences.push(Occurrence {
            range: byte_range_to_scip(text, raw.start, raw.len),
            symbol: raw.symbol,
            symbol_roles: roles,
            ..Default::default()
        });
    }

    // A prerequisite or variable that appears more than once in the source can
    // be reported once per AST entry; collapse to one occurrence per location.
    occurrences.sort_by(|a, b| a.range.cmp(&b.range).then(a.symbol.cmp(&b.symbol)));
    occurrences.dedup_by(|a, b| a.range == b.range && a.symbol == b.symbol);

    // Record defined symbols separately so we can attach SymbolInformation.
    for (symbol, name, kind) in collect_definitions(&makefile, text) {
        defined.entry(symbol).or_insert((name, kind));
    }

    let symbols = defined
        .into_iter()
        .map(|(symbol, (name, kind))| SymbolInformation {
            symbol,
            display_name: name,
            kind: kind.into(),
            ..Default::default()
        })
        .collect();

    Document {
        language: "makefile".to_string(),
        relative_path: relative_path.to_string(),
        occurrences,
        symbols,
        position_encoding: PositionEncoding::UTF8CodeUnitOffsetFromLineStart.into(),
        ..Default::default()
    }
}

/// Build the SCIP symbol string for a target.
fn target_symbol(name: &str) -> String {
    scip::symbol::format_symbol(make_symbol(name, descriptor::Suffix::Namespace))
}

/// Build the SCIP symbol string for a variable.
fn variable_symbol(name: &str) -> String {
    scip::symbol::format_symbol(make_symbol(name, descriptor::Suffix::Term))
}

fn make_symbol(name: &str, suffix: descriptor::Suffix) -> Symbol {
    Symbol {
        scheme: SCHEME.to_string(),
        package: Some(Package::default()).into(),
        descriptors: vec![Descriptor {
            name: name.to_string(),
            suffix: suffix.into(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// Collect target definitions and prerequisite references.
fn collect_targets(makefile: &Makefile, text: &str) -> Vec<RawOccurrence> {
    let mut out = Vec::new();

    for rule in makefile.rules() {
        let rule_range = rule.syntax().text_range();
        let rule_start: usize = rule_range.start().into();
        let rule_text = &text[rule_start..usize::from(rule_range.end())];

        let Some(colon_pos) = rule_text.find(':') else {
            continue;
        };

        // Targets appear before the colon.
        let target_section = &rule_text[..colon_pos];
        for target in rule.targets() {
            if let Some(idx) = find_word(target_section, &target) {
                out.push(RawOccurrence {
                    symbol: target_symbol(&target),
                    start: rule_start + idx,
                    len: target.len(),
                    is_definition: true,
                });
            }
        }

        // Prerequisites appear after the colon, on the same line.
        let after_colon = &rule_text[colon_pos + 1..];
        let prereq_end = after_colon.find('\n').unwrap_or(after_colon.len());
        let prereq_section = &after_colon[..prereq_end];
        let prereq_offset = rule_start + colon_pos + 1;
        for prereq in rule.prerequisites() {
            for idx in find_words(prereq_section, &prereq) {
                out.push(RawOccurrence {
                    symbol: target_symbol(&prereq),
                    start: prereq_offset + idx,
                    len: prereq.len(),
                    is_definition: false,
                });
            }
        }
    }

    out
}

/// Collect variable definitions and `$(VAR)`/`${VAR}` references.
fn collect_variables(makefile: &Makefile, text: &str) -> Vec<RawOccurrence> {
    let mut out = Vec::new();

    for var in makefile.variable_definitions() {
        let Some(name) = var.name() else {
            continue;
        };
        let var_range = var.syntax().text_range();
        let var_start: usize = var_range.start().into();
        let var_text = &text[var_start..usize::from(var_range.end())];
        if let Some(idx) = find_word(var_text, &name) {
            out.push(RawOccurrence {
                symbol: variable_symbol(&name),
                start: var_start + idx,
                len: name.len(),
                is_definition: true,
            });
        }
    }

    // References: scan the whole text for $(VAR) and ${VAR}.
    let names: Vec<String> = makefile
        .variable_definitions()
        .filter_map(|v| v.name())
        .collect();
    for name in &names {
        for (start, len) in find_variable_uses(text, name) {
            out.push(RawOccurrence {
                symbol: variable_symbol(name),
                start,
                len,
                is_definition: false,
            });
        }
    }

    out
}

/// Collect the defined symbols (for the document symbol table) with display
/// names and kinds.
fn collect_definitions(
    makefile: &Makefile,
    _text: &str,
) -> Vec<(String, String, symbol_information::Kind)> {
    let mut out = Vec::new();
    for rule in makefile.rules() {
        for target in rule.targets() {
            out.push((
                target_symbol(&target),
                target,
                symbol_information::Kind::Function,
            ));
        }
    }
    for var in makefile.variable_definitions() {
        if let Some(name) = var.name() {
            out.push((
                variable_symbol(&name),
                name,
                symbol_information::Kind::Variable,
            ));
        }
    }
    out
}

/// Find the byte offset of the first whole-word occurrence of `word` in `haystack`.
fn find_word(haystack: &str, word: &str) -> Option<usize> {
    find_words(haystack, word).into_iter().next()
}

/// Find byte offsets of all whole-word occurrences of `word` in `haystack`.
fn find_words(haystack: &str, word: &str) -> Vec<usize> {
    if word.is_empty() {
        return Vec::new();
    }
    let bytes = haystack.as_bytes();
    haystack
        .match_indices(word)
        .filter(|(idx, _)| {
            let before_ok = *idx == 0 || !is_word_byte(bytes[*idx - 1]);
            let after = *idx + word.len();
            let after_ok = after >= bytes.len() || !is_word_byte(bytes[after]);
            before_ok && after_ok
        })
        .map(|(idx, _)| idx)
        .collect()
}

/// Find `$(VAR)` and `${VAR}` uses of `name`, returning (name_start, name_len).
fn find_variable_uses(text: &str, name: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let paren = format!("$({}", name);
    let brace = format!("${{{}", name);
    for (pattern, close) in [(&paren, b')'), (&brace, b'}')] {
        for (idx, _) in text.match_indices(pattern.as_str()) {
            let after = idx + pattern.len();
            let next = text.as_bytes().get(after).copied();
            // Only count it if the name is followed by the closing delimiter,
            // whitespace, or a function-argument separator.
            let ok = matches!(next, Some(b) if b == close || b == b' ' || b == b')' || b == b'}' || b == b',');
            if ok {
                out.push((idx + 2, name.len()));
            }
        }
    }
    out
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Convert a byte range to a SCIP occurrence range
/// `[startLine, startChar, endLine, endChar]`, with characters as UTF-8 byte
/// offsets from the start of the line. The range stays on a single line since
/// all Makefile symbol names do.
fn byte_range_to_scip(text: &str, start: usize, len: usize) -> Vec<i32> {
    let (line, col) = byte_offset_to_line_col(text, start);
    vec![line, col, line, col + len as i32]
}

/// Convert a byte offset to (line, byte-column), both 0-based.
fn byte_offset_to_line_col(text: &str, offset: usize) -> (i32, i32) {
    let mut line = 0i32;
    let mut line_start = 0usize;
    for (i, b) in text.bytes().enumerate() {
        if i >= offset {
            break;
        }
        if b == b'\n' {
            line += 1;
            line_start = i + 1;
        }
    }
    (line, (offset - line_start) as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn occ_symbols(doc: &Document) -> Vec<(&str, &Vec<i32>, bool)> {
        doc.occurrences
            .iter()
            .map(|o| {
                (
                    o.symbol.as_str(),
                    &o.range,
                    o.symbol_roles & SymbolRole::Definition as i32 != 0,
                )
            })
            .collect()
    }

    #[test]
    fn test_target_definition_and_reference() {
        let text = "all: build\n\nbuild:\n\techo ok\n";
        let doc = build_document("Makefile", text);

        let all = target_symbol("all");
        let build = target_symbol("build");

        // all definition at line 0 col 0
        assert!(doc.occurrences.iter().any(|o| o.symbol == all
            && o.range == vec![0, 0, 0, 3]
            && o.symbol_roles & SymbolRole::Definition as i32 != 0));

        // build referenced as a prerequisite on line 0 (cols 5..10)
        assert!(doc.occurrences.iter().any(|o| o.symbol == build
            && o.range == vec![0, 5, 0, 10]
            && o.symbol_roles & SymbolRole::Definition as i32 == 0));

        // build defined on line 2 cols 0..5
        assert!(doc.occurrences.iter().any(|o| o.symbol == build
            && o.range == vec![2, 0, 2, 5]
            && o.symbol_roles & SymbolRole::Definition as i32 != 0));
    }

    #[test]
    fn test_variable_definition_and_reference() {
        let text = "CC = gcc\nall:\n\t$(CC) main.c\n";
        let doc = build_document("Makefile", text);

        let cc = variable_symbol("CC");

        // definition at line 0 col 0
        assert!(doc.occurrences.iter().any(|o| o.symbol == cc
            && o.range == vec![0, 0, 0, 2]
            && o.symbol_roles & SymbolRole::Definition as i32 != 0));

        // use $(CC) on line 2, name starts after the "$(" (byte col 3)
        assert!(doc.occurrences.iter().any(|o| o.symbol == cc
            && o.range == vec![2, 3, 2, 5]
            && o.symbol_roles & SymbolRole::Definition as i32 == 0));
    }

    #[test]
    fn test_symbol_table_lists_definitions() {
        let text = "CC = gcc\n\nall: build\n\techo ok\n";
        let doc = build_document("Makefile", text);

        let names: Vec<&str> = doc
            .symbols
            .iter()
            .map(|s| s.display_name.as_str())
            .collect();
        assert!(names.contains(&"CC"));
        assert!(names.contains(&"all"));

        let cc = doc.symbols.iter().find(|s| s.display_name == "CC").unwrap();
        assert_eq!(cc.kind.enum_value(), Ok(symbol_information::Kind::Variable));

        let all = doc
            .symbols
            .iter()
            .find(|s| s.display_name == "all")
            .unwrap();
        assert_eq!(
            all.kind.enum_value(),
            Ok(symbol_information::Kind::Function)
        );
    }

    #[test]
    fn test_brace_variable_reference() {
        let text = "CC = gcc\nall:\n\t${CC} main.c\n";
        let doc = build_document("Makefile", text);
        let cc = variable_symbol("CC");
        assert!(doc.occurrences.iter().any(|o| o.symbol == cc
            && o.range == vec![2, 3, 2, 5]
            && o.symbol_roles & SymbolRole::Definition as i32 == 0));
    }

    #[test]
    fn test_multiple_variable_uses() {
        let text = "CC = gcc\nall:\n\t$(CC) a.c\nclean:\n\t$(CC) --version\n";
        let doc = build_document("Makefile", text);
        let cc = variable_symbol("CC");
        let uses = occ_symbols(&doc)
            .into_iter()
            .filter(|(s, _, def)| *s == cc && !*def)
            .count();
        assert_eq!(uses, 2);
    }

    #[test]
    fn test_index_metadata() {
        let files = vec![("Makefile".to_string(), "all:\n\techo hi\n".to_string())];
        let index = build_index("file:///project", &files);
        let metadata = index.metadata.as_ref().unwrap();
        assert_eq!(metadata.project_root, "file:///project");
        assert_eq!(
            metadata.text_document_encoding.enum_value(),
            Ok(TextEncoding::UTF8)
        );
        assert_eq!(metadata.tool_info.name, "makefile-lsp");
        assert_eq!(index.documents.len(), 1);
    }

    #[test]
    fn test_target_symbol_format() {
        // Sanity-check the symbol string grammar: scheme, empty package, descriptor.
        assert_eq!(target_symbol("all"), "scip-makefile . . . all/");
        assert_eq!(variable_symbol("CC"), "scip-makefile . . . CC.");
    }

    #[test]
    fn test_no_symbols_empty_makefile() {
        let doc = build_document("Makefile", "");
        assert!(doc.occurrences.is_empty());
        assert!(doc.symbols.is_empty());
    }

    #[test]
    fn test_index_protobuf_roundtrip() {
        use protobuf::Message;

        let files = vec![(
            "Makefile".to_string(),
            "CC = gcc\nall: build\n\t$(CC) x.c\nbuild:\n\techo hi\n".to_string(),
        )];
        let index = build_index("file:///project", &files);

        let bytes = index.write_to_bytes().unwrap();
        let parsed = Index::parse_from_bytes(&bytes).unwrap();

        assert_eq!(parsed.metadata.project_root, "file:///project");
        assert_eq!(parsed.documents.len(), 1);
        assert_eq!(parsed.documents[0].relative_path, "Makefile");
        assert_eq!(
            parsed.documents[0].occurrences,
            index.documents[0].occurrences
        );
        assert_eq!(parsed.documents[0].symbols, index.documents[0].symbols);
    }
}
