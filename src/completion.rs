//! Completion provider for Makefiles.

use std::path::Path;

use makefile_lossless::{Makefile, TextRange, TextSize};
use tower_lsp_server::ls_types::{CompletionItem, CompletionItemKind, Documentation, Position};

use crate::builtins;
use crate::position::try_position_to_offset;

/// Get completions for a Makefile at the given position.
///
/// `makefiles` holds the current makefile first, followed by the other
/// makefiles visible from it (included or including ones), whose targets and
/// variables are offered too. `base_dir` is the directory of the source file;
/// used to resolve relative paths when offering filesystem completions for
/// prerequisites.
pub fn get_completions(
    makefiles: &[Makefile],
    source_text: &str,
    position: Position,
    base_dir: Option<&Path>,
) -> Vec<CompletionItem> {
    let lines: Vec<&str> = source_text.lines().collect();
    let line = lines.get(position.line as usize).copied().unwrap_or("");
    // Byte offset of the cursor within the line; the LSP column is in UTF-16
    // code units.
    let col: usize = try_position_to_offset(line, Position::new(0, position.character))
        .map_or(line.len(), Into::into);
    let prefix = &line[..col];

    // The current makefile and the offset in it.
    let at = makefiles
        .first()
        .zip(try_position_to_offset(source_text, position));

    // In a recipe line, offer function and variable completions after $
    if at.is_some_and(|(makefile, offset)| in_recipe(makefile, source_text, offset)) {
        if prefix.ends_with("$(") {
            let mut items = get_function_completions();
            items.extend(get_variable_reference_completions(makefiles));
            return items;
        }
        if prefix.ends_with('$') {
            return get_automatic_variable_completions();
        }
        return vec![];
    }

    if let Some((makefile, offset)) = at {
        // In the file names of an include directive, offer filesystem
        // completions, ranking common Makefile fragment names first.
        if let Some(partial) = include_partial(makefile, source_text, offset) {
            return get_include_completions(partial, base_dir);
        }

        // If the cursor sits in the prerequisites area, offer target names and
        // filesystem paths matching whatever is being typed.
        if in_prerequisites(makefile, offset) {
            let byte_offset: usize = offset.into();
            return get_prerequisite_completions(makefiles, source_text, byte_offset, base_dir);
        }
    }

    let typing_variable = position.character > 0 && !line.contains('=') && !line.contains(':');
    match words_before_cursor(prefix).as_deref() {
        Some([]) if line.trim().is_empty() => {
            let mut items = get_directive_completions(|_| true);
            items.extend(get_target_completions(makefiles));
            return items;
        }
        Some([]) if typing_variable => {
            let mut items = get_directive_completions(|_| true);
            items.extend(get_variable_completions(makefiles));
            return items;
        }
        Some(["else"]) => {
            return get_directive_completions(|name| {
                builtins::CONDITIONAL_DIRECTIVES.contains(&name)
            });
        }
        Some(["override"]) => {
            let mut items = get_directive_completions(|name| name == "define");
            items.extend(get_variable_completions(makefiles));
            return items;
        }
        _ => {}
    }

    // If typing a variable name (no = or : yet), offer variable completions
    if typing_variable {
        return get_variable_completions(makefiles);
    }

    // After $( in any context, offer function and variable completions
    if prefix.ends_with("$(") {
        let mut items = get_function_completions();
        items.extend(get_variable_reference_completions(makefiles));
        return items;
    }

    vec![]
}

/// Whether `offset` is in the prerequisite list of a rule, including at its
/// end, where the next prerequisite is typed.
fn in_prerequisites(makefile: &Makefile, offset: TextSize) -> bool {
    makefile.rules().any(|rule| {
        rule.prerequisite_list_range()
            .is_some_and(|range| range.contains_inclusive(offset))
    })
}

