//! Hover information for Makefiles.

use makefile_lossless::{is_in_prerequisites, variable_at_offset, word_at_offset, Makefile};
use tower_lsp_server::ls_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position};

use crate::builtins;
use crate::position::try_position_to_offset;

fn markdown_hover(text: String) -> Hover {
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: text,
        }),
        range: None,
    }
}

/// Get hover information for the symbol at the given position.
pub fn get_hover(
    makefile: &Makefile,
    source_text: &str,
    position: Position,
) -> Option<Hover> {
    let offset = try_position_to_offset(source_text, position)?;
    let byte_offset: usize = offset.into();

    // Variable reference: $(VAR) or ${VAR}
    if let Some(var_name) = variable_at_offset(source_text, byte_offset) {
        // Check automatic variables
        if let Some((_, doc)) = builtins::AUTOMATIC_VARIABLES.iter().find(|(n, _)| *n == var_name)
        {
            return Some(markdown_hover(format!("**`${}`** — {}", var_name, doc)));
        }

        // Check built-in functions (var_name may be "wildcard *.c", so match the first word)
        let func_name = var_name.split_whitespace().next().unwrap_or(var_name);
        if let Some(f) = builtins::find_builtin_function(func_name) {
            let sig = format!("$({} {})", f.name, f.params.join(","));
            return Some(markdown_hover(format!("`{}` — {}", sig, f.doc)));
        }

        // Check user-defined variables (prioritize over built-ins)
        if let Some(var_def) = makefile
            .variable_definitions()
            .find(|v| v.name().as_deref() == Some(var_name))
        {
            let op = var_def.assignment_operator().unwrap_or_else(|| "=".to_string());
            let value = var_def
                .raw_value()
                .map(|v| v.trim().to_string())
                .unwrap_or_default();
            return Some(markdown_hover(format!(
                "```makefile\n{} {} {}\n```",
                var_name, op, value
            )));
        }

        // Check built-in variables
        if let Some(doc) = builtins::find_builtin_variable(var_name) {
            return Some(markdown_hover(format!("**`{}`** — {}", var_name, doc)));
        }

        return None;
    }

    // Word in prerequisites area or at start of line (target name)
    if let Some(word) = word_at_offset(source_text, byte_offset) {
        // Check special targets
        if let Some((_, doc)) = builtins::SPECIAL_TARGETS.iter().find(|(n, _)| *n == word) {
            return Some(markdown_hover(format!("**`{}`** — {}", word, doc)));
        }

        // Show rule info if hovering over a target reference in prerequisites
        if is_in_prerequisites(source_text, byte_offset) {
            if let Some(rule) = makefile.rules().find(|r| r.targets().any(|t| t == word)) {
                let prereqs: Vec<String> = rule.prerequisites().collect();
                let recipes: Vec<String> = rule.recipes().collect();
                let mut info = format!("**`{}`**", word);
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
                return Some(markdown_hover(info));
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hover_text(text: &str, pos: Position) -> Option<String> {
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        get_hover(&makefile, text, pos).map(|h| match h.contents {
            HoverContents::Markup(m) => m.value,
            _ => panic!("Expected markup content"),
        })
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
        // @ is col 7, but $(@D) — let's test with a simple $(VAR) context
        // Actually, automatic vars like $@ are single-char after $, not inside $()
        // But $(@D) is inside $(). Offset for @D: "\techo $(@D)\n"
        // line 1: "\techo $(@D)", $ at col 6, ( at col 7, @ at col 8
        let result = hover_text(text, Position::new(1, 8));
        assert!(result.is_some());
        assert!(result.unwrap().contains("directory"));
    }

    #[test]
    fn test_hover_builtin_function() {
        let text = "FILES = $(wildcard *.c)\n";
        // "$(wildcard" — w at col 10
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
    fn test_hover_undefined_variable() {
        let text = "all:\n\t$(UNDEFINED)\n";
        let result = hover_text(text, Position::new(1, 3));
        assert!(result.is_none());
    }
}
