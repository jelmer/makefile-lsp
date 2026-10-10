//! Code lenses to run targets.
//!
//! The server doesn't run make itself: each lens invokes a command that the
//! client implements, so lenses are only offered to clients that say they
//! provide it.

use std::collections::HashSet;
use std::path::Path;

use makefile_lossless::TextRange;
use serde_json::{json, Value};
use tower_lsp_server::ls_types::{CodeLens, Command};

use crate::builtins::find_special_target;
use crate::dep_graph::is_graph_target;
use crate::position::text_range_to_lsp_range;
use crate::targets::targets_with_ranges;
use crate::workspace::FileSet;

/// The client-side command run by a "Run" lens. Its single argument is an
/// object with the absolute `makefile` to pass to make with `-f`, the
/// `directory` to run make in and the `target` to build.
pub const RUN_TARGET_COMMAND: &str = "makefile-lsp.runTarget";

/// Whether the client asked for run lenses, by passing
/// `{"codeLens": {"runTarget": true}}` as initialization options, which
/// means that it implements [`RUN_TARGET_COMMAND`].
pub fn run_target_enabled(options: Option<&Value>) -> Result<bool, String> {
    let Some(code_lens) = options.and_then(|o| o.get("codeLens")) else {
        return Ok(false);
    };
    match code_lens.get("runTarget") {
        None if code_lens.is_object() => Ok(false),
        Some(Value::Bool(enabled)) => Ok(*enabled),
        _ => Err(format!(
            "invalid codeLens initialization option, expected {{\"runTarget\": <bool>}}: {code_lens}"
        )),
    }
}

/// Whether `name` can be built by naming it on the make command line.
///
/// Pattern rules, special targets, suffix rules and targets containing
/// variable references (whose expansion isn't known) are left out.
fn is_runnable_target(name: &str) -> bool {
    if name.contains('$') || !is_graph_target(name) || find_special_target(name).is_some() {
        return false;
    }
    // A double-suffix rule such as `.c.o`.
    let is_suffix_rule = name.strip_prefix('.').is_some_and(|rest| {
        !rest.contains('/')
            && rest
                .split_once('.')
                .is_some_and(|(a, b)| !a.is_empty() && !b.is_empty() && !b.contains('.'))
    });
    !is_suffix_rule
}

