//! SCIP index generation for Makefiles.
//!
//! Produces a [SCIP](https://github.com/sourcegraph/scip) index covering rule
//! targets (definitions and prerequisite references) and variables (definitions
//! and `$(VAR)`/`${VAR}` references). References to built-in and automatic
//! variables (`$@`, `$<`, `$(MAKE)`, ...) carry the same documentation the LSP
//! hover serves, so SCIP consumers can show it. Each occurrence carries a
//! `SyntaxKind` so consumers can syntax-highlight from the index. Lint and parse
//! diagnostics are carried into the index as symbol-less occurrences. Symbols
//! are global, so a reference to a variable defined in an included makefile
//! links to its definition there. Symbol positions are emitted as UTF-8 byte
//! offsets from the start of the line, matching `PositionEncoding::UTF8`.

use std::collections::{BTreeMap, HashSet};

use makefile_lossless::Makefile;
use rowan::ast::AstNode;
use scip::types::{
    descriptor, symbol_information, Descriptor, Diagnostic, Document, Index, Metadata, Occurrence,
    Package, PositionEncoding, ProtocolVersion, Severity, Symbol, SymbolInformation, SymbolRole,
    SyntaxKind, TextEncoding, ToolInfo,
};
use tower_lsp_server::ls_types::{DiagnosticSeverity, NumberOrString};

use crate::position::try_lsp_range_to_text_range;
use crate::targets::targets_with_ranges;
use crate::workspace::FileSet;

const SCHEME: &str = "scip-makefile";

/// A source file to index, identified by its path relative to the project root.
pub struct SourceFile {
    /// Path relative to the project root, used as the document's identifier.
    pub relative_path: String,
    /// The file as the current document, together with the makefiles it
    /// includes and those including it.
    pub files: FileSet,
}

/// Build a SCIP index for a set of Makefiles.
///
/// `project_root` is a URI-encoded absolute path; each [`SourceFile`] is a
/// source file relative to that root.
pub fn build_index(project_root: &str, files: &[SourceFile]) -> Index {
    let documents = files
        .iter()
        .map(|f| build_document(&f.relative_path, &f.files))
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
    /// Syntax-highlighting classification for the name.
    syntax_kind: SyntaxKind,
}

fn build_document(relative_path: &str, files: &FileSet) -> Document {
    let current = files.current();
    let text = current.text();
    let makefile = current.makefile();

    let mut occurrences = Vec::new();
    // Track which symbols have definitions, with display name and kind, so the
    // document's symbol table only lists symbols defined here.
    let mut defined: BTreeMap<String, (String, symbol_information::Kind)> = BTreeMap::new();
    // Built-in and automatic variables referenced here but defined by Make
    // itself. We carry documentation for them even without a definition in this
    // file, keyed by symbol with (display name, description).
    let mut builtin_refs: BTreeMap<String, (String, String)> = BTreeMap::new();

    // Variables defined in included or including makefiles share the global
    // symbol of their definition, so references to them link across files.
    let user_defined: HashSet<String> = files
        .docs()
        .flat_map(|doc| {
            doc.makefile()
                .variable_definitions()
                .filter_map(|v| v.name())
                .collect::<Vec<_>>()
        })
        .collect();

    for raw in collect_targets(&makefile, text)
        .into_iter()
        .chain(collect_variable_definitions(&makefile, text))
        .chain(collect_variable_references(
            &makefile,
            &user_defined,
            &mut builtin_refs,
        ))
    {
        let mut roles = 0;
        if raw.is_definition {
            roles |= SymbolRole::Definition as i32;
        }
        occurrences.push(Occurrence {
            range: byte_range_to_scip(text, raw.start, raw.len),
            symbol: raw.symbol,
            symbol_roles: roles,
            syntax_kind: raw.syntax_kind.into(),
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

    let mut symbols: Vec<SymbolInformation> = defined
        .into_iter()
        .map(|(symbol, (name, kind))| SymbolInformation {
            documentation: builtin_documentation(&name, kind)
                .map(|doc| vec![doc.to_string()])
                .unwrap_or_default(),
            symbol,
            display_name: name,
            kind: kind.into(),
            ..Default::default()
        })
        .collect();

    // Document built-in/automatic variables that are referenced but not defined
    // here, reusing the same descriptions the LSP hover serves.
    symbols.extend(
        builtin_refs
            .into_iter()
            .map(|(symbol, (name, doc))| SymbolInformation {
                documentation: vec![doc],
                symbol,
                display_name: name,
                kind: symbol_information::Kind::Variable.into(),
                ..Default::default()
            }),
    );

    // Carry lint/parse diagnostics into the index as symbol-less occurrences,
    // so consumers like Sourcegraph can render them inline.
    occurrences.extend(diagnostic_occurrences(files));

    Document {
        language: "makefile".to_string(),
        relative_path: relative_path.to_string(),
        occurrences,
        symbols,
        position_encoding: PositionEncoding::UTF8CodeUnitOffsetFromLineStart.into(),
        ..Default::default()
    }
}

/// Documentation for a defined symbol that is a GNU Make built-in, if any.
///
/// Targets are matched against the special targets (`.PHONY`, `.NOTPARALLEL`,
/// ...); variables against the built-in variables (`CC`, `MAKE`, ...).
fn builtin_documentation(name: &str, kind: symbol_information::Kind) -> Option<&'static str> {
    match kind {
        symbol_information::Kind::Function => crate::builtins::find_special_target(name),
        symbol_information::Kind::Variable => crate::builtins::find_builtin_variable(name),
        _ => None,
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

        for (target, range) in targets_with_ranges(&rule) {
            out.push(RawOccurrence {
                symbol: target_symbol(&target),
                start: range.start().into(),
                len: range.len().into(),
                is_definition: true,
                syntax_kind: SyntaxKind::IdentifierFunctionDefinition,
            });
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
                    syntax_kind: SyntaxKind::IdentifierFunction,
                });
            }
        }
    }

    out
}

