//! Rename support for Makefiles.

use std::collections::HashMap;

use makefile_lossless::{Makefile, TextRange};
use tower_lsp_server::ls_types::{
    Position, PrepareRenameResponse, Range, TextEdit, Uri, WorkspaceEdit,
};

use crate::position::{text_range_to_lsp_range, try_position_to_offset};
use crate::references::{single_char_reference_ranges, symbol_at, symbol_locations, Symbol};
use crate::workspace::{Document, FileSet};

/// Why a symbol can't be renamed.
#[derive(Debug, PartialEq, Eq)]
pub enum RenameError {
    /// The symbol is defined only outside the workspace.
    DefinedOutsideWorkspace(String, Uri),
    /// The symbol is used in a file outside the workspace, which would be
    /// left referring to the old name.
    UsedOutsideWorkspace(String, Uri),
    /// The name contains a variable reference, so it is only known after
    /// expansion.
    ComputedName(String),
}

impl std::fmt::Display for RenameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenameError::DefinedOutsideWorkspace(name, uri) => write!(
                f,
                "'{}' is defined outside the workspace, in {}",
                name,
                uri.as_str()
            ),
            RenameError::UsedOutsideWorkspace(name, uri) => write!(
                f,
                "'{}' is used outside the workspace, in {}",
                name,
                uri.as_str()
            ),
            RenameError::ComputedName(name) => write!(
                f,
                "'{}' is computed from variable references and can't be renamed",
                name
            ),
        }
    }
}

fn is_defined(makefile: &Makefile, symbol: &Symbol) -> bool {
    match symbol {
        Symbol::Variable(name) => makefile
            .variable_definitions()
            .any(|v| v.name().as_deref() == Some(name)),
        Symbol::Target(name) => makefile.rules().any(|r| r.targets().any(|t| &t == name)),
    }
}

fn symbol_name(symbol: &Symbol) -> &str {
    match symbol {
        Symbol::Variable(name) | Symbol::Target(name) => name,
    }
}

/// Whether the occurrence of `symbol` at `position` contains a variable
/// reference, as in `$(P)_FLAGS`, so that its name is only known after
/// expansion.
fn is_computed(doc: &Document, symbol: &Symbol, position: Position) -> bool {
    let makefile = doc.makefile();
    let text = doc.text();
    let offset = |pos| try_position_to_offset(text, pos).expect("symbol location outside document");
    symbol_locations(&makefile, text, doc.uri(), symbol, true)
        .into_iter()
        .filter(|loc| loc.range.start <= position && position <= loc.range.end)
        .map(|loc| TextRange::new(offset(loc.range.start), offset(loc.range.end)))
        .any(|range| {
            makefile
                .variable_references()
                .any(|reference| range.contains_range(reference.text_range()))
        })
}

/// Find the renameable symbol at `position`, checking that it may be renamed.
///
/// Variables must be defined in one of the files. Prerequisites that aren't
/// defined as targets anywhere (usually plain files) may be renamed. A symbol
/// defined only in files outside the workspace can't be renamed.
pub(crate) fn renameable_symbol(
    files: &FileSet,
    position: Position,
) -> Option<Result<Symbol, RenameError>> {
    let current = files.current();
    let byte_offset: usize = try_position_to_offset(current.text(), position)?.into();
    let symbol = symbol_at(&current.makefile(), byte_offset)?;
    let defining: Vec<&Uri> = files
        .docs()
        .filter(|d| is_defined(&d.makefile(), &symbol))
        .map(|d| d.uri())
        .collect();
    if defining.is_empty() && matches!(symbol, Symbol::Variable(_)) {
        return None;
    }
    if !defining.is_empty() && !defining.iter().any(|u| files.is_editable(u)) {
        let name = symbol_name(&symbol).to_string();
        return Some(Err(RenameError::DefinedOutsideWorkspace(
            name,
            defining[0].clone(),
        )));
    }
    if is_computed(current, &symbol, position) {
        return Some(Err(RenameError::ComputedName(
            symbol_name(&symbol).to_string(),
        )));
    }
    Some(Ok(symbol))
}