/// "Run" lenses for the targets defined in the current document of `files`.
///
/// Targets are run from the topmost makefile that includes the document, if
/// one is known, and otherwise from the document itself. Only the first
/// definition of each target gets a lens.
pub fn run_target_lenses(files: &FileSet) -> Vec<CodeLens> {
    let doc = files.current();
    let Some(makefile) = files
        .includers()
        .first()
        .map(|p| p.as_path())
        .or(doc.path())
    else {
        return Vec::new();
    };
    let (Some(makefile_str), Some(directory)) =
        (makefile.to_str(), makefile.parent().and_then(Path::to_str))
    else {
        tracing::warn!(
            "not offering run lenses for non-UTF-8 path {}",
            makefile.display()
        );
        return Vec::new();
    };
    let text = doc.text();
    let mut seen: HashSet<String> = HashSet::new();
    let mut lenses = Vec::new();
    for rule in doc.makefile().rules() {
        let targets: Vec<(String, TextRange)> = targets_with_ranges(&rule)
            .into_iter()
            .filter(|(name, _)| is_runnable_target(name))
            .collect();
        let named = targets.len() > 1;
        for (name, range) in targets {
            if !seen.insert(name.clone()) {
                continue;
            }
            let title = if named {
                format!("Run {name}")
            } else {
                "Run".to_string()
            };
            lenses.push(CodeLens {
                range: text_range_to_lsp_range(text, range),
                command: Some(Command {
                    title,
                    command: RUN_TARGET_COMMAND.to_string(),
                    arguments: Some(vec![json!({
                        "makefile": makefile_str,
                        "directory": directory,
                        "target": name,
                    })]),
                }),
                data: None,
            });
        }
    }
    lenses
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::tests::Fixture;

    /// The lenses for `name` as (line, title, makefile, directory, target),
    /// with paths relative to the fixture.
    fn lenses(fx: &Fixture, name: &str) -> Vec<(u32, String, String, String, String)> {
        let root = fx.path("");
        let rel = |v: &Value| {
            let path = Path::new(v.as_str().unwrap());
            path.strip_prefix(&root).unwrap().display().to_string()
        };
        run_target_lenses(&fx.file_set(name))
            .into_iter()
            .map(|lens| {
                let command = lens.command.unwrap();
                assert_eq!(command.command, RUN_TARGET_COMMAND);
                let args = command.arguments.unwrap();
                assert_eq!(args.len(), 1);
                (
                    lens.range.start.line,
                    command.title,
                    rel(&args[0]["makefile"]),
                    rel(&args[0]["directory"]),
                    args[0]["target"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    fn lens(
        line: u32,
        title: &str,
        makefile: &str,
        dir: &str,
        target: &str,
    ) -> (u32, String, String, String, String) {
        (
            line,
            title.to_string(),
            makefile.to_string(),
            dir.to_string(),
            target.to_string(),
        )
    }

    #[test]
    fn test_run_target_enabled() {
        assert_eq!(run_target_enabled(None), Ok(false));
        assert_eq!(run_target_enabled(Some(&json!({}))), Ok(false));
        assert_eq!(
            run_target_enabled(Some(&json!({"codeLens": {}}))),
            Ok(false)
        );
        assert_eq!(
            run_target_enabled(Some(&json!({"codeLens": {"runTarget": true}}))),
            Ok(true)
        );
        assert_eq!(
            run_target_enabled(Some(&json!({"codeLens": {"runTarget": false}}))),
            Ok(false)
        );
        assert_eq!(
            run_target_enabled(Some(&json!({"codeLens": {"runTarget": "yes"}}))),
            Err(
                "invalid codeLens initialization option, expected {\"runTarget\": <bool>}: \
                 {\"runTarget\":\"yes\"}"
                    .to_string()
            )
        );
        assert_eq!(
            run_target_enabled(Some(&json!({"codeLens": true}))),
            Err(
                "invalid codeLens initialization option, expected {\"runTarget\": <bool>}: true"
                    .to_string()
            )
        );
    }

    #[test]
    fn test_simple_targets() {
        let fx = Fixture::new(&[("Makefile", "all: foo\n\nfoo:\n\techo foo\n")]);
        assert_eq!(
            lenses(&fx, "Makefile"),
            vec![
                lens(0, "Run", "Makefile", "", "all"),
                lens(2, "Run", "Makefile", "", "foo"),
            ]
        );
    }

    #[test]
    fn test_range_covers_target() {
        let fx = Fixture::new(&[("Makefile", "x: y\n  \nfoo bar: baz\n")]);
        let ranges: Vec<_> = run_target_lenses(&fx.file_set("Makefile"))
            .into_iter()
            .map(|l| {
                (
                    l.range.start.line,
                    l.range.start.character,
                    l.range.end.line,
                    l.range.end.character,
                )
            })
            .collect();
        assert_eq!(ranges, vec![(0, 0, 0, 1), (2, 0, 2, 3), (2, 4, 2, 7)]);
    }

    #[test]
    fn test_multiple_targets_are_named() {
        let fx = Fixture::new(&[("Makefile", "a b: c\n")]);
        assert_eq!(
            lenses(&fx, "Makefile"),
            vec![
                lens(0, "Run a", "Makefile", "", "a"),
                lens(0, "Run b", "Makefile", "", "b"),
            ]
        );
    }

    #[test]
    fn test_skips_unrunnable_targets() {
        let fx = Fixture::new(&[(
            "Makefile",
            ".PHONY: all\n\
             .DEFAULT_GOAL := all\n\
             all:\n\
             %.o: %.c\n\
             \t$(CC) -c $<\n\
             .c.o:\n\
             \t$(CC) -c $<\n\
             $(OUT): x\n\
             .SUFFIXES: .c .o\n\
             .venv: requirements.txt\n",
        )]);
        assert_eq!(
            lenses(&fx, "Makefile"),
            vec![
                lens(2, "Run", "Makefile", "", "all"),
                lens(9, "Run", "Makefile", "", ".venv"),
            ]
        );
    }

    #[test]
    fn test_only_first_definition() {
        let fx = Fixture::new(&[(
            "Makefile",
            "all: a\nall: b\nclean::\n\trm a\nclean::\n\trm b\n",
        )]);
        assert_eq!(
            lenses(&fx, "Makefile"),
            vec![
                lens(0, "Run", "Makefile", "", "all"),
                lens(2, "Run", "Makefile", "", "clean"),
            ]
        );
    }

    #[test]
    fn test_included_targets_not_shown() {
        let fx = Fixture::new(&[
            ("Makefile", "include rules.mk\nall:\n"),
            ("rules.mk", "x:\n"),
        ]);
        assert_eq!(
            lenses(&fx, "Makefile"),
            vec![lens(1, "Run", "Makefile", "", "all")]
        );
    }

    #[test]
    fn test_fragment_runs_via_including_makefile() {
        let fx = Fixture::new(&[
            ("Makefile", "include sub/rules.mk\nall:\n"),
            ("sub/rules.mk", "check:\n"),
        ]);
        assert_eq!(
            lenses(&fx, "sub/rules.mk"),
            vec![lens(0, "Run", "Makefile", "", "check")]
        );
    }

    #[test]
    fn test_standalone_file_in_subdirectory() {
        let fx = Fixture::new(&[("sub/build.mk", "check:\n")]);
        assert_eq!(
            lenses(&fx, "sub/build.mk"),
            vec![lens(0, "Run", "sub/build.mk", "sub", "check")]
        );
    }

    #[test]
    fn test_not_a_file() {
        let doc = crate::workspace::Document::new(
            "untitled:Untitled-1".parse().unwrap(),
            "all:\n".to_string(),
        );
        assert_eq!(run_target_lenses(&FileSet::single(doc)), vec![]);
    }
}
