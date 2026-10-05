//! Document and range formatting for Makefiles.
//!
//! The formatter only makes changes that do not alter what make does with a
//! working Makefile:
//!
//! - recipe lines indented with spaces get a tab, but only when make would
//!   otherwise reject the line ("missing separator");
//! - trailing whitespace is removed, except where it is semantically
//!   meaningful (variable values, `define` bodies, after a backslash, on
//!   continuation lines of non-recipe constructs);
//! - the file ends with exactly one newline.

use std::borrow::Cow;

use makefile_lossless::{Makefile, Parse, SyntaxKind};
use rowan::ast::AstNode;
use text_size::{TextRange, TextSize};
use tower_lsp_server::ls_types::{Range, TextEdit};

use crate::diagnostics::space_indent_range;
use crate::position::text_range_to_lsp_range;

#[derive(Debug, PartialEq, Eq)]
pub enum FormatError {
    /// The file has parse errors other than space-indented recipes; the
    /// parse tree cannot be trusted to tell recipes, variable values and
    /// `define` bodies apart.
    ParseErrors(usize),
}

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FormatError::ParseErrors(n) => {
                write!(f, "not formatting: the Makefile has {n} parse error(s)")
            }
        }
    }
}

impl std::error::Error for FormatError {}

#[derive(Debug)]
struct ByteEdit {
    range: TextRange,
    new_text: String,
}

/// Format the whole document.
pub fn format_document(parsed: &Parse<Makefile>, text: &str) -> Result<Vec<TextEdit>, FormatError> {
    Ok(compute_edits(parsed, text)?
        .into_iter()
        .map(|e| to_lsp_edit(text, e))
        .collect())
}

/// Format the lines covered by `range`.
pub fn format_range(
    parsed: &Parse<Makefile>,
    text: &str,
    range: Range,
) -> Result<Vec<TextEdit>, FormatError> {
    let first_line = range.start.line;
    // A selection ending at column 0 does not include that line.
    let last_line = if range.end.character == 0 && range.end.line > range.start.line {
        range.end.line - 1
    } else {
        range.end.line
    };
    Ok(compute_edits(parsed, text)?
        .into_iter()
        .map(|e| to_lsp_edit(text, e))
        .filter(|e| (first_line..=last_line).contains(&e.range.start.line))
        .collect())
}

fn to_lsp_edit(text: &str, edit: ByteEdit) -> TextEdit {
    TextEdit {
        range: text_range_to_lsp_range(text, edit.range),
        new_text: edit.new_text,
    }
}

fn text_range(start: usize, end: usize) -> TextRange {
    TextRange::new(TextSize::from(start as u32), TextSize::from(end as u32))
}

/// Apply non-overlapping `edits`, sorted by position, to `text`.
fn apply_edits(text: &str, edits: &[ByteEdit]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pos = 0;
    for edit in edits {
        let start: usize = edit.range.start().into();
        out.push_str(&text[pos..start]);
        out.push_str(&edit.new_text);
        pos = edit.range.end().into();
    }
    out.push_str(&text[pos..]);
    out
}

fn compute_edits(parsed: &Parse<Makefile>, text: &str) -> Result<Vec<ByteEdit>, FormatError> {
    let mut edits = tab_indent_edits(parsed, text);

    // Converting the indents must leave a file without parse errors, or
    // the lines were not recipes after all.
    let (converted, reparsed) = if edits.is_empty() {
        (Cow::Borrowed(text), Cow::Borrowed(parsed))
    } else {
        let converted = apply_edits(text, &edits);
        let reparsed = Makefile::parse(&converted);
        (Cow::Owned(converted), Cow::Owned(reparsed))
    };
    if !reparsed.errors().is_empty() {
        return Err(FormatError::ParseErrors(reparsed.errors().len()));
    }
    let makefile = reparsed.tree();
    // Recipe trailing whitespace goes to the shell. Outside of a quoted
    // string (which cannot span a line end without a backslash) the shell
    // ignores it, so trimming it is safe. Under .ONESHELL the recipe is a
    // single script and could contain e.g. here-documents, so leave it alone.
    let oneshell = makefile.rules_by_target(".ONESHELL").next().is_some();

    // The conversion only changes the start of lines, so the trailing
    // whitespace of each line is the same in both texts.
    let tail_start = content_end(text);
    let mut line_start = 0;
    let mut converted_start = 0;
    for (line, converted_line) in text[..tail_start]
        .split_inclusive('\n')
        .zip(converted.split_inclusive('\n'))
    {
        let line_end = line_start + line.trim_end_matches('\n').trim_end_matches('\r').len();
        let ws_start = line_start
            + text[line_start..line_end]
                .trim_end_matches([' ', '\t'])
                .len();
        let converted_ws_start =
            converted_start + (ws_start - line_start) + converted_line.len() - line.len();
        if ws_start < line_end
            && !text[..ws_start].ends_with('\\')
            && may_trim(
                &makefile,
                &converted,
                converted_start,
                converted_ws_start,
                oneshell,
            )
        {
            edits.push(ByteEdit {
                range: text_range(ws_start, line_end),
                new_text: String::new(),
            });
        }
        line_start += line.len();
        converted_start += converted_line.len();
    }

    edits.extend(final_newline_edit(text, tail_start));
    edits.sort_by_key(|e| e.range.start());
    Ok(edits)
}