/// Check if renaming is possible at the given position and return the range
/// of the occurrence there, with its text as placeholder.
///
/// The range is one of those that [`rename`] edits, so it covers the name
/// as written, e.g. `a\#b` for the target `a#b`.
pub fn prepare_rename(
    files: &FileSet,
    position: Position,
) -> Option<Result<PrepareRenameResponse, RenameError>> {
    let symbol = match renameable_symbol(files, position)? {
        Ok(found) => found,
        Err(e) => return Some(Err(e)),
    };
    let current = files.current();
    let source_text = current.text();
    let range = symbol_locations(
        &current.makefile(),
        source_text,
        current.uri(),
        &symbol,
        true,
    )
    .into_iter()
    .map(|loc| loc.range)
    .find(|r| r.start <= position && position <= r.end)?;
    let offset =
        |pos| try_position_to_offset(source_text, pos).expect("symbol location outside document");
    let start = offset(range.start);
    let end = offset(range.end);
    Some(Ok(PrepareRenameResponse::RangeWithPlaceholder {
        range,
        placeholder: source_text[TextRange::new(start, end)].to_string(),
    }))
}

/// Perform a rename of the symbol at the given position, in all files that
/// use it.
pub fn rename(
    files: &FileSet,
    position: Position,
    new_name: &str,
) -> Option<Result<WorkspaceEdit, RenameError>> {
    let symbol = match renameable_symbol(files, position)? {
        Ok(found) => found,
        Err(e) => return Some(Err(e)),
    };

    let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
    for doc in files.docs() {
        let makefile = doc.makefile();
        // `$X` renamed to `$NEW` would be read as `$(N)EW`.
        let needs_parens: Vec<Range> = if new_name.chars().count() > 1 {
            single_char_reference_ranges(&makefile)
                .into_iter()
                .map(|r| text_range_to_lsp_range(doc.text(), r))
                .collect()
        } else {
            vec![]
        };
        let edits: Vec<TextEdit> =
            symbol_locations(&makefile, doc.text(), doc.uri(), &symbol, true)
                .into_iter()
                .map(|loc| TextEdit {
                    new_text: if needs_parens.contains(&loc.range) {
                        format!("({new_name})")
                    } else {
                        new_name.to_string()
                    },
                    range: loc.range,
                })
                .collect();
        if edits.is_empty() {
            continue;
        }
        if !files.is_editable(doc.uri()) {
            let name = symbol_name(&symbol).to_string();
            return Some(Err(RenameError::UsedOutsideWorkspace(
                name,
                doc.uri().clone(),
            )));
        }
        changes.insert(doc.uri().clone(), edits);
    }

    Some(Ok(WorkspaceEdit {
        changes: Some(changes),
        ..Default::default()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::tests::Fixture;
    use crate::workspace::{Document, Workspace};

    fn test_uri() -> Uri {
        "file:///test/Makefile".parse().unwrap()
    }

    fn single(text: &str) -> FileSet {
        FileSet::single(Document::new(test_uri(), text.to_string()))
    }

    fn get_edits(text: &str, pos: Position, new_name: &str) -> Vec<TextEdit> {
        let uri = test_uri();
        let result = rename(&single(text), pos, new_name);
        result
            .map(|r| r.unwrap())
            .and_then(|ws| ws.changes.and_then(|c| c.get(&uri).cloned()))
            .unwrap_or_default()
    }

    #[test]
    fn test_rename_variable() {
        let text = "CC = gcc\nall:\n\t$(CC) main.c\n";
        let edits = get_edits(text, Position::new(0, 0), "GCC");
        assert_eq!(edits.len(), 2);
        // First edit: definition
        assert_eq!(edits[0].new_text, "GCC");
        assert_eq!(edits[0].range.start.line, 0);
        // Second edit: usage
        assert_eq!(edits[1].new_text, "GCC");
        assert_eq!(edits[1].range.start.line, 2);
    }

    #[test]
    fn test_rename_variable_from_reference() {
        let text = "CC = gcc\nall:\n\t$(CC) main.c\n";
        let edits = get_edits(text, Position::new(2, 3), "GCC");
        assert_eq!(edits.len(), 2);
    }

    #[test]
    fn test_rename_target() {
        let text = "all: build\n\nbuild:\n\techo ok\n";
        let edits = get_edits(text, Position::new(2, 0), "compile");
        assert_eq!(edits.len(), 2);
        assert!(edits.iter().all(|e| e.new_text == "compile"));
    }

    #[test]
    fn test_rename_target_from_prereq() {
        let text = "all: build\n\nbuild:\n\techo ok\n";
        let edits = get_edits(text, Position::new(0, 5), "compile");
        assert_eq!(edits.len(), 2);
    }

    #[test]
    fn test_rename_second_target_of_rule() {
        let text = "all: b\na b: c\n";
        let edits = get_edits(text, Position::new(0, 5), "x");
        let edit = |line, start, end| TextEdit {
            range: Range::new(Position::new(line, start), Position::new(line, end)),
            new_text: "x".to_string(),
        };
        assert_eq!(edits, vec![edit(0, 5, 6), edit(1, 2, 3)]);
    }

    fn edit(line: u32, start: u32, end: u32, new_text: &str) -> TextEdit {
        TextEdit {
            range: Range::new(Position::new(line, start), Position::new(line, end)),
            new_text: new_text.to_string(),
        }
    }

    #[test]
    fn test_rename_single_char_reference_to_longer_name() {
        // `$NEW` would be read as `$(N)EW`.
        let text = "X = 1\nY = $X ${X}\nall: $X\n";
        assert_eq!(
            get_edits(text, Position::new(0, 0), "NEW"),
            vec![
                edit(0, 0, 1, "NEW"),
                edit(1, 5, 6, "(NEW)"),
                edit(1, 9, 10, "NEW"),
                edit(2, 6, 7, "(NEW)"),
            ]
        );
    }

    #[test]
    fn test_rename_single_char_reference_in_recipe() {
        let text = "X = 1\nall:\n\techo $X\n";
        assert_eq!(
            get_edits(text, Position::new(0, 0), "NEW"),
            vec![edit(0, 0, 1, "NEW"), edit(2, 7, 8, "(NEW)")]
        );
    }

    #[test]
    fn test_rename_single_char_reference_to_single_char() {
        let text = "X = 1\nY = $X $(X)\n";
        assert_eq!(
            get_edits(text, Position::new(1, 5), "Z"),
            vec![edit(0, 0, 1, "Z"), edit(1, 5, 6, "Z"), edit(1, 9, 10, "Z")]
        );
    }

    #[test]
    fn test_rename_to_single_char() {
        let text = "FOO = 1\nY = $(FOO)\n";
        assert_eq!(
            get_edits(text, Position::new(0, 0), "F"),
            vec![edit(0, 0, 3, "F"), edit(1, 6, 9, "F")]
        );
    }

    #[test]
    fn test_rename_second_target_of_rule_from_definition() {
        let text = "all: b\na b: c\n";
        let edits = get_edits(text, Position::new(1, 2), "x");
        let edit = |line, start, end| TextEdit {
            range: Range::new(Position::new(line, start), Position::new(line, end)),
            new_text: "x".to_string(),
        };
        assert_eq!(edits, vec![edit(0, 5, 6), edit(1, 2, 3)]);
    }

    #[test]
    fn test_prepare_rename_variable() {
        let text = "CC = gcc\nall:\n\t$(CC) main.c\n";
        let result = prepare_rename(&single(text), Position::new(0, 0));
        assert!(result.is_some());
        match result.unwrap().unwrap() {
            PrepareRenameResponse::RangeWithPlaceholder { placeholder, .. } => {
                assert_eq!(placeholder, "CC");
            }
            _ => panic!("Expected RangeWithPlaceholder"),
        }
    }

    fn prepared(text: &str, pos: Position) -> Option<(Range, String)> {
        match prepare_rename(&single(text), pos)?.unwrap() {
            PrepareRenameResponse::RangeWithPlaceholder { range, placeholder } => {
                Some((range, placeholder))
            }
            other => panic!("Expected RangeWithPlaceholder, got {:?}", other),
        }
    }

    fn range(line: u32, start: u32, end: u32) -> Range {
        Range::new(Position::new(line, start), Position::new(line, end))
    }

    #[test]
    fn test_prepare_rename_escaped_target() {
        let text = "a\\#b:\n\techo\n";
        let expected = Some((range(0, 0, 4), "a\\#b".to_string()));
        assert_eq!(prepared(text, Position::new(0, 0)), expected);
        assert_eq!(prepared(text, Position::new(0, 3)), expected);
        assert_eq!(
            get_edits(text, Position::new(0, 3), "x"),
            vec![TextEdit {
                range: range(0, 0, 4),
                new_text: "x".to_string()
            }]
        );
    }

    #[test]
    fn test_rename_escaped_prerequisite() {
        let text = "all: a\\#b\na\\#b:\n";
        let expected = Some((range(0, 5, 9), "a\\#b".to_string()));
        assert_eq!(prepared(text, Position::new(0, 5)), expected);
        assert_eq!(prepared(text, Position::new(0, 8)), expected);
        let edit = |line, start, end| TextEdit {
            range: range(line, start, end),
            new_text: "x".to_string(),
        };
        assert_eq!(
            get_edits(text, Position::new(0, 8), "x"),
            vec![edit(0, 5, 9), edit(1, 0, 4)]
        );
        assert_eq!(
            get_edits(text, Position::new(1, 0), "x"),
            vec![edit(0, 5, 9), edit(1, 0, 4)]
        );
    }

    #[test]
    fn test_rename_order_only_prerequisite() {
        let text = "all: | dir\ndir:\n";
        let edit = |line, start, end| TextEdit {
            range: range(line, start, end),
            new_text: "x".to_string(),
        };
        assert_eq!(
            get_edits(text, Position::new(1, 0), "x"),
            vec![edit(0, 7, 10), edit(1, 0, 3)]
        );
    }

    #[test]
    fn test_prepare_rename_variable_references() {
        let text = "FOO = 1\nX = $(FOO) ${FOO}\n";
        assert_eq!(
            prepared(text, Position::new(1, 7)),
            Some((range(1, 6, 9), "FOO".to_string()))
        );
        assert_eq!(
            prepared(text, Position::new(1, 13)),
            Some((range(1, 13, 16), "FOO".to_string()))
        );
    }

    #[test]
    fn test_rename_substitution_reference() {
        let text = "FOO = a.c\nall: $(FOO:.c=.o)\n\techo ${FOO:.c=.o}\n";
        assert_eq!(
            prepared(text, Position::new(1, 8)),
            Some((range(1, 7, 10), "FOO".to_string()))
        );
        let edit = |line, start, end| TextEdit {
            range: range(line, start, end),
            new_text: "SRCS".to_string(),
        };
        let expected = vec![edit(0, 0, 3), edit(1, 7, 10), edit(2, 8, 11)];
        assert_eq!(get_edits(text, Position::new(1, 8), "SRCS"), expected);
        assert_eq!(get_edits(text, Position::new(0, 0), "SRCS"), expected);
    }

    #[test]
    fn test_rename_variable_in_vpath_and_define() {
        let text = "FOO = %.c\nvpath $(FOO) src\ndefine $(FOO)_F\n\techo $(FOO)\nendef\n";
        let edit = |line, start, end| TextEdit {
            range: range(line, start, end),
            new_text: "PAT".to_string(),
        };
        assert_eq!(
            get_edits(text, Position::new(0, 0), "PAT"),
            vec![
                edit(0, 0, 3),
                edit(1, 8, 11),
                edit(2, 9, 12),
                edit(3, 8, 11)
            ]
        );
    }

    #[test]
    fn test_no_rename_nested_or_parameter_reference() {
        let text = "define F\n$(1) $(A.${B})\nendef\n";
        assert_eq!(prepared(text, Position::new(1, 2)), None);
        assert_eq!(prepared(text, Position::new(1, 7)), None);
    }

    #[test]
    fn test_prepare_rename_exported_variable() {
        let text = "export FOO = 1\nall:\n\techo $(FOO)\n";
        assert_eq!(
            prepared(text, Position::new(0, 7)),
            Some((range(0, 7, 10), "FOO".to_string()))
        );
        let edit = |line, start, end| TextEdit {
            range: range(line, start, end),
            new_text: "BAR".to_string(),
        };
        assert_eq!(
            get_edits(text, Position::new(2, 8), "BAR"),
            vec![edit(0, 7, 10), edit(2, 8, 11)]
        );
    }

    #[test]
    fn test_prepare_rename_nothing() {
        let text = "all:\n\techo hello\n";
        let result = prepare_rename(&single(text), Position::new(1, 2));
        assert!(result.is_none());
    }

    #[test]
    fn test_rename_variable_multiple_usages() {
        let text = "CC = gcc\nall:\n\t$(CC) main.c\nclean:\n\t$(CC) --version\n";
        let edits = get_edits(text, Position::new(0, 0), "COMPILER");
        assert_eq!(edits.len(), 3);
    }

    fn edit_summary(edit: WorkspaceEdit) -> Vec<(Uri, u32, u32, String)> {
        let mut out: Vec<_> = edit
            .changes
            .unwrap()
            .into_iter()
            .flat_map(|(uri, edits)| {
                edits.into_iter().map(move |e| {
                    (
                        uri.clone(),
                        e.range.start.line,
                        e.range.start.character,
                        e.new_text,
                    )
                })
            })
            .collect();
        out.sort_by(|a, b| (a.0.as_str(), a.1, a.2).cmp(&(b.0.as_str(), b.1, b.2)));
        out
    }

    #[test]
    fn test_rename_variable_across_files() {
        let fx = Fixture::new(&[
            ("Makefile", "include rules.mk\nall:\n\t$(CC) x\n"),
            ("rules.mk", "CC = gcc\n"),
        ]);
        let edit = rename(&fx.file_set("Makefile"), Position::new(2, 3), "GCC")
            .unwrap()
            .unwrap();
        assert_eq!(
            edit_summary(edit),
            vec![
                (fx.uri("Makefile"), 2, 3, "GCC".to_string()),
                (fx.uri("rules.mk"), 0, 0, "GCC".to_string()),
            ]
        );
    }

    #[test]
    fn test_rename_target_from_included_file() {
        let fx = Fixture::new(&[
            ("Makefile", "include rules.mk\nall: build\n"),
            ("rules.mk", "build:\n\techo\n"),
        ]);
        let (mut ws, makefile) = fx.open("Makefile");
        ws.file_set(&makefile).unwrap();
        let rules = fx.open_in(&mut ws, "rules.mk");
        let edit = rename(
            &ws.file_set(&rules).unwrap(),
            Position::new(0, 0),
            "compile",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            edit_summary(edit),
            vec![
                (makefile, 1, 5, "compile".to_string()),
                (rules, 0, 0, "compile".to_string()),
            ]
        );
    }

    #[test]
    fn test_rename_refused_for_symbol_defined_outside_workspace() {
        let fx = Fixture::new(&[
            ("ws/Makefile", "include ../sys.mk\nall:\n\t$(SYSVAR)\n"),
            ("sys.mk", "SYSVAR = 1\n"),
        ]);
        let mut ws = Workspace::new();
        ws.set_roots(vec![fx.path("ws")]);
        let uri = fx.open_in(&mut ws, "ws/Makefile");
        let set = ws.file_set(&uri).unwrap();
        let expected = RenameError::DefinedOutsideWorkspace("SYSVAR".to_string(), fx.uri("sys.mk"));
        assert_eq!(
            prepare_rename(&set, Position::new(2, 3))
                .unwrap()
                .unwrap_err(),
            expected
        );
        assert_eq!(
            rename(&set, Position::new(2, 3), "X").unwrap().unwrap_err(),
            expected
        );
    }

    #[test]
    fn test_rename_refused_for_symbol_used_outside_workspace() {
        let fx = Fixture::new(&[
            ("ws/Makefile", "VAR = 1\ninclude ../sys.mk\n"),
            ("sys.mk", "all:\n\t$(VAR)\n"),
        ]);
        let mut ws = Workspace::new();
        ws.set_roots(vec![fx.path("ws")]);
        let uri = fx.open_in(&mut ws, "ws/Makefile");
        let set = ws.file_set(&uri).unwrap();
        assert_eq!(
            rename(&set, Position::new(0, 0), "X").unwrap().unwrap_err(),
            RenameError::UsedOutsideWorkspace("VAR".to_string(), fx.uri("sys.mk"))
        );
    }

    #[test]
    fn test_rename_variable_in_ifdef() {
        let text = "FOO = 1\nifdef FOO\nendif\n";
        let ranges: Vec<Range> = get_edits(text, Position::new(1, 6), "BAR")
            .into_iter()
            .map(|e| e.range)
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
    fn test_rename_variable_in_bsd_condition() {
        let text = "FOO = 1\n.if defined(FOO)\n.endif\n";
        let ranges: Vec<Range> = get_edits(text, Position::new(1, 12), "BAR")
            .into_iter()
            .map(|e| e.range)
            .collect();
        assert_eq!(
            ranges,
            vec![
                Range::new(Position::new(0, 0), Position::new(0, 3)),
                Range::new(Position::new(1, 12), Position::new(1, 15)),
            ]
        );
    }

    #[test]
    fn test_rename_refused_for_computed_variable_name() {
        let text = "P = a\n$(P)_FLAGS = x\nall:\n\techo $(a_FLAGS) $($(P)_FLAGS)\n";
        let refused = || Some(Some(RenameError::ComputedName("$(P)_FLAGS".to_string())));
        for pos in [
            Position::new(1, 0),
            Position::new(1, 6),
            Position::new(3, 25),
        ] {
            assert_eq!(
                prepare_rename(&single(text), pos).map(Result::err),
                refused()
            );
            assert_eq!(
                rename(&single(text), pos, "NEW").map(Result::err),
                refused()
            );
        }
    }

    #[test]
    fn test_rename_reference_in_computed_variable_name() {
        let text = "P = a\n$(P)_FLAGS = x\n";
        assert_eq!(
            prepared(text, Position::new(1, 2)),
            Some((range(1, 2, 3), "P".to_string()))
        );
        let edit = |line, start, end| TextEdit {
            range: range(line, start, end),
            new_text: "Q".to_string(),
        };
        assert_eq!(
            get_edits(text, Position::new(1, 2), "Q"),
            vec![edit(0, 0, 1), edit(1, 2, 3)]
        );
    }

    #[test]
    fn test_rename_refused_for_computed_target_name() {
        let text = "all: $(P)_bin\n$(P)_bin:\n\techo\n";
        let refused = || Some(Some(RenameError::ComputedName("$(P)_bin".to_string())));
        for pos in [Position::new(0, 10), Position::new(1, 5)] {
            assert_eq!(
                prepare_rename(&single(text), pos).map(Result::err),
                refused()
            );
            assert_eq!(
                rename(&single(text), pos, "NEW").map(Result::err),
                refused()
            );
        }
    }
}