/// Return the complete words on the line before the word being typed, or
/// `None` if the cursor is past a point where a directive could appear (after
/// `:`, `=`, a variable reference or a comment).
fn words_before_cursor(prefix: &str) -> Option<Vec<&str>> {
    if prefix.contains([':', '=', '$', '#']) {
        return None;
    }
    let mut words: Vec<&str> = prefix.split_whitespace().collect();
    if !prefix.ends_with(char::is_whitespace) {
        words.pop();
    }
    Some(words)
}

/// Generate completions for the directives accepted by `filter`.
fn get_directive_completions(filter: impl Fn(&str) -> bool) -> Vec<CompletionItem> {
    builtins::DIRECTIVES
        .iter()
        .filter(|d| filter(d.name))
        .map(|d| {
            let takes_args = !matches!(d.name, "else" | "endif" | "endef");
            CompletionItem {
                label: d.name.to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                detail: Some(d.syntax.to_string()),
                documentation: Some(Documentation::String(d.doc.to_string())),
                insert_text: Some(if takes_args {
                    format!("{} ", d.name)
                } else {
                    d.name.to_string()
                }),
                ..Default::default()
            }
        })
        .collect()
}

/// Generate target name completions including built-in special targets.
fn get_target_completions(makefiles: &[Makefile]) -> Vec<CompletionItem> {
    let existing_targets: Vec<String> = makefiles
        .iter()
        .flat_map(|m| m.rules())
        .flat_map(|r| r.targets().collect::<Vec<_>>())
        .collect();

    builtins::SPECIAL_TARGETS
        .iter()
        .filter(|(name, _)| !existing_targets.iter().any(|t| t == name))
        .map(|(name, desc)| CompletionItem {
            label: name.to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            detail: Some(desc.to_string()),
            insert_text: Some(format!("{}: ", name)),
            ..Default::default()
        })
        .collect()
}

/// Generate variable name completions from variables defined in the files.
fn get_variable_completions(makefiles: &[Makefile]) -> Vec<CompletionItem> {
    let mut seen = std::collections::HashSet::new();
    makefiles
        .iter()
        .flat_map(|m| m.variable_definitions())
        .filter_map(|v| {
            let name = v.name().filter(|n| seen.insert(n.clone()))?;
            Some(CompletionItem {
                label: name.clone(),
                kind: Some(CompletionItemKind::VARIABLE),
                detail: v.raw_value().map(|v| format!("= {}", v.trim())),
                insert_text: Some(format!("{} = ", name)),
                ..Default::default()
            })
        })
        .collect()
}

/// Generate automatic variable completions for use after $.
///
/// Single-character variables (`$@`, `$<`, ...) insert bare; the `D`/`F`
/// variants insert wrapped in `(...)` (e.g. `$(@D)`). Both are derived from the
/// shared [`builtins`] tables so completion, hover, and the SCIP index agree.
fn get_automatic_variable_completions() -> Vec<CompletionItem> {
    let single = builtins::AUTOMATIC_VARIABLES
        .iter()
        .map(|(name, doc)| (format!("${}", name), name.to_string(), doc.to_string()));
    let variants = builtins::AUTOMATIC_VARIABLE_VARIANTS.iter().map(|name| {
        let doc = builtins::find_automatic_variable(name).unwrap_or_default();
        (format!("$({})", name), format!("({})", name), doc)
    });
    single
        .chain(variants)
        .map(|(label, insert, detail)| CompletionItem {
            label,
            kind: Some(CompletionItemKind::VARIABLE),
            detail: Some(detail),
            insert_text: Some(insert),
            ..Default::default()
        })
        .collect()
}

/// Generate function completions for use after $(.
fn get_function_completions() -> Vec<CompletionItem> {
    builtins::BUILTIN_FUNCTIONS
        .iter()
        .map(|f| {
            let insert = format!("{} ", f.name);
            let sig = format!("$({} {})", f.name, f.params.join(","));
            CompletionItem {
                label: f.name.to_string(),
                kind: Some(CompletionItemKind::FUNCTION),
                detail: Some(format!("{}: {}", sig, f.doc)),
                insert_text: Some(insert),
                ..Default::default()
            }
        })
        .collect()
}

