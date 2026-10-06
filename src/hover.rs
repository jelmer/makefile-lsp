//! Hover information for Makefiles.

use makefile_lossless::{
    is_in_prerequisites, variable_at_offset, word_at_offset, Lang, Makefile, SyntaxKind,
};
use rowan::ast::AstNode;
use rowan::SyntaxNode;
use text_size::TextSize;
use tower_lsp_server::ls_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position};

use crate::builtins;
use crate::position::try_position_to_offset;
use crate::workspace::{Document, FileSet};

fn markdown_hover(text: String) -> Hover {
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: text,
        }),
        range: None,
    }
}

/// Check whether `offset` lies within the target list of a rule head.
fn in_rule_targets(makefile: &Makefile, offset: TextSize) -> bool {
    makefile
        .syntax()
        .token_at_offset(offset)
        .any(|t| t.parent().is_some_and(|p| p.kind() == SyntaxKind::TARGETS))
}

/// Describe the first rule defining `target`: its doc comment, prerequisites
/// and recipe.
fn target_hover(files: &FileSet, target: &str) -> Option<Hover> {
    let (doc, rule) = files.docs().find_map(|doc| {
        doc.makefile()
            .rules()
            .find(|r| r.targets().any(|t| t == target))
            .map(|r| (doc, r))
    })?;
    let prereqs: Vec<String> = rule.prerequisites().collect();
    let recipes: Vec<String> = rule.recipes().collect();
    let mut info = format!("**`{}`**", target);
    if let Some(comment) = doc_comment(rule.syntax()) {
        info.push_str(&format!("\n\n{}", comment));
    }
    if !prereqs.is_empty() {
        info.push_str(&format!("\n\nPrerequisites: `{}`", prereqs.join(" ")));
    }
    if !recipes.is_empty() {
        info.push_str("\n\n```makefile");
        for r in &recipes {
            info.push_str(&format!("\n\t{}", r));
        }
        info.push_str("\n```");
    }
    info.push_str(&origin_note(files, doc));
    Some(markdown_hover(info))
}

/// Collect the `#` comment lines directly above `node`, stopping at a blank
/// line or any other content. Returns `None` if there are none.
fn doc_comment(node: &SyntaxNode<Lang>) -> Option<String> {
    let mut lines = Vec::new();
    let mut token = node.first_token()?.prev_token();
    while let Some(newline) = token.filter(|t| t.kind() == SyntaxKind::NEWLINE) {
        let Some(comment) = newline
            .prev_token()
            .filter(|t| t.kind() == SyntaxKind::COMMENT && !t.text().starts_with("#!"))
        else {
            break;
        };
        // Only whole-line comments count, not trailing ones like `FOO = 1 # x`.
        let before = comment.prev_token();
        if before
            .as_ref()
            .is_some_and(|t| t.kind() != SyntaxKind::NEWLINE)
        {
            break;
        }
        let text = comment.text().trim_start_matches('#');
        lines.push(
            text.strip_prefix(' ')
                .unwrap_or(text)
                .trim_end()
                .to_string(),
        );
        token = before;
    }
    if lines.is_empty() {
        return None;
    }
    lines.reverse();
    Some(lines.join("\n"))
}

/// Note where a definition comes from, when it's not the current document.
fn origin_note(files: &FileSet, doc: &Document) -> String {
    if std::ptr::eq(doc, files.current()) {
        return String::new();
    }
    let name = match (doc.path(), files.current().dir()) {
        (Some(path), Some(dir)) => path.strip_prefix(dir).unwrap_or(path).display().to_string(),
        (Some(path), None) => path.display().to_string(),
        (None, _) => doc.uri().as_str().to_string(),
    };
    format!("\n\nDefined in `{}`", name)
}

/// Return the directive named `word` (found at `byte_offset`) if it is used as a
/// directive: at the start of a non-recipe line, optionally after modifiers
/// such as `override` or `else`, and not itself a target or variable name.
fn directive_at(
    source_text: &str,
    byte_offset: usize,
    word: &str,
) -> Option<&'static builtins::Directive> {
    let directive = builtins::find_directive(word)?;
    let is_word_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-');
    let word_start = source_text[..byte_offset]
        .rfind(|c: char| !is_word_char(c))
        .map_or(0, |i| i + 1);
    let word_end = word_start + word.len();
    let line_start = source_text[..word_start].rfind('\n').map_or(0, |i| i + 1);
    let line_end = source_text[word_end..]
        .find('\n')
        .map_or(source_text.len(), |i| word_end + i);

    if source_text[line_start..].starts_with('\t') {
        return None;
    }
    let modifiers_only = source_text[line_start..word_start]
        .split_whitespace()
        .all(|w| matches!(w, "else" | "override" | "export" | "private"));
    let after = source_text[word_end..line_end].trim_start();
    let is_definition =
        after.starts_with([':', '=']) || ["+=", "?=", "!="].iter().any(|op| after.starts_with(op));
    (modifiers_only && !is_definition).then_some(directive)
}