/// Replace the space indent of recipe lines that make would reject with a
/// tab, keeping the relative indentation of their continuation lines.
///
/// Make strips one leading tab from continuation lines, so the shell sees
/// the relative indentation only. Since only lines make would otherwise
/// reject are converted, this does not change the meaning of a working
/// Makefile.
fn tab_indent_edits(parsed: &Parse<Makefile>, text: &str) -> Vec<ByteEdit> {
    // With a custom .RECIPEPREFIX, a tab does not start a recipe.
    let custom_prefix = parsed
        .tree()
        .variable_definitions()
        .any(|v| v.name().as_deref() == Some(".RECIPEPREFIX"));
    if custom_prefix {
        return Vec::new();
    }

    let mut edits = Vec::new();
    for indent in parsed
        .positioned_errors()
        .iter()
        .filter_map(|error| space_indent_range(text, error))
    {
        let base = &text[indent];
        edits.push(ByteEdit {
            range: indent,
            new_text: "\t".to_string(),
        });
        let mut line_start = usize::from(indent.start());
        while let Some(newline) = text[line_start..].find('\n') {
            let next_start = line_start + newline + 1;
            if !ends_with_continuation(&text[line_start..next_start - 1]) {
                break;
            }
            line_start = next_start;
            let rest = text[line_start..].split('\n').next().unwrap_or_default();
            if rest.starts_with(base) && !rest[base.len()..].trim().is_empty() {
                edits.push(ByteEdit {
                    range: TextRange::at(TextSize::from(line_start as u32), TextSize::of(base)),
                    new_text: "\t".to_string(),
                });
            }
        }
    }
    edits.sort_by_key(|e| e.range.start());
    edits
}

/// Whether the line starting at `line_start` continues the previous line,
/// i.e. the previous line ends in an odd number of backslashes.
fn is_continuation_line(text: &str, line_start: usize) -> bool {
    text[..line_start]
        .strip_suffix('\n')
        .is_some_and(ends_with_continuation)
}

/// Whether `text` ends in an odd number of backslashes (ignoring a
/// trailing carriage return).
fn ends_with_continuation(text: &str) -> bool {
    let text = text.strip_suffix('\r').unwrap_or(text);
    let backslashes = text.len() - text.trim_end_matches('\\').len();
    backslashes % 2 == 1
}

/// Whether the trailing whitespace at `ws_start` can be removed.
fn may_trim(
    makefile: &Makefile,
    text: &str,
    line_start: usize,
    ws_start: usize,
    oneshell: bool,
) -> bool {
    let Some(token) = makefile
        .syntax()
        .token_at_offset(TextSize::from(ws_start as u32))
        .right_biased()
    else {
        return false;
    };
    for node in token.parent_ancestors() {
        match node.kind() {
            SyntaxKind::RECIPE => return !oneshell,
            SyntaxKind::VARIABLE => return false,
            // Target-specific variable assignments (`target: VAR = value `)
            // are parsed as prerequisites; their trailing whitespace is part
            // of the value.
            SyntaxKind::PREREQUISITES
                if node
                    .children_with_tokens()
                    .any(|c| c.kind() == SyntaxKind::OPERATOR) =>
            {
                return false
            }
            _ => {}
        }
    }
    // A continued line outside of a recipe may be part of a variable value.
    !is_continuation_line(text, line_start)
}

/// The end of the last line that has non-whitespace content.
fn content_end(text: &str) -> usize {
    let last = text.trim_end_matches([' ', '\t', '\r', '\n']).len();
    if last == 0 {
        return 0;
    }
    text[last..].find('\n').map_or(text.len(), |i| last + i)
}