/// Generate variable reference completions for use after `$(`: well-known
/// built-in variables (`$(MAKE)`, `$(CURDIR)`, ...) plus variables defined in
/// the file. The inserted text closes the parenthesis so accepting `MAKE`
/// yields `$(MAKE)`.
fn get_variable_reference_completions(makefiles: &[Makefile]) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for (name, desc) in builtins::BUILTIN_VARIABLES {
        if !seen.insert((*name).to_string()) {
            continue;
        }
        items.push(CompletionItem {
            label: name.to_string(),
            kind: Some(CompletionItemKind::VARIABLE),
            detail: Some(desc.to_string()),
            insert_text: Some(format!("{})", name)),
            ..Default::default()
        });
    }

    for v in makefiles.iter().flat_map(|m| m.variable_definitions()) {
        let Some(name) = v.name() else {
            continue;
        };
        if !seen.insert(name.clone()) {
            continue;
        }
        items.push(CompletionItem {
            label: name.clone(),
            kind: Some(CompletionItemKind::VARIABLE),
            detail: v.raw_value().map(|val| format!("= {}", val.trim())),
            insert_text: Some(format!("{})", name)),
            ..Default::default()
        });
    }

    items
}

/// Whether `offset` is on the lines of an item covering `range`, which ends
/// with the item's line ending unless it is at the end of the file.
fn on_item_lines(source_text: &str, range: TextRange, offset: TextSize) -> bool {
    range.contains(offset)
        || (offset == range.end() && !source_text[..usize::from(offset)].ends_with('\n'))
}

/// Whether `offset` is in a recipe.
fn in_recipe(makefile: &Makefile, source_text: &str, offset: TextSize) -> bool {
    makefile
        .recipe_nodes()
        .any(|recipe| on_item_lines(source_text, recipe.text_range(), offset))
}

/// If `offset` is in the file names of an `include`, `-include` or
/// `sinclude` directive, return the part of the file name before it.
fn include_partial<'a>(
    makefile: &Makefile,
    source_text: &'a str,
    offset: TextSize,
) -> Option<&'a str> {
    let include = makefile
        .includes()
        .find(|include| on_item_lines(source_text, include.text_range(), offset))?;
    if !matches!(
        include.keyword()?.as_str(),
        "include" | "-include" | "sinclude"
    ) || offset <= include.keyword_range()?.end()
    {
        return None;
    }
    let start = include
        .path_ranges()
        .find(|range| range.contains_inclusive(offset))
        .map_or(offset, |range| range.start());
    Some(&source_text[TextRange::new(start, offset)])
}

/// Common Makefile fragment naming patterns, used to rank include completions.
fn is_makefile_fragment(name: &str) -> bool {
    name.ends_with(".mk")
        || name.ends_with(".make")
        || name.starts_with("Makefile.")
        || name.starts_with("makefile.")
        || name == "Makefile"
        || name == "makefile"
        || name == "GNUmakefile"
}

/// Generate filesystem completions for an include directive path, ranking
/// common Makefile fragment names (`*.mk`, `Makefile.*`, ...) ahead of other
/// entries.
fn get_include_completions(partial: &str, base_dir: Option<&Path>) -> Vec<CompletionItem> {
    let Some(base) = base_dir else {
        return Vec::new();
    };

    // TODO: also offer files from `-I`/`--include-dir` search directories once
    // those are tracked; for now we only complete paths relative to base_dir.
    filesystem_completions(base, partial)
        .into_iter()
        .map(|mut item| {
            let is_dir = item.kind == Some(CompletionItemKind::FOLDER);
            let basename = item.label.rsplit('/').next().unwrap_or(&item.label);
            // Sort directories and Makefile fragments first; LSP clients order
            // by sort_text lexically, so prefix with a rank digit.
            let rank = if is_dir || is_makefile_fragment(basename) {
                '0'
            } else {
                '1'
            };
            item.sort_text = Some(format!("{}{}", rank, item.label));
            item
        })
        .collect()
}