/// Get hover information for the symbol at the given position.
///
/// Variables and targets defined in included or including makefiles are
/// described too.
pub fn get_hover(files: &FileSet, position: Position) -> Option<Hover> {
    let source_text = files.current().text();
    let offset = try_position_to_offset(source_text, position)?;
    let byte_offset: usize = offset.into();

    // Variable reference: $(VAR) or ${VAR}
    if let Some(var_name) = variable_at_offset(source_text, byte_offset) {
        // Check automatic variables
        if let Some(doc) = builtins::find_automatic_variable(var_name) {
            return Some(markdown_hover(format!("**`${}`**: {}", var_name, doc)));
        }

        // Check built-in functions (var_name may be "wildcard *.c", so match the first word)
        let func_name = var_name.split_whitespace().next().unwrap_or(var_name);
        if let Some(f) = builtins::find_builtin_function(func_name) {
            let sig = format!("$({} {})", f.name, f.params.join(","));
            return Some(markdown_hover(format!("`{}`: {}", sig, f.doc)));
        }

        // Check user-defined variables (prioritize over built-ins)
        let definition = files.docs().find_map(|doc| {
            doc.makefile()
                .variable_definitions()
                .find(|v| v.name().as_deref() == Some(var_name))
                .map(|v| (doc, v))
        });
        if let Some((doc, var_def)) = definition {
            let op = var_def
                .assignment_operator()
                .unwrap_or_else(|| "=".to_string());
            let value = var_def
                .raw_value()
                .map(|v| v.trim().to_string())
                .unwrap_or_default();
            let mut info = format!("```makefile\n{} {} {}\n```", var_name, op, value);
            if let Some(comment) = doc_comment(var_def.syntax()) {
                info.push_str(&format!("\n\n{}", comment));
            }
            info.push_str(&origin_note(files, doc));
            return Some(markdown_hover(info));
        }

        // Check built-in variables
        if let Some(doc) = builtins::find_builtin_variable(var_name) {
            return Some(markdown_hover(format!("**`{}`**: {}", var_name, doc)));
        }

        return None;
    }

    // Word in prerequisites area or at start of line (target name)
    if let Some(word) = word_at_offset(source_text, byte_offset) {
        if let Some(d) = directive_at(source_text, byte_offset, word) {
            return Some(markdown_hover(format!(
                "```makefile\n{}\n```\n\n{}",
                d.syntax, d.doc
            )));
        }

        // Check special targets
        if let Some(doc) = builtins::find_special_target(word) {
            return Some(markdown_hover(format!("**`{}`**: {}", word, doc)));
        }

        // Show rule info for a target, either where it is referenced as a
        // prerequisite or where it is defined.
        if is_in_prerequisites(source_text, byte_offset)
            || in_rule_targets(&files.current().makefile(), offset)
        {
            if let Some(hover) = target_hover(files, word) {
                return Some(hover);
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::workspace::tests::Fixture;

    fn markup(hover: Option<Hover>) -> Option<String> {
        hover.map(|h| match h.contents {
            HoverContents::Markup(m) => m.value,
            _ => panic!("Expected markup content"),
        })
    }

    fn hover_text(text: &str, pos: Position) -> Option<String> {
        let uri = "file:///test/Makefile".parse().unwrap();
        let files = FileSet::single(Document::new(uri, text.to_string()));
        markup(get_hover(&files, pos))
    }

    #[test]
    fn test_hover_user_variable() {
        let text = "CC = gcc\nall:\n\t$(CC) main.c\n";
        let result = hover_text(text, Position::new(2, 3));
        assert!(result.is_some());
        let content = result.unwrap();
        assert!(content.contains("CC"));
        assert!(content.contains("gcc"));
    }

    #[test]
    fn test_hover_automatic_variable() {
        let text = "all:\n\techo $(@D)\n";
        // $ at col 6, ( at col 7, @ at col 8
        let result = hover_text(text, Position::new(1, 8));
        assert!(result.is_some());
        assert!(result.unwrap().contains("directory"));
    }

    #[test]
    fn test_hover_automatic_variable_variant() {
        let text = "all:\n\techo $(^F)\n";
        // $ at col 6, ( at col 7, ^ at col 8
        let result = hover_text(text, Position::new(1, 8));
        assert!(result.is_some());
        let content = result.unwrap();
        assert!(content.contains("file-within-directory part"));
        assert!(content.contains("$^"));
    }

    #[test]
    fn test_hover_builtin_function() {
        let text = "FILES = $(wildcard *.c)\n";
        // 'w' of "wildcard" at col 10
        let result = hover_text(text, Position::new(0, 10));
        assert!(result.is_some());
        assert!(result.unwrap().contains("wildcard"));
    }

    #[test]
    fn test_hover_special_target() {
        let text = "all: build\n.PHONY: all\n";
        // ".PHONY" on line 1, col 0
        let result = hover_text(text, Position::new(1, 0));
        assert!(result.is_some());
        assert!(result.unwrap().contains("do not represent files"));
    }

    #[test]
    fn test_hover_notparallel_special_target() {
        let text = "all:\n\techo ok\n.NOTPARALLEL:\n";
        // ".NOTPARALLEL" on line 2, col 0
        let result = hover_text(text, Position::new(2, 0));
        assert!(result.is_some());
        assert!(result.unwrap().contains("parallel execution"));
    }

    #[test]
    fn test_hover_prerequisite_target() {
        let text = "all: build\n\nbuild:\n\techo ok\n";
        // "build" in prerequisites, col 5
        let result = hover_text(text, Position::new(0, 5));
        assert!(result.is_some());
        let content = result.unwrap();
        assert!(content.contains("build"));
        assert!(content.contains("echo ok"));
    }

    #[test]
    fn test_hover_target_definition() {
        let text = "all: build\n\nbuild:\n\techo ok\n";
        assert_eq!(
            hover_text(text, Position::new(2, 1)).as_deref(),
            Some("**`build`**\n\n```makefile\n\techo ok\n```")
        );
        assert_eq!(
            hover_text(text, Position::new(0, 1)).as_deref(),
            Some("**`all`**\n\nPrerequisites: `build`")
        );
    }

    #[test]
    fn test_hover_target_doc_comment() {
        let text = "# Build the thing.\n# Twice.\nall: dep\n\techo hi\n\nother: all\n";
        let expected =
            "**`all`**\n\nBuild the thing.\nTwice.\n\nPrerequisites: `dep`\n\n```makefile\n\techo hi\n```";
        assert_eq!(
            hover_text(text, Position::new(2, 0)).as_deref(),
            Some(expected)
        );
        assert_eq!(
            hover_text(text, Position::new(5, 8)).as_deref(),
            Some(expected)
        );
    }

    #[test]
    fn test_hover_target_doc_comment_after_recipe() {
        let text = "all:\n\techo hi\n# About foo\nfoo:\n";
        assert_eq!(
            hover_text(text, Position::new(3, 0)).as_deref(),
            Some("**`foo`**\n\nAbout foo")
        );
    }

    #[test]
    fn test_hover_target_doc_comment_markers() {
        assert_eq!(
            hover_text("## Run tests\ntest:\n", Position::new(1, 0)).as_deref(),
            Some("**`test`**\n\nRun tests")
        );
        assert_eq!(
            hover_text("#!/usr/bin/make -f\nall:\n", Position::new(1, 0)).as_deref(),
            Some("**`all`**")
        );
    }

    #[test]
    fn test_hover_target_ignores_detached_comments() {
        assert_eq!(
            hover_text("# Unrelated\n\nfoo:\n", Position::new(2, 0)).as_deref(),
            Some("**`foo`**")
        );
        assert_eq!(
            hover_text("X = 1 # trailing\nfoo:\n", Position::new(1, 0)).as_deref(),
            Some("**`foo`**")
        );
    }

    #[test]
    fn test_hover_variable_doc_comment() {
        let text = "# The C compiler\nCC = gcc\nall:\n\t$(CC) x.c\n";
        assert_eq!(
            hover_text(text, Position::new(3, 3)).as_deref(),
            Some("```makefile\nCC = gcc\n```\n\nThe C compiler")
        );
    }

    #[test]
    fn test_hover_nothing() {
        let text = "all:\n\techo hello\n";
        // On whitespace before recipe
        let result = hover_text(text, Position::new(0, 3));
        assert!(result.is_none());
    }

    #[test]
    fn test_hover_builtin_variable() {
        let text = "all:\n\t$(MAKE) -C subdir\n";
        let result = hover_text(text, Position::new(1, 3));
        assert!(result.is_some());
        let content = result.unwrap();
        assert!(content.contains("MAKE"));
        assert!(content.contains("make program"));
    }

    #[test]
    fn test_hover_builtin_cc_variable() {
        let text = "all:\n\t$(CC) -o main main.c\n";
        let result = hover_text(text, Position::new(1, 3));
        assert!(result.is_some());
        let content = result.unwrap();
        assert!(content.contains("CC"));
        assert!(content.contains("C compiler"));
    }

    #[test]
    fn test_hover_directive() {
        let text = "ifeq ($(CC),gcc)\nFOO = 1\nendif\n";
        assert_eq!(
            hover_text(text, Position::new(0, 1)).as_deref(),
            Some(
                "```makefile\nifeq (ARG1,ARG2)\n```\n\n\
                 Process the following lines if *ARG1* and *ARG2* are equal after expansion."
            )
        );
        assert_eq!(
            hover_text(text, Position::new(2, 2)).as_deref(),
            Some("```makefile\nendif\n```\n\nEnd a conditional.")
        );
    }

    fn directive_hover(name: &str) -> Option<String> {
        let d = builtins::find_directive(name).unwrap();
        Some(format!("```makefile\n{}\n```\n\n{}", d.syntax, d.doc))
    }

    #[test]
    fn test_hover_include_directives() {
        let text = "include a.mk\n-include b.mk\nvpath %.c src\n";
        assert_eq!(
            hover_text(text, Position::new(0, 3)),
            directive_hover("include")
        );
        assert_eq!(
            hover_text(text, Position::new(1, 0)),
            directive_hover("-include")
        );
        assert_eq!(
            hover_text(text, Position::new(2, 2)),
            directive_hover("vpath")
        );
    }

    #[test]
    fn test_hover_directive_after_modifier() {
        let text = "ifdef A\nelse ifndef B\nendif\noverride define X\nendef\n";
        assert_eq!(
            hover_text(text, Position::new(1, 6)),
            directive_hover("ifndef")
        );
        assert_eq!(
            hover_text(text, Position::new(3, 10)),
            directive_hover("define")
        );
    }

    #[test]
    fn test_hover_directive_name_used_as_target_or_variable() {
        assert_eq!(
            hover_text("include: foo\n", Position::new(0, 1)),
            Some("**`include`**\n\nPrerequisites: `foo`".to_string())
        );
        assert_eq!(hover_text("export = 1\n", Position::new(0, 1)), None);
        assert_eq!(
            hover_text("all:\n\techo include\n", Position::new(1, 7)),
            None
        );
        assert_eq!(hover_text("FOO = include\n", Position::new(0, 8)), None);
    }

    #[test]
    fn test_hover_undefined_variable() {
        let text = "all:\n\t$(UNDEFINED)\n";
        let result = hover_text(text, Position::new(1, 3));
        assert!(result.is_none());
    }

    #[test]
    fn test_hover_variable_from_included_file() {
        let fx = Fixture::new(&[
            ("Makefile", "include mk/rules.mk\nall:\n\t$(CC) x\n"),
            ("mk/rules.mk", "CC := gcc\n"),
        ]);
        assert_eq!(
            markup(get_hover(&fx.file_set("Makefile"), Position::new(2, 3))),
            Some("```makefile\nCC := gcc\n```\n\nDefined in `mk/rules.mk`".to_string())
        );
    }

    #[test]
    fn test_hover_target_from_included_file() {
        let fx = Fixture::new(&[
            ("Makefile", "include rules.mk\nall: build\n"),
            ("rules.mk", "build: gen\n\techo ok\n"),
        ]);
        assert_eq!(
            markup(get_hover(&fx.file_set("Makefile"), Position::new(1, 6))),
            Some(
                "**`build`**\n\nPrerequisites: `gen`\n\n```makefile\n\techo ok\n```\n\nDefined in `rules.mk`"
                    .to_string()
            )
        );
    }
}
