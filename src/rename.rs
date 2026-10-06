//! Rename support for Makefiles.

use std::collections::HashMap;

use makefile_lossless::{variable_at_offset, Makefile};
use tower_lsp_server::ls_types::{
    Position, PrepareRenameResponse, Range, TextEdit, Uri, WorkspaceEdit,
};

use crate::position::{offset_to_position, try_position_to_offset};
use crate::references::{symbol_at, symbol_locations, Symbol};
use crate::workspace::FileSet;

/// Why a symbol can't be renamed.
#[derive(Debug, PartialEq, Eq)]
pub enum RenameError {
    /// The symbol is defined only outside the workspace.
    DefinedOutsideWorkspace(String, Uri),
    /// The symbol is used in a file outside the workspace, which would be
    /// left referring to the old name.
    UsedOutsideWorkspace(String, Uri),
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

/// Find the renameable symbol at `position`, checking that it may be renamed.
///
/// Variables must be defined in one of the files. Prerequisites that aren't
/// defined as targets anywhere (usually plain files) may be renamed. A symbol
/// defined only in files outside the workspace can't be renamed.
fn renameable_symbol(
    files: &FileSet,
    position: Position,
) -> Option<Result<(Symbol, usize), RenameError>> {
    let current = files.current();
    let byte_offset: usize = try_position_to_offset(current.text(), position)?.into();
    let symbol = symbol_at(&current.makefile(), current.text(), byte_offset)?;
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
    Some(Ok((symbol, byte_offset)))
}

/// Check if renaming is possible at the given position and return the current name and range.
pub fn prepare_rename(
    files: &FileSet,
    position: Position,
) -> Option<Result<PrepareRenameResponse, RenameError>> {
    let (symbol, byte_offset) = match renameable_symbol(files, position)? {
        Ok(found) => found,
        Err(e) => return Some(Err(e)),
    };
    let source_text = files.current().text();
    let name = symbol_name(&symbol);
    let start = if variable_at_offset(source_text, byte_offset).is_some() {
        find_var_name_start_in_ref(source_text, byte_offset)?
    } else {
        find_word_start(source_text, byte_offset)
    };
    let start_pos = offset_to_position(source_text, text_size::TextSize::from(start as u32));
    let end_pos = Position::new(start_pos.line, start_pos.character + name.len() as u32);
    Some(Ok(PrepareRenameResponse::RangeWithPlaceholder {
        range: Range::new(start_pos, end_pos),
        placeholder: name.to_string(),
    }))
}

/// Perform a rename of the symbol at the given position, in all files that
/// use it.
pub fn rename(
    files: &FileSet,
    position: Position,
    new_name: &str,
) -> Option<Result<WorkspaceEdit, RenameError>> {
    let (symbol, _) = match renameable_symbol(files, position)? {
        Ok(found) => found,
        Err(e) => return Some(Err(e)),
    };

    let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
    for doc in files.docs() {
        let edits: Vec<TextEdit> =
            symbol_locations(&doc.makefile(), doc.text(), doc.uri(), &symbol, true)
                .into_iter()
                .map(|loc| TextEdit {
                    range: loc.range,
                    new_text: new_name.to_string(),
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

/// Find the byte offset of the start of the word at the given offset.
fn find_word_start(text: &str, offset: usize) -> usize {
    let bytes = text.as_bytes();
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-';
    (0..offset)
        .rev()
        .take_while(|&i| is_ident(bytes[i]))
        .last()
        .unwrap_or(offset)
}

/// Find the start offset of the variable name within a $() or ${} reference.
fn find_var_name_start_in_ref(text: &str, offset: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = offset;
    while i >= 2 {
        i -= 1;
        if i > 0 && (bytes[i] == b'(' || bytes[i] == b'{') && bytes[i - 1] == b'$' {
            return Some(i + 1);
        }
        if bytes[i] == b')' || bytes[i] == b'}' || bytes[i] == b'\n' {
            return None;
        }
    }
    None
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
}
