//! Hover information for Makefiles.

use makefile_lossless::{Conditional, Makefile, MakefileItem, TextRange, VariableReference};
use text_size::TextSize;
use tower_lsp_server::ls_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position};

use crate::builtins;
use crate::position::try_position_to_offset;
use crate::targets::{prerequisite_at_offset, target_at_offset};
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
    if let Some(comment) = doc_comment(MakefileItem::Rule(rule.clone())) {
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

/// The doc comment of `item`, or `None` if it has none.
fn doc_comment(item: MakefileItem) -> Option<String> {
    let lines: Vec<String> = item.doc_comments().collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
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

/// The directive keyword at `offset`, such as `include`, `else` or `endef`.
fn directive_keyword_at(makefile: &Makefile, offset: TextSize) -> Option<String> {
    let at = |range: Option<TextRange>| range.is_some_and(|r| r.contains(offset));
    if let Some(include) = makefile.includes().find(|i| at(i.keyword_range())) {
        return include.keyword();
    }
    if makefile.vpaths().any(|v| at(v.keyword_range())) {
        return Some("vpath".to_string());
    }
    let load = makefile
        .loads()
        .find(|load| load.keyword_range().is_some_and(|r| r.contains(offset)));
    if let Some(load) = load {
        let keyword = if load.is_optional() { "-load" } else { "load" };
        return Some(keyword.to_string());
    }
    let variable_keyword = makefile
        .variable_definitions()
        .flat_map(|v| v.keyword_ranges())
        .find(|(_, range)| range.contains(offset))
        .map(|(keyword, _)| keyword);
    variable_keyword.or_else(|| {
        makefile
            .all_conditionals()
            .find_map(|cond| conditional_keyword_at(&cond, offset))
    })
}

/// The keyword of `cond` at `offset`: that of a branch, the `else` of an
/// `else ifdef` and the like, or `endif`.
fn conditional_keyword_at(cond: &Conditional, offset: TextSize) -> Option<String> {
    if cond.endif_range().is_some_and(|r| r.contains(offset)) {
        return Some("endif".to_string());
    }
    cond.branches().find_map(|branch| {
        let range = branch.keyword_range().filter(|r| r.contains(offset))?;
        if branch.is_else() {
            return Some("else".to_string());
        }
        let kind = branch.conditional_type()?;
        if branch.index() == 0 {
            return Some(kind);
        }
        // The range covers both words of `else ifdef`.
        let else_range = TextRange::at(range.start(), TextSize::of("else"));
        let kind_range = TextRange::new(range.end() - TextSize::of(kind.as_str()), range.end());
        if else_range.contains(offset) {
            Some("else".to_string())
        } else {
            kind_range.contains(offset).then_some(kind)
        }
    })
}

/// Describe the variable or function referenced by `reference`.
fn variable_hover(files: &FileSet, reference: &VariableReference) -> Option<Hover> {
    let var_name = reference.name()?;
    let function_hover = |f: &builtins::BuiltinFunction| {
        let sig = format!("$({} {})", f.name, f.params.join(","));
        markdown_hover(format!("`{}`: {}", sig, f.doc))
    };
    if reference.is_function_call() {
        return builtins::find_builtin_function(&var_name).map(function_hover);
    }

    if let Some(doc) = builtins::find_automatic_variable(&var_name) {
        return Some(markdown_hover(format!("**`${}`**: {}", var_name, doc)));
    }

    // A function name without arguments, as in `$(wildcard)`
    if let Some(f) = builtins::find_builtin_function(&var_name) {
        return Some(function_hover(f));
    }

    // User-defined variables take precedence over built-in ones
    let definition = files.docs().find_map(|doc| {
        doc.makefile()
            .variable_definitions_by_name(&var_name)
            .next()
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
        if let Some(comment) = doc_comment(MakefileItem::Variable(var_def.clone())) {
            info.push_str(&format!("\n\n{}", comment));
        }
        info.push_str(&origin_note(files, doc));
        return Some(markdown_hover(info));
    }

    let doc = builtins::find_builtin_variable(&var_name)?;
    Some(markdown_hover(format!("**`{}`**: {}", var_name, doc)))
}

/// Get hover information for the symbol at the given position.
///
/// Variables and targets defined in included or including makefiles are
/// described too.
pub fn get_hover(files: &FileSet, position: Position) -> Option<Hover> {
    let source_text = files.current().text();
    let offset = try_position_to_offset(source_text, position)?;
    let byte_offset: usize = offset.into();

    let makefile = files.current().makefile();
    if let Some(reference) = makefile.variable_reference_at(offset) {
        return variable_hover(files, &reference);
    }

    if let Some(d) = directive_keyword_at(&makefile, offset)
        .as_deref()
        .and_then(builtins::find_directive)
    {
        return Some(markdown_hover(format!(
            "```makefile\n{}\n```\n\n{}",
            d.syntax, d.doc
        )));
    }

    // Show rule info for a target, either where it is referenced as a
    // prerequisite or where it is defined.
    let (target, _) = makefile.rules().find_map(|rule| {
        target_at_offset(&rule, byte_offset).or_else(|| prerequisite_at_offset(&rule, byte_offset))
    })?;
    if let Some(doc) = builtins::find_special_target(&target) {
        return Some(markdown_hover(format!("**`{}`**: {}", target, doc)));
    }
    target_hover(files, &target)
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
    fn test_hover_single_char_automatic_variable() {
        assert_eq!(
            hover_text("all:\n\techo $@\n", Position::new(1, 7)).as_deref(),
            Some("**`$@`**: The file name of the target of the rule.")
        );
    }

    #[test]
    fn test_hover_variable_on_dollar() {
        assert_eq!(
            hover_text("CC = gcc\nall:\n\t$(CC)\n", Position::new(2, 1)).as_deref(),
            Some("```makefile\nCC = gcc\n```")
        );
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
    fn test_hover_builtin_function_with_nested_reference() {
        let f = builtins::find_builtin_function("patsubst").unwrap();
        let expected = format!("`$(patsubst {})`: {}", f.params.join(","), f.doc);
        assert_eq!(
            hover_text("OBJS = $(patsubst %.c,%.o,$(SRCS))\n", Position::new(0, 10)),
            Some(expected)
        );
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
    fn test_hover_prerequisite_with_directory() {
        let text = "all: src/foo.o\nsrc/foo.o:\n\tcc\n";
        assert_eq!(
            hover_text(text, Position::new(0, 6)).as_deref(),
            Some("**`src/foo.o`**\n\n```makefile\n\tcc\n```")
        );
    }

    #[test]
    fn test_hover_target_name_in_variable_value() {
        assert_eq!(hover_text("FOO := x\nx:\n", Position::new(0, 7)), None);
        assert_eq!(hover_text("X = .PHONY\n", Position::new(0, 5)), None);
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
    fn test_hover_word_in_target_reference() {
        let text = "$(addprefix $(D)/, foo):\n\nfoo:\n\techo ok\n";
        assert_eq!(
            hover_text(text, Position::new(0, 19)).as_deref(),
            Some("`$(addprefix prefix,names...)`: Prepend *prefix* to each word in *names*.")
        );
        assert_eq!(
            hover_text(text, Position::new(2, 1)).as_deref(),
            Some("**`foo`**\n\n```makefile\n\techo ok\n```")
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
    fn test_hover_target_indented_doc_comment() {
        assert_eq!(
            hover_text("# Build\n  # it\nfoo:\n", Position::new(2, 0)).as_deref(),
            Some("**`foo`**\n\nBuild\nit")
        );
    }

    #[test]
    fn test_hover_target_ignores_continued_comment() {
        assert_eq!(
            hover_text("X = 1 \\\n# value\nfoo:\n", Position::new(2, 0)).as_deref(),
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
    fn test_hover_directive_keywords() {
        let text = "ifdef A\nelse ifndef B\nload a.so\nendif\ndefine X\nendef\nunexport Y\n";
        assert_eq!(
            hover_text(text, Position::new(1, 1)),
            directive_hover("else")
        );
        assert_eq!(hover_text(text, Position::new(1, 4)), None);
        assert_eq!(
            hover_text(text, Position::new(2, 1)),
            directive_hover("load")
        );
        assert_eq!(
            hover_text(text, Position::new(3, 1)),
            directive_hover("endif")
        );
        assert_eq!(
            hover_text(text, Position::new(5, 1)),
            directive_hover("endef")
        );
        assert_eq!(
            hover_text(text, Position::new(6, 1)),
            directive_hover("unexport")
        );
        let text = "ifdef A\nifdef B\n-load b.so\nendif\nendif\n";
        assert_eq!(
            hover_text(text, Position::new(2, 1)),
            directive_hover("-load")
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