/// Replace everything after the last non-blank line with a single newline.
fn final_newline_edit(text: &str, tail_start: usize) -> Option<ByteEdit> {
    if tail_start == 0 {
        // Nothing but whitespace.
        return (!text.is_empty()).then(|| ByteEdit {
            range: text_range(0, text.len()),
            new_text: String::new(),
        });
    }
    // Blank lines after a trailing backslash are part of the continued line.
    if ends_with_continuation(&text[..tail_start]) || &text[tail_start..] == "\n" {
        return None;
    }
    Some(ByteEdit {
        range: text_range(tail_start, text.len()),
        new_text: "\n".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::ls_types::Position;

    fn format(text: &str) -> String {
        let parsed = Makefile::parse(text);
        apply(text, compute_edits(&parsed, text).unwrap())
    }

    fn apply(text: &str, edits: Vec<ByteEdit>) -> String {
        let mut out = String::new();
        let mut pos = 0;
        for edit in edits {
            let start: usize = edit.range.start().into();
            let end: usize = edit.range.end().into();
            assert!(start >= pos, "overlapping edits");
            out.push_str(&text[pos..start]);
            out.push_str(&edit.new_text);
            pos = end;
        }
        out.push_str(&text[pos..]);
        out
    }

    fn assert_formats(input: &str, expected: &str) {
        let formatted = format(input);
        assert_eq!(formatted, expected);
        assert_eq!(format(&formatted), expected, "formatting is not idempotent");
    }

    #[test]
    fn test_clean_file_has_no_edits() {
        let text = "VAR = x\n\nall: foo\n\techo $(VAR)\n";
        let parsed = Makefile::parse(text);
        assert_eq!(format_document(&parsed, text).unwrap(), vec![]);
    }

    #[test]
    fn test_empty_file() {
        assert_formats("", "");
    }

    #[test]
    fn test_whitespace_only_file() {
        assert_formats("\n \n\n", "");
    }

    #[test]
    fn test_space_indented_recipe() {
        assert_formats("all:\n    echo hi\n", "all:\n\techo hi\n");
    }

    #[test]
    fn test_space_indented_recipe_in_conditional() {
        assert_formats(
            "all:\nifdef X\n  echo x\nendif\n",
            "all:\nifdef X\n\techo x\nendif\n",
        );
    }

    #[test]
    fn test_recipe_continuation_keeps_relative_indent() {
        assert_formats(
            "all:\n    echo a \\\n      b \\\n  c\n",
            "all:\n\techo a \\\n\t  b \\\n  c\n",
        );
    }

    #[test]
    fn test_indented_assignment_in_rule_conditional_untouched() {
        let text = "all:\n\techo\nifeq ($(X),y)\n  CFLAGS += -O2  \n  include foo.mk\nendif\n";
        assert_formats(text, text);
    }

    #[test]
    fn test_space_indented_comment_untouched() {
        assert_formats("all:\n\techo\n  # note\n", "all:\n\techo\n  # note\n");
    }

    #[test]
    fn test_space_indented_rule_like_line_untouched() {
        let text = "all:\n    scp a host:b\n";
        assert_formats(text, text);
    }

    #[test]
    fn test_space_indented_function_call_untouched() {
        let text = "all:\n\techo\nifdef X\n  $(info hi)\nendif\n";
        assert_formats(text, text);
    }

    #[test]
    fn test_prerequisite_continuation_untouched() {
        let text = "all: foo \\\n   bar\n\techo\n";
        assert_formats(text, text);
    }

    #[test]
    fn test_recipe_prefix_disables_conversion() {
        let text = ".RECIPEPREFIX = >\nall:\n    echo\n";
        let parsed = Makefile::parse(text);
        assert_eq!(
            format_document(&parsed, text),
            Err(FormatError::ParseErrors(1))
        );
    }

    #[test]
    fn test_recipe_prefix_recipes_trimmed() {
        assert_formats(
            ".RECIPEPREFIX = >\nall:\n>echo  \n",
            ".RECIPEPREFIX = >\nall:\n>echo\n",
        );
    }

    #[test]
    fn test_trims_rule_recipe_and_comment() {
        assert_formats(
            "# c  \nall: foo  \n\techo hi \t\n",
            "# c\nall: foo\n\techo hi\n",
        );
    }

    #[test]
    fn test_keeps_variable_value_whitespace() {
        let text = "VAR = x  \nV2 := y \\\n  z  \n";
        assert_formats(text, text);
    }

    #[test]
    fn test_keeps_target_specific_value_whitespace() {
        let text = "all: VAR = x  \n";
        assert_formats(text, text);
    }

    #[test]
    fn test_keeps_define_body() {
        let text = "define FOO  \n    echo hi  \n\tbar \nendef\n";
        assert_formats(text, text);
    }

    #[test]
    fn test_keeps_whitespace_after_backslash() {
        // Trimming would turn this into a continuation line.
        let text = "# a \\ \nall:\n\techo \\  \n";
        assert_formats(text, text);
    }

    #[test]
    fn test_oneshell_recipes_not_trimmed() {
        let text = ".ONESHELL:\nall:\n\tcat <<EOF\n\ta  \n\tEOF\n";
        assert_formats(text, text);
    }

    #[test]
    fn test_adds_final_newline() {
        assert_formats("all:\n\techo", "all:\n\techo\n");
    }

    #[test]
    fn test_collapses_trailing_blank_lines() {
        assert_formats("all:\n\techo  \n\n\t\n  \n", "all:\n\techo\n");
    }

    #[test]
    fn test_keeps_blank_line_after_trailing_continuation() {
        let text = "VAR = a \\\n\n\n";
        assert_formats(text, text);
    }

    #[test]
    fn test_crlf() {
        assert_formats("all:  \r\n    echo  \r\n\r\n", "all:\r\n\techo\r\n");
    }

    #[test]
    fn test_idempotent_on_mixed_file() {
        let input = concat!(
            "# Build  \n",
            "CC = gcc  \n",
            "all: main.o util.o  \n",
            "    $(CC) -o app \\\n",
            "        main.o util.o   \n",
            "\n",
            "ifdef DEBUG\n",
            "  CFLAGS += -g  \n",
            "endif\n",
            "\n",
            "clean:\n",
            "\trm -f *.o  \n",
            "\n\n\n",
        );
        let expected = concat!(
            "# Build\n",
            "CC = gcc  \n",
            "all: main.o util.o\n",
            "\t$(CC) -o app \\\n",
            "\t    main.o util.o\n",
            "\n",
            "ifdef DEBUG\n",
            "  CFLAGS += -g  \n",
            "endif\n",
            "\n",
            "clean:\n",
            "\trm -f *.o\n",
        );
        assert_formats(input, expected);
    }

    #[test]
    fn test_parse_errors_refused() {
        let text = "foo bar\nall:\n    echo  \n";
        let parsed = Makefile::parse(text);
        assert_eq!(
            format_document(&parsed, text),
            Err(FormatError::ParseErrors(1))
        );
    }

    #[test]
    fn test_space_indented_line_outside_rule_refused() {
        let text = "X = 1\n    echo hi\n";
        let parsed = Makefile::parse(text);
        assert_eq!(
            format_document(&parsed, text),
            Err(FormatError::ParseErrors(1))
        );
    }

    #[test]
    fn test_space_indented_recipes_before_tab_recipe() {
        assert_formats(
            "all:\n    echo a  \n  echo b\n\techo c\n",
            "all:\n\techo a\n\techo b\n\techo c\n",
        );
    }

    #[test]
    fn test_format_document_lsp_edits() {
        let text = "all:  \n    echo\n";
        let parsed = Makefile::parse(text);
        assert_eq!(
            format_document(&parsed, text).unwrap(),
            vec![
                TextEdit {
                    range: Range::new(Position::new(0, 4), Position::new(0, 6)),
                    new_text: String::new(),
                },
                TextEdit {
                    range: Range::new(Position::new(1, 0), Position::new(1, 4)),
                    new_text: "\t".to_string(),
                },
            ]
        );
    }

    #[test]
    fn test_format_range_only_selected_lines() {
        let text = "a:  \n    echo\nb:  \n    echo\n";
        let parsed = Makefile::parse(text);
        let range = Range::new(Position::new(2, 0), Position::new(4, 0));
        assert_eq!(
            format_range(&parsed, text, range).unwrap(),
            vec![
                TextEdit {
                    range: Range::new(Position::new(2, 2), Position::new(2, 4)),
                    new_text: String::new(),
                },
                TextEdit {
                    range: Range::new(Position::new(3, 0), Position::new(3, 4)),
                    new_text: "\t".to_string(),
                },
            ]
        );
    }
}