/// Generate completions for the prerequisites part of a rule: existing targets
/// defined in the makefile plus filesystem entries matching the partial word
/// the user is typing.
fn get_prerequisite_completions(
    makefiles: &[Makefile],
    source_text: &str,
    byte_offset: usize,
    base_dir: Option<&Path>,
) -> Vec<CompletionItem> {
    let partial = partial_word_before(source_text, byte_offset);

    let mut items = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // Targets defined elsewhere in this Makefile (excluding the one on the
    // current line, which we can't easily disambiguate without more parsing —
    // duplicates are fine, GNU Make accepts a target as its own prerequisite
    // only with explicit hand-written intent anyway).
    for rule in makefiles.iter().flat_map(|m| m.rules()) {
        for target in rule.targets() {
            // Skip pattern rules and special targets like `.PHONY`.
            if target.contains('%') || target.starts_with('.') {
                continue;
            }
            if !seen.insert(target.clone()) {
                continue;
            }
            items.push(CompletionItem {
                label: target.clone(),
                kind: Some(CompletionItemKind::REFERENCE),
                detail: Some("target".to_string()),
                insert_text: Some(target),
                ..Default::default()
            });
        }
    }

    if let Some(base) = base_dir {
        for item in filesystem_completions(base, partial) {
            if seen.insert(item.label.clone()) {
                items.push(item);
            }
        }
    }

    items
}

/// Return the partial word ending at `byte_offset` — everything from the last
/// whitespace or `:` back through `byte_offset`. Used to figure out which
/// directory to list for filesystem completions.
fn partial_word_before(source_text: &str, byte_offset: usize) -> &str {
    let line_start = source_text[..byte_offset]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let segment = &source_text[line_start..byte_offset];
    let start_in_segment = segment
        .rfind(|c: char| c.is_whitespace() || c == ':')
        .map(|i| i + 1)
        .unwrap_or(0);
    &segment[start_in_segment..]
}