/// Collect variable definitions (the `NAME` in `NAME = value`).
fn collect_variable_definitions(makefile: &Makefile, text: &str) -> Vec<RawOccurrence> {
    let mut out = Vec::new();

    for var in makefile.variable_definitions() {
        let (Some(name), Some(range)) = (var.name(), var.name_range()) else {
            continue;
        };
        // Occurrences are single-line; skip names continued over lines.
        if text[range].contains('\n') {
            continue;
        }
        out.push(RawOccurrence {
            symbol: variable_symbol(&name),
            start: range.start().into(),
            len: range.len().into(),
            is_definition: true,
            syntax_kind: SyntaxKind::IdentifierMutableGlobal,
        });
    }

    out
}

/// Collect occurrences for every variable reference, classifying each name.
///
/// A name defined in the file set (in `user_defined`) is emitted as a
/// reference to the user's own symbol. Otherwise, if it is a built-in or
/// automatic variable (`$@`, `$<`, `$(MAKE)`, ...), it is emitted and its
/// documentation recorded in `docs`, reusing the same descriptions the LSP
/// hover serves. Names that are neither are skipped (they are reported by the
/// undefined-variable lint).
fn collect_variable_references(
    makefile: &Makefile,
    user_defined: &HashSet<String>,
    docs: &mut BTreeMap<String, (String, String)>,
) -> Vec<RawOccurrence> {
    let mut out = Vec::new();
    for r in makefile.variable_references() {
        let (Some(name), Some(range)) = (r.name(), r.name_range()) else {
            continue;
        };
        let symbol = variable_symbol(&name);
        if !user_defined.contains(&name) {
            // Built-in or automatic variable: attach its documentation. Unknown
            // names get no symbol so consumers don't see bogus references.
            let Some(doc) = builtin_variable_doc(&name) else {
                continue;
            };
            docs.entry(symbol.clone()).or_insert_with(|| (name, doc));
        }
        out.push(RawOccurrence {
            symbol,
            start: range.start().into(),
            len: range.len().into(),
            is_definition: false,
            syntax_kind: SyntaxKind::IdentifierMutableGlobal,
        });
    }
    out
}

