//! Check recipe lines for shell syntax errors by running them through
//! `sh -n`.
//!
//! This spawns a process per recipe line, so it is only run when a document
//! is opened or saved, not on every change.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};

use makefile_lossless::{Makefile, Recipe, SyntaxKind};
use rowan::ast::AstNode;
use tower_lsp_server::ls_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range};

use crate::position::offset_to_position;

/// Word substituted for make variable and function references, so the shell
/// sees a plain word in their place.
const PLACEHOLDER: &str = "__make_ref__";

/// Check the recipe lines of `makefile` for shell syntax errors.
///
/// The shell is `/bin/sh` unless `SHELL` is set to a known Bourne-style
/// shell. Each logical recipe line (continuations included) is checked on
/// its own, as make runs each in a separate shell, after rewriting it as
/// described for `shell_script`.
///
/// Skipped entirely when `SHELL` can't be determined statically or isn't a
/// Bourne-style shell, and with `.ONESHELL`.
///
/// Reported as warnings rather than errors, as a make reference that
/// expands to shell syntax (say, `then` or `;`) can make a valid line look
/// broken.
pub fn check_shell_syntax(source_text: &str, makefile: &Makefile) -> Vec<Diagnostic> {
    // TODO: with .ONESHELL, check each recipe as a single script.
    if makefile.rules_by_target(".ONESHELL").next().is_some() {
        return Vec::new();
    }
    let Some(shell) = shell_program(makefile) else {
        return Vec::new();
    };

    let mut cache: HashMap<String, Option<(usize, String)>> = HashMap::new();
    let mut diagnostics = Vec::new();
    for rule in makefile.rules() {
        for recipe in rule.recipe_nodes() {
            let script = shell_script(&recipe.shell_text());
            if script.trim().is_empty() {
                continue;
            }
            let result = match cache.get(&script) {
                Some(result) => result.clone(),
                None => {
                    let Some(result) = run_syntax_check(&shell, &script) else {
                        // The shell couldn't be run at all; already logged.
                        return Vec::new();
                    };
                    cache.insert(script.clone(), result.clone());
                    result
                }
            };
            let Some((line, message)) = result else {
                continue;
            };
            diagnostics.push(Diagnostic {
                range: recipe_line_range(source_text, &recipe, line),
                severity: Some(DiagnosticSeverity::WARNING),
                code: Some(NumberOrString::String("invalid-shell-syntax".to_string())),
                source: Some("makefile-lsp".to_string()),
                message: format!("shell syntax error: {}", message),
                ..Default::default()
            });
        }
    }
    diagnostics
}

/// The shell make would run recipes with, or `None` if it can't be
/// determined or isn't one we can check with `-n`.
fn shell_program(makefile: &Makefile) -> Option<String> {
    let mut values = HashSet::new();
    for def in makefile.variable_definitions_by_name("SHELL") {
        let target_specific = def
            .syntax()
            .ancestors()
            .any(|a| a.kind() == SyntaxKind::RULE);
        if target_specific {
            return None;
        }
        values.insert(def.raw_value()?.trim().to_string());
    }
    let value = match values.len() {
        0 => return Some("/bin/sh".to_string()),
        1 => values.into_iter().next()?,
        _ => return None,
    };
    if value.contains('$') {
        return None;
    }

    let mut words = value.split_whitespace();
    let mut program = words.next()?;
    if Path::new(program).file_name()? == "env" {
        program = words.next()?;
    }
    if words.next().is_some() {
        return None;
    }
    let name = Path::new(program).file_name()?.to_str()?;
    matches!(name, "sh" | "bash" | "dash" | "ksh" | "mksh" | "zsh").then(|| program.to_string())
}

/// Turn the text make passes to the shell for a recipe line into something
/// `sh -n` can check, keeping the line structure intact.
///
/// The `@`, `-` and `+` prefixes are stripped and `$$` becomes `$`.
/// A make reference at the start of a command is often a prefix like
/// `$(Q)` or `$(QUIET_CC)` glued to the command, or expands to whole
/// commands, so unless an operator follows it becomes the separate command
/// `:;`. Elsewhere it becomes a placeholder word.
fn shell_script(shell_text: &str) -> String {
    let mut rest =
        shell_text.trim_start_matches(|c: char| matches!(c, '@' | '-' | '+') || c.is_whitespace());
    let mut out = String::with_capacity(rest.len());
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        if after.is_empty() {
            out.push('$');
            rest = after;
            break;
        }
        if let Some(tail) = after.strip_prefix('$') {
            out.push('$');
            rest = tail;
            continue;
        }
        let (reference, tail) = after.split_at(reference_length(after));
        let operator_follows = skip_blanks_forward(tail)
            .chars()
            .next()
            .is_none_or(|c| matches!(c, '|' | '&' | ';' | '<' | '>' | ')'));
        if operator_follows || !in_command_position(&out) {
            out.push_str(PLACEHOLDER);
        } else {
            out.push_str(":;");
        }
        // Keep line numbers stable for references split over lines.
        for _ in reference.matches('\n') {
            out.push_str("\\\n");
        }
        rest = tail;
    }
    out.push_str(rest);
    out
}

