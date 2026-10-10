//! Completion provider for Makefiles.

use std::path::{Path, PathBuf};

use makefile_lossless::{Makefile, MakefileVariant, TextRange, TextSize};
use tower_lsp_server::ls_types::{
    CompletionItem, CompletionItemKind, CompletionTextEdit, Documentation, InsertTextFormat,
    InsertTextMode, Position, Range, TextEdit,
};

use crate::builtins;
use crate::position::try_position_to_offset;

/// Get completions for a Makefile at the given position.
///
/// `makefiles` holds the current makefile first, followed by the other
/// makefiles visible from it (included or including ones), whose targets and
/// variables are offered too. `base_dir` is the directory of the source file;
/// used to resolve relative paths when offering filesystem completions for
/// prerequisites. `include_dirs` are the other directories searched for
/// included files. `snippets` is the make variant to offer snippets for, or
/// `None` if the client does not support snippets.
pub fn get_completions(
    makefiles: &[Makefile],
    source_text: &str,
    position: Position,
    base_dir: Option<&Path>,
    include_dirs: &[PathBuf],
    snippets: Option<MakefileVariant>,
) -> Vec<CompletionItem> {
    let lines: Vec<&str> = source_text.lines().collect();
    let line = lines.get(position.line as usize).copied().unwrap_or("");
    // Byte offset of the cursor within the line; the LSP column is in UTF-16
    // code units.
    let col: usize = try_position_to_offset(line, Position::new(0, position.character))
        .map_or(line.len(), Into::into);
    let prefix = &line[..col];
    let suffix = &line[col..];
    let function_snippets = snippets.is_some();

    // The current makefile and the offset in it.
    let at = makefiles
        .first()
        .zip(try_position_to_offset(source_text, position));

    // In a recipe line, offer function and variable completions after $
    if at.is_some_and(|(makefile, offset)| in_recipe(makefile, source_text, offset)) {
        if prefix.ends_with("$(") {
            let mut items = get_function_completions(function_snippets, suffix);
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
            return get_include_completions(partial, base_dir, include_dirs);
        }

        // If the cursor sits in the prerequisites area, offer target names and
        // filesystem paths matching whatever is being typed.
        if in_prerequisites(makefile, offset) {
            let byte_offset: usize = offset.into();
            return get_prerequisite_completions(makefiles, source_text, byte_offset, base_dir);
        }
    }

    let typing_variable = position.character > 0 && !line.contains('=') && !line.contains(':');
    // Snippets insert whole lines, so only offer them when nothing follows.
    let line_snippets = || {
        let (Some(variant), Some((makefile, offset))) = (snippets, at) else {
            return vec![];
        };
        if !suffix.trim().is_empty() {
            return vec![];
        }
        let word = prefix.trim_start();
        let start = position.character - word.encode_utf16().count() as u32;
        let range = Range::new(Position::new(position.line, start), position);
        get_line_snippets(variant, recipe_prefix(makefile, variant, offset), range)
    };
    match words_before_cursor(prefix).as_deref() {
        Some([]) if line.trim().is_empty() => {
            let mut items = get_directive_completions(|_| true);
            items.extend(get_target_completions(makefiles));
            items.extend(line_snippets());
            return items;
        }
        Some([]) if typing_variable => {
            let mut items = get_directive_completions(|_| true);
            items.extend(get_variable_completions(makefiles));
            items.extend(line_snippets());
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
        let mut items = get_function_completions(function_snippets, suffix);
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

/// Generate function completions for use after $(. As snippets, they insert
/// placeholders for the arguments and close the parenthesis unless `suffix`,
/// the rest of the line, already starts with one.
fn get_function_completions(snippets: bool, suffix: &str) -> Vec<CompletionItem> {
    builtins::BUILTIN_FUNCTIONS
        .iter()
        .map(|f| {
            let sig = format!("$({} {})", f.name, f.params.join(","));
            let (insert, format) = if snippets {
                let args: Vec<String> = f
                    .params
                    .iter()
                    .enumerate()
                    .map(|(i, p)| format!("${{{}:{}}}", i + 1, escape_snippet(p)))
                    .collect();
                let close = if suffix.starts_with(')') { "" } else { ")$0" };
                let insert = format!("{} {}{}", f.name, args.join(","), close);
                (insert, Some(InsertTextFormat::SNIPPET))
            } else {
                (format!("{} ", f.name), None)
            };
            CompletionItem {
                label: f.name.to_string(),
                kind: Some(CompletionItemKind::FUNCTION),
                detail: Some(format!("{}: {}", sig, f.doc)),
                insert_text: Some(insert),
                insert_text_format: format,
                ..Default::default()
            }
        })
        .collect()
}

/// A snippet for a construct starting at the beginning of a line. A tab in
/// `body` stands for the recipe prefix.
struct LineSnippet {
    label: &'static str,
    detail: &'static str,
    body: &'static str,
}

const RULE_SNIPPET: LineSnippet = LineSnippet {
    label: "rule",
    detail: "Rule with a recipe",
    body: "${1:target}: ${2:prerequisites}\n\t$0",
};

const PHONY_RULE_SNIPPET: LineSnippet = LineSnippet {
    label: "phony rule",
    detail: "Rule for a target that is not a file",
    body: ".PHONY: ${1:target}\n${1:target}: ${2:prerequisites}\n\t$0",
};

const SUFFIX_RULE_SNIPPET: LineSnippet = LineSnippet {
    label: "suffix rule",
    detail: "Suffix rule making .o files from .c files",
    body: ".${1:c}.${2:o}:\n\t$0",
};

const GNU_SNIPPETS: &[LineSnippet] = &[
    RULE_SNIPPET,
    PHONY_RULE_SNIPPET,
    LineSnippet {
        label: "pattern rule",
        detail: "Pattern rule compiling .c files to .o files",
        body: "%.${1:o}: %.${2:c}\n\t${0:\\$(CC) \\$(CPPFLAGS) \\$(CFLAGS) -c -o \\$@ \\$<}",
    },
    LineSnippet {
        label: "ifeq/endif",
        detail: "Conditional on two values being equal",
        body: "ifeq (${1:\\$(VAR)},${2:value})\n$0\nendif",
    },
    LineSnippet {
        label: "ifeq/else/endif",
        detail: "Conditional on two values being equal, with an else branch",
        body: "ifeq (${1:\\$(VAR)},${2:value})\n$3\nelse\n$0\nendif",
    },
    LineSnippet {
        label: "ifdef/endif",
        detail: "Conditional on a variable being defined",
        body: "ifdef ${1:VAR}\n$0\nendif",
    },
    LineSnippet {
        label: "ifdef/else/endif",
        detail: "Conditional on a variable being defined, with an else branch",
        body: "ifdef ${1:VAR}\n$2\nelse\n$0\nendif",
    },
    LineSnippet {
        label: "ifndef/endif",
        detail: "Conditional on a variable not being defined",
        body: "ifndef ${1:VAR}\n$0\nendif",
    },
    LineSnippet {
        label: "define/endef",
        detail: "Multi-line variable",
        body: "define ${1:name}\n$0\nendef",
    },
];

const BSD_SNIPPETS: &[LineSnippet] = &[
    RULE_SNIPPET,
    PHONY_RULE_SNIPPET,
    SUFFIX_RULE_SNIPPET,
    LineSnippet {
        label: ".if/.endif",
        detail: "Conditional",
        body: ".if ${1:condition}\n$0\n.endif",
    },
    LineSnippet {
        label: ".if/.else/.endif",
        detail: "Conditional with an else branch",
        body: ".if ${1:condition}\n$2\n.else\n$0\n.endif",
    },
    LineSnippet {
        label: ".ifdef/.endif",
        detail: "Conditional on a variable being defined",
        body: ".ifdef ${1:VAR}\n$0\n.endif",
    },
    LineSnippet {
        label: ".for/.endfor",
        detail: "Loop over the words of a list",
        body: ".for ${1:item} in ${2:list}\n$0\n.endfor",
    },
];

const NMAKE_SNIPPETS: &[LineSnippet] = &[
    RULE_SNIPPET,
    LineSnippet {
        label: "inference rule",
        detail: "Inference rule making .obj files from .c files",
        body: ".${1:c}.${2:obj}:\n\t$0",
    },
    LineSnippet {
        label: "!IF/!ENDIF",
        detail: "Conditional",
        body: "!IF ${1:condition}\n$0\n!ENDIF",
    },
    LineSnippet {
        label: "!IF/!ELSE/!ENDIF",
        detail: "Conditional with an else branch",
        body: "!IF ${1:condition}\n$2\n!ELSE\n$0\n!ENDIF",
    },
    LineSnippet {
        label: "!IFDEF/!ENDIF",
        detail: "Conditional on a macro being defined",
        body: "!IFDEF ${1:MACRO}\n$0\n!ENDIF",
    },
];

const POSIX_SNIPPETS: &[LineSnippet] = &[RULE_SNIPPET, PHONY_RULE_SNIPPET, SUFFIX_RULE_SNIPPET];

/// Generate the line snippets for `variant`, replacing `range`, the word
/// typed so far.
fn get_line_snippets(
    variant: MakefileVariant,
    recipe_prefix: char,
    range: Range,
) -> Vec<CompletionItem> {
    let snippets = match variant {
        MakefileVariant::BSDMake => BSD_SNIPPETS,
        MakefileVariant::NMake => NMAKE_SNIPPETS,
        MakefileVariant::POSIXMake => POSIX_SNIPPETS,
        _ => GNU_SNIPPETS,
    };
    let prefix = escape_snippet(&recipe_prefix.to_string());
    snippets
        .iter()
        .map(|s| CompletionItem {
            label: s.label.to_string(),
            kind: Some(CompletionItemKind::SNIPPET),
            detail: Some(s.detail.to_string()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            // Keep the client from reindenting the lines, which would break
            // recipes.
            insert_text_mode: Some(InsertTextMode::AS_IS),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit::new(
                range,
                s.body.replace('\t', &prefix),
            ))),
            ..Default::default()
        })
        .collect()
}

/// Escape the characters that are special in snippet text.
fn escape_snippet(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '$' | '}' | '\\') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

/// The recipe prefix in effect at `offset`: the one set with `.RECIPEPREFIX`
/// for GNU make, or a tab.
// TODO: take .RECIPEPREFIX set in included files into account.
fn recipe_prefix(makefile: &Makefile, variant: MakefileVariant, offset: TextSize) -> char {
    if variant != MakefileVariant::GNUMake {
        return '\t';
    }
    makefile.recipe_prefix_at(offset)
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

/// Generate filesystem completions for an include directive path, from
/// `base_dir` and then `include_dirs`, ranking common Makefile fragment names
/// (`*.mk`, `Makefile.*`, ...) ahead of other entries.
///
/// make's default include directories are left out, since they mostly hold
/// C headers.
fn get_include_completions(
    partial: &str,
    base_dir: Option<&Path>,
    include_dirs: &[PathBuf],
) -> Vec<CompletionItem> {
    let mut seen = std::collections::HashSet::new();
    base_dir
        .into_iter()
        .chain(include_dirs.iter().map(PathBuf::as_path))
        .flat_map(|dir| filesystem_completions(dir, partial))
        .filter(|item| seen.insert(item.label.clone()))
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
        .rfind(|c: char| c.is_ascii_whitespace() || c == ':')
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
        let completions = get_completions(&[makefile], text, Position::new(1, 0), None, &[], None);
        assert!(!completions.is_empty());
        assert!(completions.iter().any(|c| c.label == ".PHONY"));
    }

    #[test]
    fn test_completions_exclude_existing_targets() {
        let text = ".PHONY: all\n\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions = get_completions(&[makefile], text, Position::new(1, 0), None, &[], None);
        assert!(!completions.iter().any(|c| c.label == ".PHONY"));
    }

    #[test]
    fn test_completions_in_recipe() {
        let text = "all:\n\t";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions = get_completions(&[makefile], text, Position::new(1, 1), None, &[], None);
        assert!(completions.is_empty());
    }

    #[test]
    fn test_variable_completions() {
        let text = "CC = gcc\nCFLAGS = -Wall\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions = get_completions(&[makefile], text, Position::new(2, 1), None, &[], None);
        // Should not crash, may offer variable completions
        let _ = completions;
    }

    #[test]
    fn test_completions_in_recipe_builtin_variables() {
        let text = "all:\n\t$(";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions = get_completions(&[makefile], text, Position::new(1, 3), None, &[], None);
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
        let completions = get_completions(&[makefile], text, Position::new(2, 3), None, &[], None);
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
        let completions = get_function_completions(false, "");
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
        let completions = get_completions(&[makefile], text, Position::new(6, 5), None, &[], None);
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
        let completions = get_completions(&[makefile], text, Position::new(8, 5), None, &[], None);
        let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
        assert!(labels.contains(&"build"));
        assert!(!labels.iter().any(|l| l.contains('%')));
        assert!(!labels.iter().any(|l| l.starts_with('.')));
    }

    #[test]
    fn test_prerequisite_completions_on_continuation_line() {
        let text = "build:\nall: a \\\n  \n";
        let makefile = Makefile::parse(text).tree();
        let completions = get_completions(&[makefile], text, Position::new(2, 2), None, &[], None);
        let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, vec!["build", "all"]);
    }

    #[test]
    fn test_no_prerequisite_completions_in_variable_value() {
        let text = "build:\nFOO := b";
        let makefile = Makefile::parse(text).tree();
        let completions = get_completions(&[makefile], text, Position::new(1, 8), None, &[], None);
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
        let completions = get_completions(
            &[makefile],
            text,
            Position::new(0, 5),
            Some(dir.path()),
            &[],
            None,
        );

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
        let completions = get_completions(
            &[makefile],
            text,
            Position::new(0, 9),
            Some(dir.path()),
            &[],
            None,
        );

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
        let completions = get_completions(
            &[makefile],
            text,
            Position::new(0, 5),
            Some(dir.path()),
            &[],
            None,
        );
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
        let completions = get_completions(
            &[makefile],
            text,
            Position::new(0, 6),
            Some(dir.path()),
            &[],
            None,
        );
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
        let completions = get_completions(
            &[makefile],
            text,
            Position::new(0, 8),
            Some(dir.path()),
            &[],
            None,
        );

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
    fn test_include_completions_from_include_dirs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("inc/sub")).unwrap();
        std::fs::write(dir.path().join("src/local.mk"), "").unwrap();
        std::fs::write(dir.path().join("src/both.mk"), "").unwrap();
        std::fs::write(dir.path().join("inc/both.mk"), "").unwrap();
        std::fs::write(dir.path().join("inc/rules.mk"), "").unwrap();
        let text = "include \n";
        let parsed = Makefile::parse(text);
        let completions = get_completions(
            &[parsed.tree()],
            text,
            Position::new(0, 8),
            Some(&dir.path().join("src")),
            &[dir.path().join("inc")],
            None,
        );
        let mut labels: Vec<(&str, Option<CompletionItemKind>)> = completions
            .iter()
            .map(|c| (c.label.as_str(), c.kind))
            .collect();
        labels.sort_by_key(|(label, _)| *label);
        assert_eq!(
            labels,
            vec![
                ("both.mk", Some(CompletionItemKind::FILE)),
                ("local.mk", Some(CompletionItemKind::FILE)),
                ("rules.mk", Some(CompletionItemKind::FILE)),
                ("sub", Some(CompletionItemKind::FOLDER)),
            ]
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
            &[],
            None,
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
            &[],
            None,
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
        let completions = get_completions(
            &[makefile],
            text,
            Position::new(0, 14),
            Some(dir.path()),
            &[],
            None,
        );
        let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
        assert!(labels.contains(&"rules/common.mk"), "got {:?}", labels);
    }

    fn labels(text: &str, pos: Position) -> Vec<String> {
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        get_completions(&[makefile], text, pos, None, &[], None)
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
        let completions = get_completions(&[makefile], text, Position::new(1, 3), None, &[], None);
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

    #[test]
    fn test_partial_word_before_non_ascii_space() {
        // make does not split words on a no-break space.
        assert_eq!(partial_word_before("all: a\u{a0}b", 9), "a\u{a0}b");
    }

    fn labels_in(fx: &crate::workspace::tests::Fixture, pos: Position) -> Vec<String> {
        let set = fx.file_set("Makefile");
        let makefiles: Vec<Makefile> = set.docs().map(|d| d.makefile()).collect();
        let mut labels: Vec<String> =
            get_completions(&makefiles, set.current().text(), pos, None, &[], None)
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

    fn snippets(text: &str, pos: Position, variant: MakefileVariant) -> Vec<(String, String)> {
        let makefile = Makefile::parse(text).tree();
        get_completions(&[makefile], text, pos, None, &[], Some(variant))
            .into_iter()
            .filter(|c| c.kind == Some(CompletionItemKind::SNIPPET))
            .map(|c| {
                assert_eq!(c.insert_text_format, Some(InsertTextFormat::SNIPPET));
                let Some(CompletionTextEdit::Edit(edit)) = c.text_edit else {
                    panic!("snippet {} has no text edit", c.label);
                };
                (c.label, edit.new_text)
            })
            .collect()
    }

    fn snippet_labels(text: &str, pos: Position, variant: MakefileVariant) -> Vec<String> {
        snippets(text, pos, variant)
            .into_iter()
            .map(|(label, _)| label)
            .collect()
    }

    #[test]
    fn test_gnu_snippets_on_empty_line() {
        assert_eq!(
            snippets("all:\n\n", Position::new(1, 0), MakefileVariant::GNUMake),
            [
                ("rule", "${1:target}: ${2:prerequisites}\n\t$0"),
                (
                    "phony rule",
                    ".PHONY: ${1:target}\n${1:target}: ${2:prerequisites}\n\t$0"
                ),
                (
                    "pattern rule",
                    "%.${1:o}: %.${2:c}\n\t${0:\\$(CC) \\$(CPPFLAGS) \\$(CFLAGS) -c -o \\$@ \\$<}"
                ),
                ("ifeq/endif", "ifeq (${1:\\$(VAR)},${2:value})\n$0\nendif"),
                (
                    "ifeq/else/endif",
                    "ifeq (${1:\\$(VAR)},${2:value})\n$3\nelse\n$0\nendif"
                ),
                ("ifdef/endif", "ifdef ${1:VAR}\n$0\nendif"),
                ("ifdef/else/endif", "ifdef ${1:VAR}\n$2\nelse\n$0\nendif"),
                ("ifndef/endif", "ifndef ${1:VAR}\n$0\nendif"),
                ("define/endef", "define ${1:name}\n$0\nendef"),
            ]
            .map(|(l, t)| (l.to_string(), t.to_string()))
        );
    }

    #[test]
    fn test_no_snippets_without_client_support() {
        let makefile = Makefile::parse("all:\n\n").tree();
        let completions = get_completions(
            &[makefile],
            "all:\n\n",
            Position::new(1, 0),
            None,
            &[],
            None,
        );
        assert!(!completions.is_empty());
        assert_eq!(
            completions
                .iter()
                .filter(|c| c.insert_text_format.is_some() || c.text_edit.is_some())
                .count(),
            0
        );
    }

    #[test]
    fn test_snippet_replaces_typed_word() {
        let makefile = Makefile::parse("  ife").tree();
        let item = get_completions(
            &[makefile],
            "  ife",
            Position::new(0, 5),
            None,
            &[],
            Some(MakefileVariant::GNUMake),
        )
        .into_iter()
        .find(|c| c.label == "ifeq/endif")
        .unwrap();
        assert_eq!(item.insert_text_mode, Some(InsertTextMode::AS_IS));
        assert_eq!(
            item.text_edit,
            Some(CompletionTextEdit::Edit(TextEdit::new(
                Range::new(Position::new(0, 2), Position::new(0, 5)),
                "ifeq (${1:\\$(VAR)},${2:value})\n$0\nendif".to_string()
            )))
        );
    }

    #[test]
    fn test_bsd_snippets() {
        assert_eq!(
            snippets(".i", Position::new(0, 2), MakefileVariant::BSDMake),
            [
                ("rule", "${1:target}: ${2:prerequisites}\n\t$0"),
                (
                    "phony rule",
                    ".PHONY: ${1:target}\n${1:target}: ${2:prerequisites}\n\t$0"
                ),
                ("suffix rule", ".${1:c}.${2:o}:\n\t$0"),
                (".if/.endif", ".if ${1:condition}\n$0\n.endif"),
                (
                    ".if/.else/.endif",
                    ".if ${1:condition}\n$2\n.else\n$0\n.endif"
                ),
                (".ifdef/.endif", ".ifdef ${1:VAR}\n$0\n.endif"),
                (".for/.endfor", ".for ${1:item} in ${2:list}\n$0\n.endfor"),
            ]
            .map(|(l, t)| (l.to_string(), t.to_string()))
        );
    }

    #[test]
    fn test_nmake_snippets() {
        assert_eq!(
            snippets(
                "!IFDEF DEBUG\n!ENDIF\n!I",
                Position::new(2, 2),
                MakefileVariant::NMake
            ),
            [
                ("rule", "${1:target}: ${2:prerequisites}\n\t$0"),
                ("inference rule", ".${1:c}.${2:obj}:\n\t$0"),
                ("!IF/!ENDIF", "!IF ${1:condition}\n$0\n!ENDIF"),
                (
                    "!IF/!ELSE/!ENDIF",
                    "!IF ${1:condition}\n$2\n!ELSE\n$0\n!ENDIF"
                ),
                ("!IFDEF/!ENDIF", "!IFDEF ${1:MACRO}\n$0\n!ENDIF"),
            ]
            .map(|(l, t)| (l.to_string(), t.to_string()))
        );
    }

    #[test]
    fn test_posix_snippets() {
        assert_eq!(
            snippet_labels("", Position::new(0, 0), MakefileVariant::POSIXMake),
            vec!["rule", "phony rule", "suffix rule"]
        );
    }

    #[test]
    fn test_snippets_use_recipe_prefix() {
        let text = ".RECIPEPREFIX = >\n\n";
        let rule = snippets(text, Position::new(1, 0), MakefileVariant::GNUMake)
            .into_iter()
            .find(|(label, _)| label == "rule")
            .unwrap();
        assert_eq!(rule.1, "${1:target}: ${2:prerequisites}\n>$0");
    }

    #[test]
    fn test_snippets_recipe_prefix_set_later_ignored() {
        let text = "\n.RECIPEPREFIX = >\n";
        let rule = snippets(text, Position::new(0, 0), MakefileVariant::GNUMake)
            .into_iter()
            .find(|(label, _)| label == "rule")
            .unwrap();
        assert_eq!(rule.1, "${1:target}: ${2:prerequisites}\n\t$0");
    }

    #[test]
    fn test_snippets_recipe_prefix_expansion() {
        let rule = |text: &str| {
            snippets(text, Position::new(2, 0), MakefileVariant::GNUMake)
                .into_iter()
                .find(|(label, _)| label == "rule")
                .unwrap()
                .1
        };
        // := expands the value, = does not.
        assert_eq!(
            rule("X = >\n.RECIPEPREFIX := $(X)\n\n"),
            "${1:target}: ${2:prerequisites}\n>$0"
        );
        assert_eq!(
            rule("X = >\n.RECIPEPREFIX = $(X)\n\n"),
            "${1:target}: ${2:prerequisites}\n\\$$0"
        );
        // .RECIPEPREFIX is always defined, so ?= has no effect.
        assert_eq!(
            rule("X = >\n.RECIPEPREFIX ?= >\n\n"),
            "${1:target}: ${2:prerequisites}\n\t$0"
        );
    }

    #[test]
    fn test_snippets_escape_recipe_prefix() {
        let text = ".RECIPEPREFIX = }\n\n";
        let rule = snippets(text, Position::new(1, 0), MakefileVariant::GNUMake)
            .into_iter()
            .find(|(label, _)| label == "rule")
            .unwrap();
        assert_eq!(rule.1, "${1:target}: ${2:prerequisites}\n\\}$0");
    }

    #[test]
    fn test_no_snippets_mid_line() {
        let gnu = MakefileVariant::GNUMake;
        // Text after the cursor.
        assert_eq!(
            snippet_labels("ife x\n", Position::new(0, 3), gnu),
            Vec::<String>::new()
        );
        // After the first word.
        assert_eq!(
            snippet_labels("FOO ba\n", Position::new(0, 6), gnu),
            Vec::<String>::new()
        );
        // In a rule or a variable value.
        assert_eq!(
            snippet_labels("all: \n", Position::new(0, 5), gnu),
            Vec::<String>::new()
        );
        assert_eq!(
            snippet_labels("X = a\n", Position::new(0, 5), gnu),
            Vec::<String>::new()
        );
        // In a recipe.
        assert_eq!(
            snippet_labels("all:\n\t\n", Position::new(1, 1), gnu),
            Vec::<String>::new()
        );
    }

    fn function_item(text: &str, pos: Position, snippets: bool) -> CompletionItem {
        let makefile = Makefile::parse(text).tree();
        get_completions(
            &[makefile],
            text,
            pos,
            None,
            &[],
            snippets.then_some(MakefileVariant::GNUMake),
        )
        .into_iter()
        .find(|c| c.label == "patsubst")
        .unwrap()
    }

    #[test]
    fn test_function_snippet() {
        let item = function_item("X = $(", Position::new(0, 6), true);
        assert_eq!(item.insert_text_format, Some(InsertTextFormat::SNIPPET));
        assert_eq!(
            item.insert_text.as_deref(),
            Some("patsubst ${1:pattern},${2:replacement},${3:text})$0")
        );
    }

    #[test]
    fn test_function_snippet_before_closing_paren() {
        let item = function_item("X = $()", Position::new(0, 6), true);
        assert_eq!(
            item.insert_text.as_deref(),
            Some("patsubst ${1:pattern},${2:replacement},${3:text}")
        );
    }

    #[test]
    fn test_function_snippet_in_recipe() {
        let item = function_item("all:\n\techo $(", Position::new(1, 8), true);
        assert_eq!(
            item.insert_text.as_deref(),
            Some("patsubst ${1:pattern},${2:replacement},${3:text})$0")
        );
    }

    #[test]
    fn test_function_without_snippet_support() {
        let item = function_item("X = $(", Position::new(0, 6), false);
        assert_eq!(item.insert_text_format, None);
        assert_eq!(item.insert_text.as_deref(), Some("patsubst "));
    }
}