/// Documentation for a built-in or automatic variable name, if known.
fn builtin_variable_doc(name: &str) -> Option<String> {
    crate::builtins::find_automatic_variable(name)
        .or_else(|| crate::builtins::find_builtin_variable(name).map(str::to_string))
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

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Build symbol-less occurrences carrying the file's diagnostics.
///
/// The diagnostics come from the LSP analysis, whose ranges are in UTF-16 code
/// units; we convert each back to a byte range and re-encode it in SCIP's
/// UTF-8-byte-from-line-start scheme so it matches the symbol occurrences.
fn diagnostic_occurrences(files: &FileSet) -> Vec<Occurrence> {
    let text = files.current().text();
    crate::diagnostics::get_file_set_diagnostics(files)
        .into_iter()
        .map(|diag| {
            let range = match try_lsp_range_to_text_range(text, &diag.range) {
                Some(r) => byte_span_to_scip(text, r.start().into(), r.end().into()),
                None => vec![0, 0, 0, 0],
            };
            Occurrence {
                range,
                diagnostics: vec![Diagnostic {
                    severity: severity_to_scip(diag.severity).into(),
                    code: diagnostic_code(&diag.code),
                    message: diag.message,
                    source: diag.source.unwrap_or_default(),
                    ..Default::default()
                }],
                ..Default::default()
            }
        })
        .collect()
}

/// Map an LSP diagnostic severity to its SCIP equivalent.
fn severity_to_scip(severity: Option<DiagnosticSeverity>) -> Severity {
    match severity {
        Some(DiagnosticSeverity::ERROR) => Severity::Error,
        Some(DiagnosticSeverity::WARNING) => Severity::Warning,
        Some(DiagnosticSeverity::INFORMATION) => Severity::Information,
        Some(DiagnosticSeverity::HINT) => Severity::Hint,
        _ => Severity::UnspecifiedSeverity,
    }
}

/// Render an LSP diagnostic code as the string SCIP expects.
fn diagnostic_code(code: &Option<NumberOrString>) -> String {
    match code {
        Some(NumberOrString::String(s)) => s.clone(),
        Some(NumberOrString::Number(n)) => n.to_string(),
        None => String::new(),
    }
}

/// Convert a byte range to a SCIP occurrence range
/// `[startLine, startChar, endLine, endChar]`, with characters as UTF-8 byte
/// offsets from the start of the line. The range stays on a single line since
/// all Makefile symbol names do.
fn byte_range_to_scip(text: &str, start: usize, len: usize) -> Vec<i32> {
    let (line, col) = byte_offset_to_line_col(text, start);
    vec![line, col, line, col + len as i32]
}

/// Convert a byte span `start..end` to a SCIP occurrence range, allowing the
/// span to cross line boundaries (diagnostics can cover whole rules).
fn byte_span_to_scip(text: &str, start: usize, end: usize) -> Vec<i32> {
    let (start_line, start_col) = byte_offset_to_line_col(text, start);
    let (end_line, end_col) = byte_offset_to_line_col(text, end);
    vec![start_line, start_col, end_line, end_col]
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

    fn single(text: &str) -> FileSet {
        let uri = "untitled:Makefile".parse().unwrap();
        FileSet::single(crate::workspace::Document::new(uri, text.to_string()))
    }

    fn src_file(relative_path: &str, text: &str) -> SourceFile {
        SourceFile {
            relative_path: relative_path.to_string(),
            files: single(text),
        }
    }

    fn build_single(relative_path: &str, text: &str) -> Document {
        build_document(relative_path, &single(text))
    }

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
        let doc = build_single("Makefile", text);

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
    fn test_target_definitions_in_rule_with_several_targets() {
        let text = "a-b b: c\n";
        let doc = build_single("Makefile", text);
        let definitions: Vec<(&str, &Vec<i32>)> = occ_symbols(&doc)
            .into_iter()
            .filter(|(_, _, is_definition)| *is_definition)
            .map(|(symbol, range, _)| (symbol, range))
            .collect();
        let a_b = target_symbol("a-b");
        let b = target_symbol("b");
        assert_eq!(
            definitions,
            vec![
                (a_b.as_str(), &vec![0, 0, 0, 3]),
                (b.as_str(), &vec![0, 4, 0, 5])
            ]
        );
    }

    #[test]
    fn test_variable_definition_and_reference() {
        let text = "CC = gcc\nall:\n\t$(CC) main.c\n";
        let doc = build_single("Makefile", text);

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
    fn test_variable_named_like_its_modifier() {
        let text = "override override = 1\n";
        let doc = build_single("Makefile", text);
        let definitions: Vec<(&str, &Vec<i32>)> = occ_symbols(&doc)
            .into_iter()
            .filter(|(_, _, is_definition)| *is_definition)
            .map(|(symbol, range, _)| (symbol, range))
            .collect();
        let symbol = variable_symbol("override");
        assert_eq!(definitions, vec![(symbol.as_str(), &vec![0, 9, 0, 17])]);
    }

    #[test]
    fn test_occurrence_syntax_kinds() {
        let text = "CC = gcc\nall: build\n\t$(CC) x.c\nbuild:\n\techo hi\n";
        let doc = build_single("Makefile", text);

        let cc = variable_symbol("CC");
        let all = target_symbol("all");
        let build = target_symbol("build");

        let kind = |sym: &str, def: bool| {
            doc.occurrences
                .iter()
                .find(|o| {
                    o.symbol == sym && (o.symbol_roles & SymbolRole::Definition as i32 != 0) == def
                })
                .unwrap()
                .syntax_kind
                .enum_value()
                .unwrap()
        };

        assert_eq!(kind(&all, true), SyntaxKind::IdentifierFunctionDefinition);
        assert_eq!(kind(&build, true), SyntaxKind::IdentifierFunctionDefinition);
        assert_eq!(kind(&build, false), SyntaxKind::IdentifierFunction);
        assert_eq!(kind(&cc, true), SyntaxKind::IdentifierMutableGlobal);
        assert_eq!(kind(&cc, false), SyntaxKind::IdentifierMutableGlobal);
    }

    #[test]
    fn test_symbol_table_lists_definitions() {
        let text = "CC = gcc\n\nall: build\n\techo ok\n";
        let doc = build_single("Makefile", text);

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
    fn test_special_target_documentation() {
        let text = "all: build\n\techo ok\nbuild:\n\techo b\n.PHONY: all\n";
        let doc = build_single("Makefile", text);
        let phony = doc
            .symbols
            .iter()
            .find(|s| s.display_name == ".PHONY")
            .unwrap();
        assert_eq!(
            phony.documentation,
            vec!["Declare targets that do not represent files.".to_string()]
        );
    }

    #[test]
    fn test_builtin_variable_documentation() {
        let text = "CC = gcc\nall:\n\t$(CC) main.c\n";
        let doc = build_single("Makefile", text);
        let cc = doc.symbols.iter().find(|s| s.display_name == "CC").unwrap();
        assert_eq!(
            cc.documentation,
            vec!["C compiler (default: cc).".to_string()]
        );
    }

    /// The documentation attached to the symbol that `name` resolves to.
    fn doc_for(doc: &Document, name: &str) -> Option<String> {
        doc.symbols
            .iter()
            .find(|s| s.display_name == name)
            .map(|s| s.documentation.join(""))
    }

    #[test]
    fn test_automatic_variable_documentation() {
        let text = "all:\n\t$(CC) -o $@ $<\n";
        let doc = build_single("Makefile", text);

        assert_eq!(
            doc_for(&doc, "@").as_deref(),
            Some("The file name of the target of the rule.")
        );
        assert_eq!(
            doc_for(&doc, "<").as_deref(),
            Some("The name of the first prerequisite.")
        );

        // The occurrences point at the name byte, not the `$`.
        let at = variable_symbol("@");
        assert!(doc.occurrences.iter().any(|o| o.symbol == at));
    }

    #[test]
    fn test_automatic_variable_variant_documentation() {
        let text = "all:\n\techo $(@D) $(^F)\n";
        let doc = build_single("Makefile", text);
        assert_eq!(
            doc_for(&doc, "@D").as_deref(),
            Some("The directory part of `$@`.")
        );
        assert_eq!(
            doc_for(&doc, "^F").as_deref(),
            Some("The file-within-directory part of `$^`.")
        );
    }

    #[test]
    fn test_builtin_variable_reference_documentation() {
        // MAKE and CURDIR are never defined here, but referenced.
        let text = "all:\n\tcd $(CURDIR) && $(MAKE) -C sub\n";
        let doc = build_single("Makefile", text);
        assert_eq!(
            doc_for(&doc, "CURDIR").as_deref(),
            Some("The absolute pathname of the current working directory.")
        );
        assert_eq!(
            doc_for(&doc, "MAKE").as_deref(),
            Some("The name of the make program being run.")
        );
    }

    #[test]
    fn test_user_definition_shadows_builtin_reference() {
        // CC is built-in, but defined here: it must be documented via the
        // definition path (which already handles built-in docs), and must not
        // appear twice in the symbol table.
        let text = "CC = clang\nall:\n\t$(CC) main.c\n";
        let doc = build_single("Makefile", text);
        let cc_syms: Vec<_> = doc
            .symbols
            .iter()
            .filter(|s| s.display_name == "CC")
            .collect();
        assert_eq!(cc_syms.len(), 1);
        assert_eq!(
            cc_syms[0].documentation,
            vec!["C compiler (default: cc).".to_string()]
        );
    }

    #[test]
    fn test_unknown_single_char_not_documented() {
        // $$ (escaped dollar) and unknown $x must not produce symbols.
        let text = "all:\n\techo $$HOME $x\n";
        let doc = build_single("Makefile", text);
        assert!(doc_for(&doc, "$").is_none());
        assert!(doc_for(&doc, "x").is_none());
    }

    #[test]
    fn test_nested_automatic_variable_documented() {
        // $@ nested inside $(dir ...) must still be documented.
        let text = "all:\n\techo $(dir $@)\n";
        let doc = build_single("Makefile", text);
        assert_eq!(
            doc_for(&doc, "@").as_deref(),
            Some("The file name of the target of the rule.")
        );
        let at = variable_symbol("@");
        assert!(doc.occurrences.iter().any(|o| o.symbol == at));
    }

    #[test]
    fn test_nested_builtin_variable_documented() {
        // CURDIR nested inside $(addprefix ...) must still be documented.
        let text = "all:\n\techo $(addprefix $(CURDIR)/,a b)\n";
        let doc = build_single("Makefile", text);
        assert_eq!(
            doc_for(&doc, "CURDIR").as_deref(),
            Some("The absolute pathname of the current working directory.")
        );
    }

    #[test]
    fn test_nested_user_variable_referenced() {
        // A user variable nested inside a function call is still referenced.
        let text = "SRC = a.c\nall:\n\techo $(notdir $(SRC))\n";
        let doc = build_single("Makefile", text);
        let src = variable_symbol("SRC");
        let refs = doc
            .occurrences
            .iter()
            .filter(|o| o.symbol == src && o.symbol_roles & SymbolRole::Definition as i32 == 0)
            .count();
        assert_eq!(refs, 1);
    }

    #[test]
    fn test_comment_variable_not_documented() {
        // A built-in mentioned in a full-line comment is not a reference: Make
        // never expands it.
        let text = "# see $(MAKE) docs\nall:\n\techo done\n";
        let doc = build_single("Makefile", text);
        assert!(doc_for(&doc, "MAKE").is_none());
        let make = variable_symbol("MAKE");
        assert!(!doc.occurrences.iter().any(|o| o.symbol == make));
    }

    #[test]
    fn test_recipe_comment_variable_documented() {
        // In a recipe, Make expands `$` before the shell sees `#`, so a variable
        // after a recipe-line `#` is a real reference.
        let text = "all:\n\techo hi # uses $(MAKE)\n";
        let doc = build_single("Makefile", text);
        assert_eq!(
            doc_for(&doc, "MAKE").as_deref(),
            Some("The name of the make program being run.")
        );
    }

    #[test]
    fn test_user_symbols_have_no_documentation() {
        let text = "FOO = bar\nall: build\n\techo ok\nbuild:\n\techo b\n";
        let doc = build_single("Makefile", text);
        for name in ["FOO", "all", "build"] {
            let sym = doc.symbols.iter().find(|s| s.display_name == name).unwrap();
            assert!(
                sym.documentation.is_empty(),
                "{name} should have no documentation"
            );
        }
    }

    #[test]
    fn test_brace_variable_reference() {
        let text = "CC = gcc\nall:\n\t${CC} main.c\n";
        let doc = build_single("Makefile", text);
        let cc = variable_symbol("CC");
        assert!(doc.occurrences.iter().any(|o| o.symbol == cc
            && o.range == vec![2, 3, 2, 5]
            && o.symbol_roles & SymbolRole::Definition as i32 == 0));
    }

    #[test]
    fn test_substitution_reference_in_recipe() {
        let text = "SRCS = a.c\nall:\n\techo $(SRCS:.c=.o)\n";
        let doc = build_single("Makefile", text);
        let srcs = variable_symbol("SRCS");
        let uses: Vec<_> = occ_symbols(&doc)
            .into_iter()
            .filter(|(s, _, def)| *s == srcs && !*def)
            .map(|(_, range, _)| range.clone())
            .collect();
        assert_eq!(uses, vec![vec![2, 8, 2, 12]]);
    }

    #[test]
    fn test_multiple_variable_uses() {
        let text = "CC = gcc\nall:\n\t$(CC) a.c\nclean:\n\t$(CC) --version\n";
        let doc = build_single("Makefile", text);
        let cc = variable_symbol("CC");
        let uses = occ_symbols(&doc)
            .into_iter()
            .filter(|(s, _, def)| *s == cc && !*def)
            .count();
        assert_eq!(uses, 2);
    }

    #[test]
    fn test_index_metadata() {
        let files = vec![src_file("Makefile", "all:\n\techo hi\n")];
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
        let doc = build_single("Makefile", "");
        assert!(doc.occurrences.is_empty());
        assert!(doc.symbols.is_empty());
    }

    #[test]
    fn test_index_protobuf_roundtrip() {
        use protobuf::Message;

        let files = vec![src_file(
            "Makefile",
            "CC = gcc\nall: build\n\t$(CC) x.c\nbuild:\n\techo hi\n",
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

    /// Diagnostics ride on symbol-less occurrences.
    fn diags(doc: &Document) -> Vec<(&Vec<i32>, &Diagnostic)> {
        doc.occurrences
            .iter()
            .filter(|o| o.symbol.is_empty())
            .flat_map(|o| o.diagnostics.iter().map(move |d| (&o.range, d)))
            .collect()
    }

    #[test]
    fn test_diagnostic_in_occurrence() {
        let doc = build_single("Makefile", "CFLAGS = $(MISSING) -Wall\n");
        let found = diags(&doc);
        assert_eq!(found.len(), 1);
        let (range, diag) = found[0];
        assert_eq!(*range, vec![0, 9, 0, 19]);
        assert_eq!(diag.code, "undefined-variable");
        assert_eq!(diag.severity.enum_value(), Ok(Severity::Warning));
        assert_eq!(diag.source, "makefile-lsp");
        assert_eq!(diag.message, "variable 'MISSING' is not defined");
    }

    #[test]
    fn test_multiline_diagnostic_range() {
        // A circular dependency is anchored on the whole rule, so the range
        // crosses line boundaries.
        let doc = build_single("Makefile", "a: b\n\techo a\nb: a\n\techo b\n");
        let circular: Vec<_> = diags(&doc)
            .into_iter()
            .filter(|(_, d)| d.code == "circular-dependency")
            .collect();
        assert_eq!(circular.len(), 1);
        let (range, _) = circular[0];
        assert_eq!(*range, vec![0, 0, 2, 0]);
    }

    #[test]
    fn test_no_diagnostics_for_clean_makefile() {
        let doc = build_single("Makefile", "all: build\n\techo done\nbuild:\n\techo b\n");
        assert!(diags(&doc).is_empty());
    }

    #[test]
    fn test_references_across_includes() {
        use crate::workspace::tests::Fixture;

        let fx = Fixture::new(&[
            ("Makefile", "X = $(OBJ)\ninclude rules.mk\n"),
            ("rules.mk", "OBJ = x.o\nexport Y = $(X)\n"),
        ]);
        let mut ws = crate::workspace::Workspace::new();
        ws.set_roots(vec![fx.path("")]);
        let files = ws.file_set_for_path(&fx.path("Makefile")).unwrap();
        let doc = build_document("Makefile", &files);
        let obj = variable_symbol("OBJ");
        assert_eq!(
            occ_symbols(&doc)
                .into_iter()
                .filter(|(s, _, _)| *s == obj)
                .map(|(_, range, def)| (range.clone(), def))
                .collect::<Vec<_>>(),
            vec![(vec![0, 6, 0, 9], false)]
        );
        assert_eq!(diags(&doc), vec![]);

        let files = ws.file_set_for_path(&fx.path("rules.mk")).unwrap();
        let doc = build_document("rules.mk", &files);
        assert!(occ_symbols(&doc).contains(&(
            variable_symbol("X").as_str(),
            &vec![1, 13, 1, 14],
            false
        )));
        assert_eq!(diags(&doc), vec![]);
    }
}