/// The length of the make reference at the start of `text`, which follows
/// a `$`.
fn reference_length(text: &str) -> usize {
    let mut chars = text.char_indices();
    let Some((_, open)) = chars.next() else {
        return 0;
    };
    let close = match open {
        '(' => ')',
        '{' => '}',
        _ => return open.len_utf8(),
    };
    // make only counts the bracket type the reference was opened with.
    let mut depth = 1;
    for (i, c) in chars {
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                return i + 1;
            }
        }
    }
    text.len()
}

/// Strip leading blanks and line continuations.
fn skip_blanks_forward(mut text: &str) -> &str {
    loop {
        text = text.trim_start_matches([' ', '\t']);
        match text.strip_prefix("\\\n") {
            Some(rest) => text = rest,
            None => return text,
        }
    }
}

/// Does the shell script `before` end where a new command starts?
fn in_command_position(mut before: &str) -> bool {
    loop {
        before = before.trim_end_matches([' ', '\t']);
        match before.strip_suffix("\\\n") {
            Some(rest) => before = rest,
            None => break,
        }
    }
    before
        .chars()
        .next_back()
        .is_none_or(|c| matches!(c, ';' | '&' | '|' | '(' | '{' | '\n'))
}

/// Run `shell -n` on `script`.
///
/// Returns `Some(None)` if the script is valid, `Some(Some((line, message)))`
/// with the zero-based line of the first error, or `None` if the shell
/// couldn't be run.
fn run_syntax_check(shell: &str, script: &str) -> Option<Option<(usize, String)>> {
    let mut child = match Command::new(shell)
        .arg("-n")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            warn_once(shell, &e);
            return None;
        }
    };
    let mut stdin = child.stdin.take()?;
    let written = stdin.write_all(script.as_bytes());
    drop(stdin);
    let output = match child.wait_with_output() {
        Ok(output) => output,
        Err(e) => {
            tracing::warn!("failed to run {} -n: {}", shell, e);
            return None;
        }
    };
    if output.status.success() {
        return Some(None);
    }
    // The shell may stop reading at the first error.
    if let Err(e) = written {
        if e.kind() != std::io::ErrorKind::BrokenPipe {
            tracing::warn!("failed to pass recipe to {} -n: {}", shell, e);
            return None;
        }
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Some(Some(parse_shell_error(&stderr)))
}

/// Log that `shell` can't be run, once per shell.
fn warn_once(shell: &str, error: &std::io::Error) {
    static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let mut warned = WARNED
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if warned.insert(shell.to_string()) {
        tracing::warn!(
            "unable to run {} to check recipe syntax, skipping: {}",
            shell,
            error
        );
    }
}

/// Extract the zero-based line number and message from a shell's `-n`
/// error output.
///
/// dash prints `sh: 3: Syntax error: ...` and bash `bash: line 3: syntax
/// error ...`. If no line number can be found, the whole first line is used
/// as the message for line 0.
fn parse_shell_error(stderr: &str) -> (usize, String) {
    let first = stderr.lines().next().unwrap_or("").trim();
    let parts: Vec<&str> = first.split(": ").collect();
    for (i, part) in parts.iter().enumerate() {
        let number = part.strip_prefix("line ").unwrap_or(part);
        if let Ok(line) = number.parse::<usize>() {
            return (line.saturating_sub(1), parts[i + 1..].join(": "));
        }
    }
    (0, first.to_string())
}

