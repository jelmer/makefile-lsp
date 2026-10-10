//! Check recipe lines for shell syntax errors by running them through
//! `sh -n`.
//!
//! This spawns a process per recipe line (or per recipe with `.ONESHELL`),
//! so it is only run when a document is opened or saved, not on every
//! change.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};

use makefile_lossless::{
    split_references, ConditionalItem, Makefile, MakefileItem, MakefileVariant, Recipe, Rule,
    TextPart,
};
use tower_lsp_server::ls_types::{Diagnostic, DiagnosticSeverity, NumberOrString};

use crate::position::text_range_to_lsp_range;

/// Word substituted for make variable and function references, so the shell
/// sees a plain word in their place.
const PLACEHOLDER: &str = "__make_ref__";

/// Check the recipe lines of `makefile` for shell syntax errors.
///
/// The shell is `/bin/sh` unless `SHELL` is set to a known Bourne-style
/// shell. Each logical recipe line (continuations included) is checked on
/// its own, as make runs each in a separate shell, after rewriting it as
/// described for `shell_script`. With GNU make's `.ONESHELL`, all lines of
/// a recipe are checked together as one script.
///
/// Skipped entirely when `SHELL` can't be determined statically or isn't a
/// Bourne-style shell, or when `.ONESHELL` is only set conditionally. With
/// `.ONESHELL`, recipes with conditionals in them are skipped.
///
/// Reported as warnings rather than errors, as a make reference that
/// expands to shell syntax (say, `then` or `;`) can make a valid line look
/// broken.
pub fn check_shell_syntax(
    source_text: &str,
    makefile: &Makefile,
    variant: MakefileVariant,
) -> Vec<Diagnostic> {
    let Some(oneshell) = oneshell(makefile, variant) else {
        return Vec::new();
    };
    let Some(shell) = shell_program(makefile) else {
        return Vec::new();
    };

    let mut cache: HashMap<String, Option<(usize, String)>> = HashMap::new();
    let mut diagnostics = Vec::new();
    for rule in makefile.rules() {
        let scripts = if oneshell {
            oneshell_recipes(&rule).into_iter().collect()
        } else {
            rule.recipe_nodes()
                .map(|recipe| vec![recipe])
                .collect::<Vec<_>>()
        };
        for recipes in scripts {
            // With .ONESHELL and a Bourne-style shell, GNU make strips the
            // prefix characters from every line, not just the first.
            let script = recipes
                .iter()
                .map(|recipe| shell_script(&recipe.shell_text(), variant))
                .collect::<Vec<_>>()
                .join("\n");
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
            let ranges: Vec<_> = recipes.iter().flat_map(Recipe::line_ranges).collect();
            diagnostics.push(Diagnostic {
                range: text_range_to_lsp_range(source_text, ranges[line.min(ranges.len() - 1)]),
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

/// Whether each recipe runs as a single script, or `None` if that depends
/// on a conditional.
///
/// GNU make honours `.ONESHELL` wherever it appears in the makefile. BSD
/// make (as of bmake 20200710) has no `.ONESHELL`, and although its jobs
/// mode runs a recipe in one shell, it still wraps each line separately.
fn oneshell(makefile: &Makefile, variant: MakefileVariant) -> Option<bool> {
    if !matches!(
        variant,
        MakefileVariant::GNUMake | MakefileVariant::POSIXMake
    ) {
        return Some(false);
    }
    let mut conditional = false;
    for rule in makefile.rules_by_target(".ONESHELL") {
        if MakefileItem::Rule(rule).enclosing_branches().is_empty() {
            return Some(true);
        }
        conditional = true;
    }
    (!conditional).then_some(false)
}

/// The recipe lines of `rule`, or `None` if its body has conditionals, as
/// which lines make passes to the shell is then unknown.
fn oneshell_recipes(rule: &Rule) -> Option<Vec<Recipe>> {
    rule.body_items()
        .map(|item| match item {
            ConditionalItem::Recipe(recipe) => Some(recipe),
            _ => None,
        })
        .collect()
}

/// The shell make would run recipes with, or `None` if it can't be
/// determined or isn't one we can check with `-n`.
fn shell_program(makefile: &Makefile) -> Option<String> {
    let mut values = HashSet::new();
    for def in makefile.variable_definitions_by_name("SHELL") {
        if def.is_target_specific() {
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
fn shell_script(shell_text: &str, variant: MakefileVariant) -> String {
    let rest =
        shell_text.trim_start_matches(|c: char| matches!(c, '@' | '-' | '+') || c.is_whitespace());
    let mut out = String::with_capacity(rest.len());
    for part in split_references(rest, variant) {
        let range = part.range();
        let text = &rest[range.clone()];
        match part {
            TextPart::EscapedDollar(_) => out.push('$'),
            // A lone `$` at the end of the line is passed through.
            TextPart::Reference { .. } if text != "$" => {
                let operator_follows = skip_blanks_forward(&rest[range.end..])
                    .chars()
                    .next()
                    .is_none_or(|c| matches!(c, '|' | '&' | ';' | '<' | '>' | ')'));
                if operator_follows || !in_command_position(&out) {
                    out.push_str(PLACEHOLDER);
                } else {
                    out.push_str(":;");
                }
                // Keep line numbers stable for references split over lines.
                for _ in text.matches('\n') {
                    out.push_str("\\\n");
                }
            }
            _ => out.push_str(text),
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::ls_types::{Position, Range};

    fn check(text: &str) -> Vec<(Range, String)> {
        let parsed = Makefile::parse(text);
        check_shell_syntax(text, &parsed.tree(), MakefileVariant::GNUMake)
            .into_iter()
            .map(|d| (d.range, d.message))
            .collect()
    }

    #[test]
    fn test_shell_script_substitutes_references() {
        assert_eq!(
            shell_script(
                "@-$(CC) -o $@ ${SRCS} $$HOME $(call f,$(x),(y)) x$(Y)z",
                MakefileVariant::GNUMake
            ),
            ":; -o __make_ref__ __make_ref__ $HOME __make_ref__ x__make_ref__z"
        );
    }

    #[test]
    fn test_shell_script_bracket_types() {
        assert_eq!(
            shell_script("echo $(a ${b)} ${c $(d}) $", MakefileVariant::GNUMake),
            "echo __make_ref__} __make_ref__) $"
        );
    }

    #[test]
    fn test_shell_script_bsd_modifiers() {
        assert_eq!(
            shell_script("echo ${X:S,},x,} done", MakefileVariant::BSDMake),
            "echo __make_ref__ done"
        );
    }

    #[test]
    fn test_shell_script_keeps_lines() {
        assert_eq!(
            shell_script(
                "echo $(foo \\\n\tbar) \\\n\tbaz $",
                MakefileVariant::GNUMake
            ),
            "echo __make_ref__\\\n \\\n\tbaz $"
        );
    }

    #[test]
    fn test_shell_script_command_position() {
        assert_eq!(
            shell_script(
                "$(Q)for f in $^; do $(Q)echo $$f; done",
                MakefileVariant::GNUMake
            ),
            ":;for f in __make_ref__; do __make_ref__echo $f; done"
        );
        assert_eq!(
            shell_script(
                "$(QUIET) \\\n\t($(foreach x,y,z &&) true) && $(RM) $@",
                MakefileVariant::GNUMake
            ),
            ":; \\\n\t(:; true) && :; __make_ref__"
        );
        assert_eq!(
            shell_script("$(CMD) | grep x; $(CMD)", MakefileVariant::GNUMake),
            "__make_ref__ | grep x; __make_ref__"
        );
        assert_eq!(
            shell_script(
                "{ $(foreach m,$(M),echo $(m);) } > out",
                MakefileVariant::GNUMake
            ),
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
    fn test_continuation_line_keeps_second_tab() {
        // make strips only one tab from a continuation line, so the second
        // is part of the command.
        let text = "all:\n\tif true; then \\\n\t\techo x\n";
        assert_eq!(
            check(text),
            vec![(
                Range::new(Position::new(2, 1), Position::new(2, 8)),
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
    fn test_shell_in_conditional_in_rule_body() {
        // GNU make treats this assignment as global, not target-specific.
        let text = "a:\nifndef X\n\techo\nSHELL := /bin/bash\nendif\nall:\n\tls )\n";
        assert_eq!(
            check(text),
            vec![(
                Range::new(Position::new(6, 1), Position::new(6, 5)),
                "shell syntax error: syntax error near unexpected token `)'".to_string()
            )]
        );
    }

    fn check_variant(text: &str, variant: MakefileVariant) -> Vec<(Range, String)> {
        let parsed = Makefile::parse(text);
        check_shell_syntax(text, &parsed.tree(), variant)
            .into_iter()
            .map(|d| (d.range, d.message))
            .collect()
    }

    #[test]
    fn test_oneshell_split_if() {
        let body = "all:\n\t@if true; then\n\t  -echo yes\n\tfi\n";
        assert_eq!(
            check(body),
            vec![
                (
                    Range::new(Position::new(1, 1), Position::new(1, 15)),
                    "shell syntax error: Syntax error: end of file unexpected (expecting \"fi\")"
                        .to_string()
                ),
                (
                    Range::new(Position::new(3, 1), Position::new(3, 3)),
                    "shell syntax error: Syntax error: \"fi\" unexpected".to_string()
                ),
            ]
        );
        // .ONESHELL applies to the whole makefile, wherever it appears.
        assert_eq!(check(&format!(".ONESHELL:\n{}", body)), vec![]);
        assert_eq!(check(&format!("{}.ONESHELL:\n", body)), vec![]);
    }

    #[test]
    fn test_oneshell_error_range() {
        let text = concat!(
            ".ONESHELL:\n",
            "all: ; echo start\n",
            "\tif true; then \\\n",
            "\t  echo a\n",
            "\n",
            "\techo b\n",
            "\tls )\n",
            "\tfi\n",
        );
        assert_eq!(
            check(text),
            vec![(
                Range::new(Position::new(6, 1), Position::new(6, 5)),
                "shell syntax error: Syntax error: \")\" unexpected (expecting \"fi\")".to_string()
            )]
        );
    }

    #[test]
    fn test_oneshell_unterminated() {
        let text = ".ONESHELL:\nall:\n\tif true; then\n\techo x\nother:\n\ttrue\n";
        assert_eq!(
            check(text),
            vec![(
                Range::new(Position::new(3, 1), Position::new(3, 7)),
                "shell syntax error: Syntax error: end of file unexpected (expecting \"fi\")"
                    .to_string()
            )]
        );
    }

    #[test]
    fn test_oneshell_heredoc() {
        // Inside an unquoted here-document `'` doesn't quote, so `$(` starts
        // a command substitution: valid line by line, not as one script.
        let body = "all:\n\tcat <<EOF\n\techo '$$('\n\tEOF\n";
        assert_eq!(check(body), vec![]);
        assert_eq!(
            check(&format!(".ONESHELL:\n{}", body)),
            vec![(
                Range::new(Position::new(4, 1), Position::new(4, 4)),
                "shell syntax error: Syntax error: Unterminated quoted string".to_string()
            )]
        );
    }

    #[test]
    fn test_oneshell_conditional() {
        // Which branch make uses is unknown.
        assert_eq!(
            check(
                ".ONESHELL:\nall:\nifdef V\n\tif true; then\nelse\n\tif false; then\nendif\n\tfi\n"
            ),
            vec![]
        );
        assert_eq!(
            check("ifdef ONE\n.ONESHELL:\nendif\nall:\n\tif true; then\n\tfi\n"),
            vec![]
        );
    }

    #[test]
    fn test_oneshell_bsd_make() {
        // bmake has no .ONESHELL, and even with -j an `if` split over lines
        // fails.
        assert_eq!(
            check_variant(
                ".ONESHELL:\nall:\n\tif true; then\n\tfi\n",
                MakefileVariant::BSDMake
            ),
            vec![
                (
                    Range::new(Position::new(2, 1), Position::new(2, 14)),
                    "shell syntax error: Syntax error: end of file unexpected (expecting \"fi\")"
                        .to_string()
                ),
                (
                    Range::new(Position::new(3, 1), Position::new(3, 3)),
                    "shell syntax error: Syntax error: \"fi\" unexpected".to_string()
                ),
            ]
        );
    }

    #[test]
    fn test_missing_shell() {
        let parsed = Makefile::parse("SHELL = /nonexistent/sh\nall:\n\tls )\n");
        assert_eq!(
            check_shell_syntax(
                "SHELL = /nonexistent/sh\nall:\n\tls )\n",
                &parsed.tree(),
                MakefileVariant::GNUMake
            ),
            vec![]
        );
    }
}