/// List filesystem entries under `base_dir` that match the directory implied
/// by `partial`. When `partial` contains a `/`, we recurse into the
/// corresponding subdirectory and complete the basename portion; otherwise we
/// list `base_dir`. Returns labels that, when accepted, replace the typed
/// prefix portion (the basename), keeping any leading directory portion intact
/// because the LSP client matches against `label`/`filter_text` from the
/// trigger character backwards through word boundaries — we include the full
/// path in `insert_text`.
fn filesystem_completions(base_dir: &Path, partial: &str) -> Vec<CompletionItem> {
    let (dir_part, basename_prefix) = match partial.rsplit_once('/') {
        Some((dir, base)) => (dir, base),
        None => ("", partial),
    };

    let dir_to_list = if dir_part.is_empty() {
        base_dir.to_path_buf()
    } else if Path::new(dir_part).is_absolute() {
        Path::new(dir_part).to_path_buf()
    } else {
        base_dir.join(dir_part)
    };

    let Ok(entries) = std::fs::read_dir(&dir_to_list) else {
        return Vec::new();
    };

    let mut items = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        // Skip hidden files unless the user explicitly typed a leading dot.
        if name.starts_with('.') && !basename_prefix.starts_with('.') {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let display = if dir_part.is_empty() {
            name.to_string()
        } else {
            format!("{}/{}", dir_part, name)
        };
        let insert = if is_dir {
            format!("{}/", display)
        } else {
            display.clone()
        };
        items.push(CompletionItem {
            label: display,
            kind: Some(if is_dir {
                CompletionItemKind::FOLDER
            } else {
                CompletionItemKind::FILE
            }),
            insert_text: Some(insert),
            ..Default::default()
        });
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_completions_empty_line() {
        let text = "all: build\n\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions = get_completions(&[makefile], text, Position::new(1, 0), None);
        assert!(!completions.is_empty());
        assert!(completions.iter().any(|c| c.label == ".PHONY"));
    }

    #[test]
    fn test_completions_exclude_existing_targets() {
        let text = ".PHONY: all\n\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions = get_completions(&[makefile], text, Position::new(1, 0), None);
        assert!(!completions.iter().any(|c| c.label == ".PHONY"));
    }

    #[test]
    fn test_completions_in_recipe() {
        let text = "all:\n\t";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions = get_completions(&[makefile], text, Position::new(1, 1), None);
        assert!(completions.is_empty());
    }

    #[test]
    fn test_variable_completions() {
        let text = "CC = gcc\nCFLAGS = -Wall\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions = get_completions(&[makefile], text, Position::new(2, 1), None);
        // Should not crash, may offer variable completions
        let _ = completions;
    }

    #[test]
    fn test_completions_in_recipe_builtin_variables() {
        let text = "all:\n\t$(";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions = get_completions(&[makefile], text, Position::new(1, 3), None);
        let make = completions.iter().find(|c| c.label == "MAKE").unwrap();
        assert_eq!(make.insert_text.as_deref(), Some("MAKE)"));
        assert!(completions.iter().any(|c| c.label == "MAKEFLAGS"));
        assert!(completions.iter().any(|c| c.label == "CURDIR"));
        // Functions are still offered alongside variables.
        assert!(completions.iter().any(|c| c.label == "wildcard"));
    }

    #[test]
    fn test_completions_in_recipe_user_variables() {
        let text = "CC = gcc\nall:\n\t$(";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions = get_completions(&[makefile], text, Position::new(2, 3), None);
        let cc = completions.iter().find(|c| c.label == "CC").unwrap();
        assert_eq!(cc.insert_text.as_deref(), Some("CC)"));
    }

    #[test]
    fn test_variable_reference_completions_dedups() {
        // A user-defined variable that shares a name with a built-in should
        // appear once, taking the built-in's slot.
        let text = "CC = clang\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let items = get_variable_reference_completions(&[makefile]);
        let cc: Vec<_> = items.iter().filter(|c| c.label == "CC").collect();
        assert_eq!(cc.len(), 1);
    }

    #[test]
    fn test_function_completions() {
        let completions = get_function_completions();
        assert!(!completions.is_empty());
        assert!(completions.iter().any(|c| c.label == "subst"));
        assert!(completions.iter().any(|c| c.label == "wildcard"));
    }

    #[test]
    fn test_automatic_variable_completions_cover_all_variants() {
        let completions = get_automatic_variable_completions();
        // Single-character forms insert bare.
        let at = completions.iter().find(|c| c.label == "$@").unwrap();
        assert_eq!(at.insert_text.as_deref(), Some("@"));
        // Every D/F variant is offered, including the ^/+/?/* ones the hover and
        // SCIP index now document.
        for variant in builtins::AUTOMATIC_VARIABLE_VARIANTS {
            let label = format!("$({})", variant);
            let item = completions
                .iter()
                .find(|c| c.label == label)
                .unwrap_or_else(|| panic!("missing completion for {}", label));
            assert_eq!(
                item.insert_text.as_deref(),
                Some(&*format!("({})", variant))
            );
        }
    }

    #[test]
    fn test_prerequisite_target_completions() {
        let text = "build:\n\techo build\n\ntest:\n\techo test\n\nall: \n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        // Position cursor right after "all: "
        let completions = get_completions(&[makefile], text, Position::new(6, 5), None);
        let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
        assert!(
            labels.contains(&"build"),
            "expected 'build' in {:?}",
            labels
        );
        assert!(labels.contains(&"test"), "expected 'test' in {:?}", labels);
    }

    #[test]
    fn test_prerequisite_excludes_pattern_and_special_targets() {
        let text = ".PHONY: build\n\n%.o: %.c\n\techo compile\n\nbuild:\n\techo build\n\nall: \n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions = get_completions(&[makefile], text, Position::new(8, 5), None);
        let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
        assert!(labels.contains(&"build"));
        assert!(!labels.iter().any(|l| l.contains('%')));
        assert!(!labels.iter().any(|l| l.starts_with('.')));
    }

    #[test]
    fn test_prerequisite_completions_on_continuation_line() {
        let text = "build:\nall: a \\\n  \n";
        let makefile = Makefile::parse(text).tree();
        let completions = get_completions(&[makefile], text, Position::new(2, 2), None);
        let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, vec!["build", "all"]);
    }

    #[test]
    fn test_no_prerequisite_completions_in_variable_value() {
        let text = "build:\nFOO := b";
        let makefile = Makefile::parse(text).tree();
        let completions = get_completions(&[makefile], text, Position::new(1, 8), None);
        assert_eq!(completions, vec![]);
    }

    #[test]
    fn test_prerequisite_filesystem_completions() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.c"), "").unwrap();
        std::fs::write(dir.path().join("util.c"), "").unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();

        let text = "all: \n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions = get_completions(&[makefile], text, Position::new(0, 5), Some(dir.path()));

        let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
        assert!(labels.contains(&"main.c"), "got {:?}", labels);
        assert!(labels.contains(&"util.c"));
        assert!(labels.contains(&"src"));

        let src_item = completions.iter().find(|c| c.label == "src").unwrap();
        assert_eq!(src_item.kind, Some(CompletionItemKind::FOLDER));
        assert_eq!(src_item.insert_text.as_deref(), Some("src/"));
    }

    #[test]
    fn test_prerequisite_filesystem_subdir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src").join("main.c"), "").unwrap();
        std::fs::write(dir.path().join("src").join("util.c"), "").unwrap();

        let text = "all: src/\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions = get_completions(&[makefile], text, Position::new(0, 9), Some(dir.path()));

        let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
        assert!(labels.contains(&"src/main.c"), "got {:?}", labels);
        assert!(labels.contains(&"src/util.c"));
    }

    #[test]
    fn test_prerequisite_filesystem_skips_hidden() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("visible.c"), "").unwrap();
        std::fs::write(dir.path().join(".hidden"), "").unwrap();

        let text = "all: \n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions = get_completions(&[makefile], text, Position::new(0, 5), Some(dir.path()));
        let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
        assert!(labels.contains(&"visible.c"));
        assert!(!labels.contains(&".hidden"));
    }

    #[test]
    fn test_prerequisite_filesystem_includes_hidden_when_typed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("visible.c"), "").unwrap();
        std::fs::write(dir.path().join(".hidden"), "").unwrap();

        let text = "all: .\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions = get_completions(&[makefile], text, Position::new(0, 6), Some(dir.path()));
        let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
        assert!(labels.contains(&".hidden"), "got {:?}", labels);
    }

    /// The include partial in `text` at its end.
    fn partial_at_end(text: &str) -> Option<String> {
        let makefile = Makefile::parse(text).tree();
        include_partial(&makefile, text, TextSize::of(text)).map(str::to_string)
    }

    #[test]
    fn test_include_partial() {
        assert_eq!(partial_at_end("include "), Some(String::new()));
        assert_eq!(partial_at_end("include foo.mk"), Some("foo.mk".to_string()));
        assert_eq!(partial_at_end("-include .env"), Some(".env".to_string()));
        assert_eq!(partial_at_end("sinclude bar"), Some("bar".to_string()));
        assert_eq!(partial_at_end("  include foo"), Some("foo".to_string()));
        assert_eq!(
            partial_at_end("include a.mk b.mk"),
            Some("b.mk".to_string())
        );
        assert_eq!(partial_at_end("include"), None);
        assert_eq!(partial_at_end("all: foo"), None);
        assert_eq!(partial_at_end("includex foo"), None);
    }

    #[test]
    fn test_include_completions_rank_fragments_first() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.mk"), "").unwrap();
        std::fs::write(dir.path().join("README.txt"), "").unwrap();
        std::fs::write(dir.path().join("Makefile.local"), "").unwrap();

        let text = "include \n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions = get_completions(&[makefile], text, Position::new(0, 8), Some(dir.path()));

        let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
        assert!(labels.contains(&"config.mk"), "got {:?}", labels);
        assert!(labels.contains(&"Makefile.local"));
        assert!(labels.contains(&"README.txt"));

        let mk = completions.iter().find(|c| c.label == "config.mk").unwrap();
        let readme = completions
            .iter()
            .find(|c| c.label == "README.txt")
            .unwrap();
        assert!(
            mk.sort_text.as_deref() < readme.sort_text.as_deref(),
            "fragment {:?} should rank before {:?}",
            mk.sort_text,
            readme.sort_text
        );
    }

    #[test]
    fn test_include_completions_for_earlier_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("rules")).unwrap();
        let text = "include ru b.mk\n";
        let parsed = Makefile::parse(text);
        let completions = get_completions(
            &[parsed.tree()],
            text,
            Position::new(0, 10),
            Some(dir.path()),
        );
        let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, vec!["rules"]);
    }

    #[test]
    fn test_no_include_completions_on_next_line() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("rules")).unwrap();
        let text = "include a.mk\n\n";
        let parsed = Makefile::parse(text);
        let completions = get_completions(
            &[parsed.tree()],
            text,
            Position::new(1, 0),
            Some(dir.path()),
        );
        assert!(completions.iter().all(|c| c.label != "rules"));
    }

    #[test]
    fn test_completions_in_recipe_with_recipe_prefix() {
        let completions = labels(".RECIPEPREFIX = >\nall:\n>echo $\n", Position::new(2, 7));
        assert!(
            completions.contains(&"$@".to_string()),
            "got {:?}",
            completions
        );
    }

    #[test]
    fn test_include_completions_partial_filter() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.mk"), "").unwrap();
        std::fs::create_dir(dir.path().join("rules")).unwrap();
        std::fs::write(dir.path().join("rules").join("common.mk"), "").unwrap();

        let text = "include rules/\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions =
            get_completions(&[makefile], text, Position::new(0, 14), Some(dir.path()));
        let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
        assert!(labels.contains(&"rules/common.mk"), "got {:?}", labels);
    }

    fn labels(text: &str, pos: Position) -> Vec<String> {
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        get_completions(&[makefile], text, pos, None)
            .into_iter()
            .map(|c| c.label)
            .collect()
    }

    #[test]
    fn test_directive_completions_on_empty_line() {
        let completions = labels("all:\n\n", Position::new(1, 0));
        let directives: Vec<&str> = builtins::DIRECTIVES.iter().map(|d| d.name).collect();
        assert_eq!(&completions[..directives.len()], &directives[..]);
        assert!(completions.contains(&".PHONY".to_string()));
    }

    #[test]
    fn test_directive_completions_while_typing() {
        let text = "CC = gcc\nifd\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions = get_completions(&[makefile], text, Position::new(1, 3), None);
        let ifdef = completions.iter().find(|c| c.label == "ifdef").unwrap();
        assert_eq!(ifdef.kind, Some(CompletionItemKind::KEYWORD));
        assert_eq!(ifdef.insert_text.as_deref(), Some("ifdef "));
        let endif = completions.iter().find(|c| c.label == "endif").unwrap();
        assert_eq!(endif.insert_text.as_deref(), Some("endif"));
        // Variable names are still offered.
        assert!(completions.iter().any(|c| c.label == "CC"));
    }

    #[test]
    fn test_no_directive_completions_after_first_word() {
        let completions = labels("FOO ba\n", Position::new(0, 6));
        assert!(!completions.contains(&"include".to_string()));
        let completions = labels("all: in\n", Position::new(0, 7));
        assert!(!completions.contains(&"include".to_string()));
    }

    #[test]
    fn test_no_directive_completions_in_recipe() {
        assert_eq!(
            labels("all:\n\tin\n", Position::new(1, 3)),
            Vec::<String>::new()
        );
    }

    #[test]
    fn test_conditional_completions_after_else() {
        assert_eq!(
            labels("ifdef A\nelse \nendif\n", Position::new(1, 5)),
            vec!["ifeq", "ifneq", "ifdef", "ifndef"]
        );
        assert_eq!(
            labels("ifdef A\nelse ifn\nendif\n", Position::new(1, 8)),
            vec!["ifeq", "ifneq", "ifdef", "ifndef"]
        );
    }

    #[test]
    fn test_define_completion_after_override() {
        assert_eq!(
            labels("CC = gcc\noverride \n", Position::new(1, 9)),
            vec!["define", "CC"]
        );
    }

    #[test]
    fn test_words_before_cursor() {
        assert_eq!(words_before_cursor(""), Some(vec![]));
        assert_eq!(words_before_cursor("inc"), Some(vec![]));
        assert_eq!(words_before_cursor("else "), Some(vec!["else"]));
        assert_eq!(words_before_cursor("else if"), Some(vec!["else"]));
        assert_eq!(words_before_cursor("all: "), None);
        assert_eq!(words_before_cursor("FOO = x"), None);
        assert_eq!(words_before_cursor("$(fo"), None);
    }

    #[test]
    fn test_partial_word_before() {
        assert_eq!(partial_word_before("all: src/", 9), "src/");
        assert_eq!(partial_word_before("all: main", 9), "main");
        assert_eq!(partial_word_before("all: ", 5), "");
        assert_eq!(partial_word_before("all: foo bar", 12), "bar");
    }

    fn labels_in(fx: &crate::workspace::tests::Fixture, pos: Position) -> Vec<String> {
        let set = fx.file_set("Makefile");
        let makefiles: Vec<Makefile> = set.docs().map(|d| d.makefile()).collect();
        let mut labels: Vec<String> = get_completions(&makefiles, set.current().text(), pos, None)
            .into_iter()
            .filter(|c| c.kind != Some(CompletionItemKind::FUNCTION))
            .filter(|c| {
                !builtins::BUILTIN_VARIABLES
                    .iter()
                    .any(|(n, _)| *n == c.label)
            })
            .map(|c| c.label)
            .collect();
        labels.sort();
        labels
    }

    #[test]
    fn test_completions_include_variables_from_included_files() {
        let fx = crate::workspace::tests::Fixture::new(&[
            ("Makefile", "include rules.mk\nLOCAL = 1\nall:\n\t$(\n"),
            ("rules.mk", "RULES_VAR = x\nLOCAL = 2\n"),
        ]);
        assert_eq!(
            labels_in(&fx, Position::new(3, 3)),
            vec!["LOCAL".to_string(), "RULES_VAR".to_string()]
        );
    }

    #[test]
    fn test_prerequisite_completions_include_targets_from_included_files() {
        let fx = crate::workspace::tests::Fixture::new(&[
            ("Makefile", "include rules.mk\nall: \n"),
            ("rules.mk", "build:\n\techo\n"),
        ]);
        assert_eq!(
            labels_in(&fx, Position::new(1, 5)),
            vec!["all".to_string(), "build".to_string()]
        );
    }

    #[test]
    fn test_completions_after_non_ascii_comment() {
        // The cursor sits right after the two-byte 'é'; treating the UTF-16
        // column as a byte offset would slice inside it.
        assert_eq!(
            labels("# h\u{e9}llo\n", Position::new(0, 4)),
            Vec::<String>::new()
        );
    }

    #[test]
    fn test_completions_in_recipe_after_non_ascii() {
        let completions = labels("all:\n\techo \u{e9} $(\n", Position::new(1, 10));
        assert!(completions.contains(&"wildcard".to_string()));
        assert!(!completions.contains(&"$@".to_string()));
    }

    #[test]
    fn test_completions_in_recipe_after_surrogate_pair() {
        let completions = labels("all:\n\t\u{1f600} $(\n", Position::new(1, 6));
        assert!(completions.contains(&"wildcard".to_string()));
    }

    #[test]
    fn test_function_completions_in_value_after_non_ascii() {
        let completions = labels("X = \u{e9} $(\n", Position::new(0, 8));
        assert!(completions.contains(&"wildcard".to_string()));
    }
}