/// The range of the `line`th physical line of `recipe`, clamped to its last
/// line, excluding the leading tab and line ending.
fn recipe_line_range(source_text: &str, recipe: &Recipe, line: usize) -> Range {
    let range = recipe.text_range();
    let recipe_text = &source_text[range];
    let lines: Vec<&str> = recipe_text
        .trim_end_matches(['\n', '\r'])
        .split('\n')
        .collect();
    let index = line.min(lines.len() - 1);
    let start = offset_to_position(source_text, range.start());
    let line_text = lines[index].trim_end_matches('\r');
    let first_column = if index == 0 { start.character } else { 0 };
    let indent = line_text.len() - line_text.trim_start_matches('\t').len();
    let line_number = start.line + index as u32;
    let width: u32 = line_text[indent..].encode_utf16().count() as u32;
    let begin = first_column + line_text[..indent].len() as u32;
    Range::new(
        Position::new(line_number, begin),
        Position::new(line_number, begin + width),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(text: &str) -> Vec<(Range, String)> {
        let parsed = Makefile::parse(text);
        check_shell_syntax(text, &parsed.tree())
            .into_iter()
            .map(|d| (d.range, d.message))
            .collect()
    }

    #[test]
    fn test_shell_script_substitutes_references() {
        assert_eq!(
            shell_script("@-$(CC) -o $@ ${SRCS} $$HOME $(call f,$(x),(y)) x$(Y)z"),
            ":; -o __make_ref__ __make_ref__ $HOME __make_ref__ x__make_ref__z"
        );
    }

    #[test]
    fn test_shell_script_keeps_lines() {
        assert_eq!(
            shell_script("echo $(foo \\\n\tbar) \\\n\tbaz $"),
            "echo __make_ref__\\\n \\\n\tbaz $"
        );
    }

    #[test]
    fn test_shell_script_command_position() {
        assert_eq!(
            shell_script("$(Q)for f in $^; do $(Q)echo $$f; done"),
            ":;for f in __make_ref__; do __make_ref__echo $f; done"
        );
        assert_eq!(
            shell_script("$(QUIET) \\\n\t($(foreach x,y,z &&) true) && $(RM) $@"),
            ":; \\\n\t(:; true) && :; __make_ref__"
        );
        assert_eq!(
            shell_script("$(CMD) | grep x; $(CMD)"),
            "__make_ref__ | grep x; __make_ref__"
        );
        assert_eq!(
            shell_script("{ $(foreach m,$(M),echo $(m);) } > out"),
            "{ :; } > out"
        );
    }

    #[test]
    fn test_parse_dash_error() {
        assert_eq!(
            parse_shell_error(
                "/bin/sh: 3: Syntax error: end of file unexpected (expecting \"fi\")\n"
            ),
            (
                2,
                "Syntax error: end of file unexpected (expecting \"fi\")".to_string()
            )
        );
    }

    #[test]
    fn test_parse_bash_error() {
        assert_eq!(
            parse_shell_error(
                "bash: line 1: syntax error near unexpected token `)'\nbash: line 1: `ls )'\n"
            ),
            (0, "syntax error near unexpected token `)'".to_string())
        );
    }

    #[test]
    fn test_parse_unknown_error() {
        assert_eq!(
            parse_shell_error("something odd\n"),
            (0, "something odd".to_string())
        );
    }

    #[test]
    fn test_valid_recipes() {
        let text = concat!(
            "all: foo\n",
            "\t@echo \"building $@\"\n",
            "\t-rm -f $(OBJS)\n",
            "\tif [ -n \"$(V)\" ]; then \\\n",
            "\t  echo verbose; \\\n",
            "\tfi\n",
            "\tfor f in $^; do echo $$f; done\n",
            "\t# a comment\n",
            "\t$(MAKE) -C sub\n",
            "foo: ; touch $@\n",
        );
        assert_eq!(check(text), vec![]);
    }

    #[test]
    fn test_unterminated_if() {
        let text = "all:\n\techo hi\n\tif true; then \\\n\t  echo x\n";
        assert_eq!(
            check(text),
            vec![(
                Range::new(Position::new(3, 1), Position::new(3, 9)),
                "shell syntax error: Syntax error: end of file unexpected (expecting \"fi\")"
                    .to_string()
            )]
        );
    }

    #[test]
    fn test_unexpected_token() {
        let text = "all:\n\t@ls )\n";
        assert_eq!(
            check(text),
            vec![(
                Range::new(Position::new(1, 1), Position::new(1, 6)),
                "shell syntax error: Syntax error: \")\" unexpected".to_string()
            )]
        );
    }

    #[test]
    fn test_error_on_inline_recipe() {
        let text = "all: ; echo \"oops\n";
        let diags = check(text);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].0.start.line, 0);
    }

    #[test]
    fn test_bash_shell() {
        // Process substitution is bash-only.
        let text = "SHELL := /bin/bash\nall:\n\tdiff <(echo a) <(echo b)\n";
        assert_eq!(check(text), vec![]);
        let text = "SHELL := /usr/bin/env bash\nall:\n\tls )\n";
        assert_eq!(
            check(text),
            vec![(
                Range::new(Position::new(2, 1), Position::new(2, 5)),
                "shell syntax error: syntax error near unexpected token `)'".to_string()
            )]
        );
    }

    #[test]
    fn test_unknown_shell_skipped() {
        assert_eq!(check("SHELL = python3\nall:\n\tls )\n"), vec![]);
        assert_eq!(check("SHELL = $(BASH)\nall:\n\tls )\n"), vec![]);
        assert_eq!(check("all: SHELL = bash\nall:\n\tls )\n"), vec![]);
    }

    #[test]
    fn test_oneshell_skipped() {
        assert_eq!(check(".ONESHELL:\nall:\n\tif true; then\n\tfi\n"), vec![]);
    }

    #[test]
    fn test_missing_shell() {
        let parsed = Makefile::parse("SHELL = /nonexistent/sh\nall:\n\tls )\n");
        assert_eq!(
            check_shell_syntax("SHELL = /nonexistent/sh\nall:\n\tls )\n", &parsed.tree()),
            vec![]
        );
    }
}
