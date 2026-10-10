//! The `unreachable-target` check: targets that nothing depends on and that
//! aren't the default goal, so make only builds them when asked by name.
//!
//! Every target can be named on the command line, so this only looks at
//! targets that look like files made from other files: names with a `.` or
//! `/`, with prerequisites, none of which are `.PHONY` or `FORCE`-style
//! targets. Rules without prerequisites, like `bin/tool:` that runs another
//! build system, are usually commands run by hand, and double-colon rules
//! are often hooks. Only top-level makefiles whose includes could all be
//! followed are checked.
//!
//! A target counts as used if its name appears anywhere make could pick it
//! up from:
//!
//! - a prerequisite of any rule, special targets like `.PRECIOUS` included,
//!   or a word in the value of any variable;
//! - the default goal (any first target that might be read first, and
//!   `.DEFAULT_GOAL` or BSD `.MAIN`), or a `.PHONY` target;
//! - a word in a recipe that runs `$(MAKE)`, or an include file name, as
//!   make remakes included makefiles;
//! - a used name with the same stem but another extension, which an
//!   implicit rule may build it from (`foo.c` for `foo.o`).
//!
//! The check is skipped entirely when the set of used names can't be known:
//! when `MAKECMDGOALS` is consulted, when rules may be generated with
//! `$(eval)` and when a prerequisite expands anything but variables,
//! substitution references and functions that only rearrange words.
//!
//! TODO: a parent directory's makefile may run `$(MAKE) -C dir target`,
//! which isn't seen.
//! TODO: support nmake, whose inference rules aren't taken into account.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use makefile_lossless::{
    split_references, FunctionCall, Makefile, MakefileVariant, Modifier, ModifierArg,
    ModifierArgPart, ParsedReference, ReferenceError, TextPart,
};
use tower_lsp_server::ls_types::{Diagnostic, DiagnosticSeverity, DiagnosticTag, NumberOrString};

use crate::position::text_range_to_lsp_range;
use crate::targets::targets_with_ranges;
use crate::workspace::FileSet;

/// File names make reads by default, which are taken to be top-level
/// makefiles rather than fragments meant to be included.
const TOP_LEVEL_NAMES: &[&str] = &["GNUmakefile", "makefile", "Makefile", "BSDmakefile"];

/// Report targets of the current document of `files` that nothing uses.
pub fn check_unreachable_targets(files: &FileSet) -> Vec<Diagnostic> {
    let current = files.current();
    let variant = current.variant();
    let is_top_level = current
        .path()
        .and_then(Path::file_name)
        .and_then(|n| n.to_str())
        .is_some_and(|n| TOP_LEVEL_NAMES.contains(&n));
    if variant == MakefileVariant::NMake || !is_top_level || !files.is_complete() {
        return Vec::new();
    }
    let makefiles: Vec<(Makefile, MakefileVariant)> = files
        .docs()
        .map(|doc| (doc.makefile(), doc.variant()))
        .collect();
    let Some(used) = UsedNames::collect(&makefiles) else {
        return Vec::new();
    };

    let makefile = &makefiles[0].0;
    let source_text = current.text();
    let mut reported = HashSet::new();
    let mut diagnostics = Vec::new();
    for rule in makefile.rules() {
        if rule.scoped_assignment().is_some() {
            continue;
        }
        let targets = targets_with_ranges(&rule);
        if rule.is_grouped() && targets.iter().any(|(name, _)| used.contains(name)) {
            continue;
        }
        for (name, range) in targets {
            if !is_candidate(&name)
                || !used.is_derived(&name)
                || used.double_colon.contains(&name)
                || used.contains(&name)
                || !reported.insert(name.clone())
            {
                continue;
            }
            diagnostics.push(Diagnostic {
                range: text_range_to_lsp_range(source_text, range),
                severity: Some(DiagnosticSeverity::HINT),
                code: Some(NumberOrString::String("unreachable-target".to_string())),
                source: Some("makefile-lsp".to_string()),
                message: format!(
                    "nothing depends on target '{}' and it is not the default goal",
                    name
                ),
                tags: Some(vec![DiagnosticTag::UNNECESSARY]),
                ..Default::default()
            });
        }
    }
    diagnostics
}

/// Whether `name` is a plain file target this check considers.
fn is_candidate(name: &str) -> bool {
    name.contains(['.', '/'])
        && !(name.contains(['$', '%', '*', '?', '[', '('])
            || (name.starts_with('.') && !name.contains('/')))
}

/// The names that make might build a target for, from all makefiles of a
/// file set.
#[derive(Debug, Default)]
struct UsedNames {
    /// Complete words.
    words: HashSet<String>,
    /// Parts of words that also contain a variable reference.
    partial: Vec<String>,
    /// Targets of double-colon rules, which are often extension points.
    double_colon: HashSet<String>,
    /// The prerequisites of each target.
    prerequisites: HashMap<String, HashSet<String>>,
    /// Targets that are always out of date: `.PHONY` ones, and those whose
    /// rules have neither prerequisites nor a recipe, like `FORCE:`.
    forced: HashSet<String>,
}

impl UsedNames {
    /// Collect the used names, or `None` if they can't be determined.
    fn collect(makefiles: &[(Makefile, MakefileVariant)]) -> Option<Self> {
        let mut used = Self::default();
        let mut empty: HashSet<String> = HashSet::new();
        let mut nonempty: HashSet<String> = HashSet::new();
        let mut values: HashMap<String, Vec<String>> = HashMap::new();
        for (makefile, variant) in makefiles {
            for def in makefile.variable_definitions() {
                let (Some(name), Some(value)) = (def.name(), def.value_for(*variant)) else {
                    continue;
                };
                used.add_text(&value, *variant);
                values.entry(name).or_default().push(value);
            }
        }

        for (makefile, variant) in makefiles {
            let variant = *variant;
            if crate::diagnostics::may_generate_rules(makefile)
                || makefile
                    .variable_references()
                    .any(|r| matches!(r.name().as_deref(), Some("MAKECMDGOALS" | ".TARGETS")))
            {
                return None;
            }
            for goal in makefile.variable_definitions_by_name(".DEFAULT_GOAL") {
                let value = goal.value_for(variant).unwrap_or_default();
                if value.contains('$') {
                    return None;
                }
                used.add_text(&value, variant);
            }
            for path in crate::workspace::include_paths(makefile) {
                used.add_text(&path.name, variant);
            }
            used.add_default_goal(makefile);

            for rule in makefile.rules() {
                let targets: Vec<String> = rule.targets().collect();
                let prereqs: Vec<String> = rule
                    .prerequisites()
                    .chain(rule.order_only_prerequisites())
                    .collect();
                // BSD make also marks targets phony with a `.PHONY` source.
                let declares_phony = targets.iter().any(|t| t == ".PHONY");
                if declares_phony || prereqs.iter().any(|p| p == ".PHONY") {
                    if declares_phony && prereqs.iter().any(|p| p.contains('$')) {
                        return None;
                    }
                    let names = if declares_phony { &prereqs } else { &targets };
                    used.forced
                        .extend(names.iter().map(|n| normalize(n).to_string()));
                    for target in &targets {
                        used.add_text(target, variant);
                    }
                }
                let expander = Expander {
                    values: &values,
                    variant,
                };
                for prereq in &prereqs {
                    for text in expander.expand(prereq, &mut HashSet::new())? {
                        used.words
                            .extend(text.split_whitespace().map(|w| normalize(w).to_string()));
                    }
                    used.add_text(prereq, variant);
                }
                if prereqs.is_empty() && rule.recipe_nodes().next().is_none() {
                    empty.extend(targets.iter().map(|t| normalize(t).to_string()));
                } else {
                    nonempty.extend(targets.iter().map(|t| normalize(t).to_string()));
                }
                for target in &targets {
                    used.prerequisites
                        .entry(normalize(target).to_string())
                        .or_default()
                        .extend(prereqs.iter().map(|p| normalize(p).to_string()));
                }
                if rule.is_double_colon() {
                    used.double_colon.extend(targets);
                }
                for recipe in rule.recipe_nodes() {
                    if recipe
                        .references()
                        .any(|r| r.name().as_deref() == Some("MAKE"))
                    {
                        used.add_text(&recipe.shell_text(), variant);
                    }
                }
            }
        }
        used.forced.extend(empty.difference(&nonempty).cloned());
        Some(used)
    }

    /// Add the targets that may be the default goal of `makefile` if it is
    /// the first one make reads: the first target that doesn't start with
    /// `.` and isn't a pattern, of each rule up to the first one outside
    /// any conditional.
    fn add_default_goal(&mut self, makefile: &Makefile) {
        for rule in makefile.rules() {
            if rule.scoped_assignment().is_some() {
                continue;
            }
            let Some(goal) = rule
                .targets()
                .find(|t| !t.contains('%') && (!t.starts_with('.') || t.contains('/')))
            else {
                continue;
            };
            self.words.insert(normalize(&goal).to_string());
            if rule.enclosing_branches().is_empty() {
                break;
            }
        }
    }

    /// Add the words of `text`, keeping the literal parts of words that
    /// contain a variable reference as partial words.
    fn add_text(&mut self, text: &str, variant: MakefileVariant) {
        const MARKER: char = '\0';
        let mut literal = String::new();
        for part in split_references(text, variant) {
            match part {
                TextPart::Literal(range) => literal.push_str(&text[range]),
                _ => literal.push(MARKER),
            }
        }
        for word in literal.split_whitespace() {
            if word.contains(MARKER) {
                self.partial.extend(
                    word.split(MARKER)
                        .filter(|p| !p.is_empty())
                        .map(str::to_string),
                );
            } else {
                self.words.insert(normalize(word).to_string());
            }
        }
    }

    /// Whether `target` is made from other files: it has prerequisites,
    /// none of which are always out of date. Rules without them are usually
    /// commands run by hand.
    fn is_derived(&self, target: &str) -> bool {
        self.prerequisites
            .get(normalize(target))
            .is_some_and(|p| !p.is_empty() && p.is_disjoint(&self.forced))
    }

    /// Whether `target` may be built because of a used name. Its own
    /// prerequisites don't count for the implicit rule stems.
    fn contains(&self, target: &str) -> bool {
        let target = normalize(target);
        let stem = Path::new(target).with_extension("");
        let own = self.prerequisites.get(target);
        self.words.contains(target)
            || self.partial.iter().any(|p| target.contains(p.as_str()))
            || self.words.iter().any(|w| {
                Path::new(w).with_extension("") == stem && !own.is_some_and(|o| o.contains(w))
            })
    }
}

fn normalize(name: &str) -> &str {
    name.trim_start_matches("./")
}

/// The most texts an expansion may have, beyond which the check is skipped.
const MAX_EXPANSIONS: usize = 64;

/// Expands text using the values variables are assigned anywhere in the
/// makefiles.
///
/// Since a variable may have several values, depending on conditionals and
/// appends, text expands to each combination of them. Only plain references,
/// substitution references and a few GNU make functions that just rearrange
/// words are expanded.
struct Expander<'a> {
    values: &'a HashMap<String, Vec<String>>,
    variant: MakefileVariant,
}

impl Expander<'_> {
    /// The texts `text` may expand to, or `None` if it uses anything that
    /// can't be expanded. A reference to a variable that is being expanded
    /// expands to nothing.
    fn expand(&self, text: &str, visiting: &mut HashSet<String>) -> Option<Vec<String>> {
        let mut texts = vec![String::new()];
        for part in split_references(text, self.variant) {
            let expansions = match part {
                TextPart::Literal(range) => vec![text[range].to_string()],
                TextPart::Reference {
                    parsed: Ok(reference),
                    ..
                } => self.expand_reference(&reference, visiting)?,
                TextPart::Reference {
                    range,
                    parsed: Err(ReferenceError::FunctionCall { .. }),
                } => self.expand_function(&text[range], visiting)?,
                _ => return None,
            };
            texts = combine(&[texts, expansions], |[a, b]| Some(format!("{a}{b}")))?;
        }
        Some(texts)
    }

    fn expand_reference(
        &self,
        reference: &ParsedReference,
        visiting: &mut HashSet<String>,
    ) -> Option<Vec<String>> {
        let name = &reference.name;
        let defs = self.values.get(name)?;
        let mut values = Vec::new();
        if visiting.insert(name.clone()) {
            for def in defs {
                values.extend(self.expand(def, visiting)?);
            }
            visiting.remove(name);
        } else {
            values.push(String::new());
        }
        dedup(&mut values);
        match reference.modifiers.as_slice() {
            [] => Some(values),
            [Modifier::SysVSubstitute { from, to }] => {
                let from = self.expand_arg(from, visiting)?;
                let to = self.expand_arg(to, visiting)?;
                combine(&[values, from, to], |[value, from, to]| {
                    substitution_reference(value, from, to)
                })
            }
            _ => None,
        }
    }

    fn expand_arg(&self, arg: &ModifierArg, visiting: &mut HashSet<String>) -> Option<Vec<String>> {
        let mut texts = vec![String::new()];
        for part in arg.parts() {
            let expansions = match part {
                ModifierArgPart::Literal(text) => vec![text.clone()],
                ModifierArgPart::Expr(text) => self.expand(text, visiting)?,
                _ => return None,
            };
            texts = combine(&[texts, expansions], |[a, b]| Some(format!("{a}{b}")))?;
        }
        Some(texts)
    }

    /// Expand a call of a GNU make function, from `$` to the closing brace.
    fn expand_function(&self, text: &str, visiting: &mut HashSet<String>) -> Option<Vec<String>> {
        let (call, _) = FunctionCall::parse_prefix(text).ok()??;
        let args = call
            .arguments
            .iter()
            .map(|range| self.expand(&text[range.clone()], visiting))
            .collect::<Option<Vec<_>>>()?;
        let results = match (call.name.as_str(), args.len()) {
            ("subst", 3) => combine(&args, |[from, to, text]| Some(subst(from, to, text)))?,
            ("patsubst", 3) => combine(&args, |[pattern, replacement, text]| {
                map_words(text, |w| patsubst_word(pattern, replacement, w))
            })?,
            ("addprefix", 2) => combine(&args, |[prefix, names]| {
                map_words(names, |w| Some(format!("{prefix}{w}")))
            })?,
            ("addsuffix", 2) => combine(&args, |[suffix, names]| {
                map_words(names, |w| Some(format!("{w}{suffix}")))
            })?,
            ("filter" | "filter-out", 2) => {
                let keep = call.name == "filter";
                combine(&args, |[patterns, text]| {
                    let mut words = Vec::new();
                    for word in text.split_whitespace() {
                        let mut matches = false;
                        for pattern in patterns.split_whitespace() {
                            matches |= pattern_stem(pattern, word)?.is_some();
                        }
                        if matches == keep {
                            words.push(word);
                        }
                    }
                    Some(words.join(" "))
                })?
            }
            ("sort", 1) => combine(&args, |[list]| {
                let words: std::collections::BTreeSet<&str> = list.split_whitespace().collect();
                Some(words.into_iter().collect::<Vec<_>>().join(" "))
            })?,
            ("strip", 1) => combine(&args, |[text]| map_words(text, |w| Some(w.to_string())))?,
            ("notdir", 1) => combine(&args, |[names]| {
                map_words(names, |w| Some(w.rsplit('/').next()?.to_string()))
            })?,
            ("dir", 1) => combine(&args, |[names]| {
                map_words(names, |w| {
                    Some(w.rfind('/').map_or("./", |slash| &w[..=slash]).to_string())
                })
            })?,
            ("basename", 1) => combine(&args, |[names]| {
                map_words(names, |w| {
                    let start = w.rfind('/').map_or(0, |slash| slash + 1);
                    let end = w[start..].rfind('.').map_or(w.len(), |dot| start + dot);
                    Some(w[..end].to_string())
                })
            })?,
            _ => return None,
        };
        Some(results)
    }
}

/// Call `f` with each combination of one text from each of `lists`, or
/// return `None` if there are too many or `f` does.
fn combine<const N: usize>(
    lists: &[Vec<String>],
    f: impl Fn([&str; N]) -> Option<String>,
) -> Option<Vec<String>> {
    if lists.len() != N || lists.iter().map(Vec::len).product::<usize>() > MAX_EXPANSIONS {
        return None;
    }
    let mut results = Vec::new();
    let mut indices = [0; N];
    loop {
        results.push(f(std::array::from_fn(|i| lists[i][indices[i]].as_str()))?);
        let Some(i) = (0..N).rev().find(|&i| indices[i] + 1 < lists[i].len()) else {
            break;
        };
        indices[i] += 1;
        indices[i + 1..].fill(0);
    }
    dedup(&mut results);
    Some(results)
}

fn dedup(texts: &mut Vec<String>) {
    let mut seen = HashSet::new();
    texts.retain(|t| seen.insert(t.clone()));
}

/// Apply `f` to each word of `text`, joining the results with spaces as GNU
/// make does.
fn map_words(text: &str, f: impl Fn(&str) -> Option<String>) -> Option<String> {
    Some(
        text.split_whitespace()
            .map(f)
            .collect::<Option<Vec<_>>>()?
            .join(" "),
    )
}

/// GNU make's `$(subst)`, which appends `to` if `from` is empty.
fn subst(from: &str, to: &str, text: &str) -> String {
    if from.is_empty() {
        format!("{text}{to}")
    } else {
        text.replace(from, to)
    }
}

/// The part of `word` matched by the first `%` in `pattern`, or `Some(None)`
/// if it doesn't match. Without a `%`, `pattern` has to match the whole word
/// and the stem is empty. Returns `None` for patterns with a backslash,
/// which may quote the `%`.
fn pattern_stem<'a>(pattern: &str, word: &'a str) -> Option<Option<&'a str>> {
    if pattern.contains('\\') {
        return None;
    }
    let Some((prefix, suffix)) = pattern.split_once('%') else {
        return Some((word == pattern).then_some(""));
    };
    Some(
        (word.len() >= prefix.len() + suffix.len()
            && word.starts_with(prefix)
            && word.ends_with(suffix))
        .then(|| &word[prefix.len()..word.len() - suffix.len()]),
    )
}

/// `$(patsubst)` of a single word: the first `%` in `replacement` is
/// replaced by the stem, if `pattern` has one.
fn patsubst_word(pattern: &str, replacement: &str, word: &str) -> Option<String> {
    if replacement.contains('\\') {
        return None;
    }
    let Some(stem) = pattern_stem(pattern, word)? else {
        return Some(word.to_string());
    };
    if !pattern.contains('%') {
        return Some(replacement.to_string());
    }
    Some(replacement.replacen('%', stem, 1))
}

/// A substitution reference `$(VAR:from=to)`. Without a `%` in `from` it
/// replaces `from` at the end of each word, as if both started with a `%`.
fn substitution_reference(value: &str, from: &str, to: &str) -> Option<String> {
    let (from, to) = if from.contains('%') {
        (from.to_string(), to.to_string())
    } else {
        (format!("%{from}"), format!("%{to}"))
    };
    map_words(value, |w| patsubst_word(&from, &to, w))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::tests::Fixture;

    /// The unreachable targets reported for `name` in a fixture of `files`,
    /// as (line, message target name).
    fn unreachable(files: &[(&str, &str)], name: &str) -> Vec<(u32, String)> {
        let fx = Fixture::new(files);
        check_unreachable_targets(&fx.file_set(name))
            .into_iter()
            .map(|d| {
                assert_eq!(d.severity, Some(DiagnosticSeverity::HINT));
                assert_eq!(d.tags, Some(vec![DiagnosticTag::UNNECESSARY]));
                let start = d.message.find('\'').unwrap() + 1;
                let end = start + d.message[start..].find('\'').unwrap();
                (d.range.start.line, d.message[start..end].to_string())
            })
            .collect()
    }

    fn makefile(text: &str) -> Vec<(u32, String)> {
        unreachable(&[("Makefile", text)], "Makefile")
    }

    fn none() -> Vec<(u32, String)> {
        Vec::new()
    }

    const PHONY: &str = ".PHONY: all\n";

    #[test]
    fn test_unreferenced_file_target() {
        let text =
            format!("{PHONY}all: prog\nprog: prog.o\n\tcc -o $@ $^\nold.o: old.c\n\tcc -c $<\n");
        assert_eq!(makefile(&text), vec![(4, "old.o".to_string())]);
    }

    #[test]
    fn test_diagnostic() {
        let fx = Fixture::new(&[("Makefile", ".PHONY: all\nall:\nstale.o: x\n")]);
        let diags = check_unreachable_targets(&fx.file_set("Makefile"));
        assert_eq!(
            diags,
            vec![Diagnostic {
                range: tower_lsp_server::ls_types::Range::new(
                    tower_lsp_server::ls_types::Position::new(2, 0),
                    tower_lsp_server::ls_types::Position::new(2, 7),
                ),
                severity: Some(DiagnosticSeverity::HINT),
                code: Some(NumberOrString::String("unreachable-target".to_string())),
                source: Some("makefile-lsp".to_string()),
                message: "nothing depends on target 'stale.o' and it is not the default goal"
                    .to_string(),
                tags: Some(vec![DiagnosticTag::UNNECESSARY]),
                ..Default::default()
            }]
        );
    }

    #[test]
    fn test_reported_at_rule_not_target_specific_variable() {
        let text = format!("{PHONY}all:\nstale.o: CFLAGS = -O2\nstale.o: stale.c\n");
        assert_eq!(makefile(&text), vec![(3, "stale.o".to_string())]);
    }

    #[test]
    fn test_reported_once_per_name() {
        let text = format!("{PHONY}all:\nstale.o: a\nstale.o: b\n\ttouch $@\n");
        assert_eq!(makefile(&text), vec![(2, "stale.o".to_string())]);
    }

    #[test]
    fn test_default_goal_not_reported() {
        assert_eq!(
            makefile(".PHONY: clean\nprog.bin: x\nclean:\n\trm prog.bin\n"),
            none()
        );
        // make skips targets starting with `.` and patterns.
        assert_eq!(
            makefile(".PHONY: clean\n.c.o:\n\tcc\n%.x: %.y\n\tcp\nprog.bin: x\nclean:\n"),
            none()
        );
        // The first eligible target of the rule.
        assert_eq!(
            makefile(".PHONY: clean\n.foo prog.bin: x\nclean:\n"),
            none()
        );
        // Only the first rule.
        assert_eq!(
            makefile(".PHONY: clean\nprog.bin: x\nother.o: y\nclean:\n"),
            vec![(2, "other.o".to_string())]
        );
        // Target-specific variables aren't rules.
        assert_eq!(
            makefile(".PHONY: clean\nprog.bin: CFLAGS = -O2\nprog.bin: x\nother.o: y\nclean:\n"),
            vec![(3, "other.o".to_string())]
        );
    }

    #[test]
    fn test_default_goal_in_conditional() {
        let text = ".PHONY: clean\nifdef X\na.o: x\nelse\nb.o: y\nendif\nc.o: z\nd.o: w\nclean:\n";
        assert_eq!(makefile(text), vec![(7, "d.o".to_string())]);
    }

    #[test]
    fn test_default_goal_variable() {
        let text = ".PHONY: clean\nclean:\n.DEFAULT_GOAL := prog.bin\nprog.bin: x\n";
        assert_eq!(makefile(text), none());
        let text = ".PHONY: clean\nclean:\n.DEFAULT_GOAL := $(X)\nprog.bin: x\n";
        assert_eq!(makefile(text), none());
    }

    #[test]
    fn test_bsd_main() {
        let text = ".PHONY: clean\nclean:\n.MAIN: prog.bin\nprog.bin: x\nother.o: y\n";
        assert_eq!(
            unreachable(&[("BSDmakefile", text)], "BSDmakefile"),
            vec![(4, "other.o".to_string())]
        );
    }

    #[test]
    fn test_bsd_phony_attribute() {
        let text = ".MAIN: all\nall: prog\nprog: x\ndeploy.stamp: x .PHONY\n\ttouch $@\n";
        assert_eq!(unreachable(&[("BSDmakefile", text)], "BSDmakefile"), none());
    }

    #[test]
    fn test_phony_targets_not_reported() {
        let text = ".PHONY: all docs.html\nall:\ndocs.html: docs.md\n\tpandoc $<\n";
        assert_eq!(makefile(text), none());
    }

    #[test]
    fn test_without_phony() {
        assert_eq!(
            makefile("all: prog\nprog:\nstale.o: x\n"),
            vec![(2, "stale.o".to_string())]
        );
    }

    #[test]
    fn test_nmake_skipped() {
        // nmake has no .PHONY; this is parsed as nmake for the `!IF`.
        let text = "!IF 1\n!ENDIF\n.PHONY: all\nall:\nstale.o: x\n";
        assert_eq!(
            Makefile::parse(text).variant(),
            Some(MakefileVariant::NMake)
        );
        assert_eq!(makefile(text), none());
    }

    #[test]
    fn test_special_targets_not_reported() {
        let text = format!("{PHONY}all:\n.SUFFIXES:\n.PRECIOUS: x\n.DELETE_ON_ERROR:\n");
        assert_eq!(makefile(&text), none());
    }

    #[test]
    fn test_pattern_rules_not_reported() {
        let text = format!("{PHONY}all:\n%.o: %.c gen.h\n\tcc\ngen.h: gen.sh\n\ttouch $@\n");
        assert_eq!(makefile(&text), none());
    }

    #[test]
    fn test_commands_not_reported() {
        // Without prerequisites, a rule is more likely a command run by hand.
        let text = format!("{PHONY}all:\nbin/tool:\n\twaf build\n");
        assert_eq!(makefile(&text), none());
        let text = format!("{PHONY}all:\nbin/tool: FORCE\n\twaf build\nFORCE:\n");
        assert_eq!(makefile(&text), none());
        let text = ".PHONY: all force\nall:\nbin/tool: force src.c\n\twaf build\n";
        assert_eq!(makefile(text), none());
        let text = format!("{PHONY}all:\nbin/tool: FORCE\n\twaf build\nFORCE: x\n");
        assert_eq!(makefile(&text), vec![(2, "bin/tool".to_string())]);
    }

    #[test]
    fn test_special_target_prerequisites_count() {
        let text = format!("{PHONY}all:\n.PRECIOUS: keep.o\nkeep.o: keep.c\n");
        assert_eq!(makefile(&text), none());
    }

    #[test]
    fn test_order_only_prerequisites_count() {
        let text =
            format!("{PHONY}all: prog\nprog: | out/dir.stamp\nout/dir.stamp: x\n\ttouch $@\n");
        assert_eq!(makefile(&text), none());
    }

    #[test]
    fn test_double_colon_not_reported() {
        let text = format!("{PHONY}all:\nhook.stamp:: a\n\techo a\nhook.stamp:: b\n\techo b\n");
        assert_eq!(makefile(&text), none());
    }

    #[test]
    fn test_grouped_targets() {
        let text = format!("{PHONY}all: parse.c\nparse.c y.tab.h &: parse.y\n\tbison $<\n");
        assert_eq!(makefile(&text), none());
        let text = format!("{PHONY}all: parse.c\nparse.c y.tab.h: parse.y\n\tbison $<\n");
        assert_eq!(makefile(&text), vec![(2, "y.tab.h".to_string())]);
    }

    #[test]
    fn test_implicit_rule_stem() {
        // make builds prog.o from prog.c with a built-in rule.
        let text = format!("{PHONY}all: prog\nprog: prog.o\nprog.c: prog.y\n\tbison -o $@ $<\n");
        assert_eq!(makefile(&text), none());
        // And prog from prog.o.
        let text = format!("{PHONY}all: prog\nprog.o: prog.c\n");
        assert_eq!(makefile(&text), none());
    }

    #[test]
    fn test_variable_prerequisites() {
        let text = format!(
            "{PHONY}all: prog\nOBJS = a.o b.o\nprog: $(OBJS)\na.o: x.h\nb.o: y.h\nc.o: z.h\n"
        );
        assert_eq!(makefile(&text), vec![(6, "c.o".to_string())]);
    }

    #[test]
    fn test_variable_prerequisites_with_prefix() {
        let text = format!("{PHONY}all: prog\nB = build\nprog: $(B)/a.o\nbuild/a.o: x.h\n");
        assert_eq!(makefile(&text), none());
    }

    #[test]
    fn test_unexpandable_prerequisites_skipped() {
        let text =
            format!("{PHONY}all: prog\nprog: $(patsubst %.c,%.o,$(wildcard *.c))\nstale.o: x\n");
        assert_eq!(makefile(&text), none());
        let text =
            format!("{PHONY}all: prog\nprog: $(SRCS:.c=.o)\nSRCS = $(wildcard *.c)\nstale.o: x\n");
        assert_eq!(makefile(&text), none());
        let text = format!("{PHONY}all: prog\nprog: $(SRCS:\\%.c=%.o)\nSRCS = a.c\nstale.o: x\n");
        assert_eq!(makefile(&text), none());
        let text = format!("{PHONY}all: prog\nprog: $(sort $(SRCS) $(SRCS) $(SRCS) $(SRCS) $(SRCS) $(SRCS) $(SRCS))\nSRCS = a.c\nSRCS = b.c\nstale.o: x\n");
        assert_eq!(makefile(&text), none());
        let text = format!("{PHONY}all: prog\nprog: $(UNDEFINED)\nstale.o: x\n");
        assert_eq!(makefile(&text), none());
        let text = format!("{PHONY}all: prog\nOBJS = $(shell ls)\nprog: $(OBJS)\nstale.o: x\n");
        assert_eq!(makefile(&text), none());
        let text = format!("{PHONY}.SECONDEXPANSION:\nall: prog\nprog: $$@.o\nstale.o: x\n");
        assert_eq!(makefile(&text), none());
    }

    #[test]
    fn test_substitution_reference_prerequisites() {
        let rules = "build/a.o: a.c\nbuild/b.o: b.c\nstale.o: x\n";
        let text =
            format!("{PHONY}all: prog\nSRCS = a.c b.c\nprog: $(SRCS:%.c=build/%.o)\n{rules}");
        assert_eq!(makefile(&text), vec![(6, "stale.o".to_string())]);
        let text = format!(
            "{PHONY}all: prog\nSRCS = a.c b.c\nB = build\nprog: $(SRCS:%.c=$(B)/%.o)\n{rules}"
        );
        assert_eq!(makefile(&text), vec![(7, "stale.o".to_string())]);
        let text = format!(
            "{PHONY}all: prog\nSRCS = a.c b.c\nOBJS = $(SRCS:%.c=build/%.o)\nprog: $(OBJS)\n{rules}"
        );
        assert_eq!(makefile(&text), vec![(7, "stale.o".to_string())]);
        // The suffix form only replaces at the end of words.
        let text = format!(
            "{PHONY}all: prog\nSRCS = a.c b.c.c\nprog: $(SRCS:.c=_t.o)\na_t.o: x\nb.c_t.o: y\nb_t.o.c: z\n"
        );
        assert_eq!(makefile(&text), vec![(6, "b_t.o.c".to_string())]);
        let text = format!("{PHONY}all: prog\nSRCS = a.c\nprog: ${{SRCS:.c=_t.o}}\na_t.o: x\n");
        assert_eq!(
            unreachable(&[("BSDmakefile", &text)], "BSDmakefile"),
            none()
        );
        // BSD make modifiers other than substitution references aren't
        // expanded.
        let text = format!("{PHONY}all: prog\nSRCS = a.c\nprog: ${{SRCS:R}}\nstale.o: x\n");
        assert_eq!(
            unreachable(&[("BSDmakefile", &text)], "BSDmakefile"),
            none()
        );
    }

    #[test]
    fn test_function_prerequisites() {
        let check = |prereqs: &str, targets: &str| {
            let text = format!(
                "{PHONY}all: prog\nSRCS = src/a.c src/b.h\nprog: {prereqs}\n{targets}: x\n"
            );
            makefile(&text)
        };
        let stale = |target: &str| vec![(4, target.to_string())];
        assert_eq!(
            check("$(patsubst src/%.c,obj/%.o,$(SRCS))", "obj/a.o"),
            none()
        );
        assert_eq!(
            check("$(patsubst src/%.c,obj/%.o,$(SRCS))", "obj/b.o"),
            stale("obj/b.o")
        );
        assert_eq!(check("$(subst src/,obj/,$(SRCS))", "obj/b.h"), none());
        assert_eq!(check("$(addprefix out/,$(SRCS))", "out/src/b.h"), none());
        assert_eq!(check("$(addsuffix .gz,$(SRCS))", "src/a.c.gz"), none());
        assert_eq!(
            check("$(addsuffix .gz,$(filter %.c,$(SRCS)))", "src/a.c.gz"),
            none()
        );
        assert_eq!(
            check("$(addsuffix .gz,$(filter %.c,$(SRCS)))", "src/b.h.gz"),
            stale("src/b.h.gz")
        );
        assert_eq!(
            check("$(addsuffix .gz,$(filter-out %.c,$(SRCS)))", "src/b.h.gz"),
            none()
        );
        assert_eq!(
            check("$(addsuffix .gz,$(filter-out %.c,$(SRCS)))", "src/a.c.gz"),
            stale("src/a.c.gz")
        );
        assert_eq!(
            check("$(addprefix d/,$(sort $(strip $(SRCS))))", "d/src/a.c"),
            none()
        );
        assert_eq!(check("$(addprefix d/,$(notdir $(SRCS)))", "d/a.c"), none());
        assert_eq!(check("$(addsuffix x.t,$(dir $(SRCS)))", "src/x.t"), none());
        assert_eq!(
            check("$(addsuffix .gz,$(basename $(SRCS)))", "src/a.gz"),
            none()
        );
    }

    #[test]
    fn test_expand() {
        // As GNU make 4.4.1 expands them.
        let values = HashMap::from([
            (
                "X".to_string(),
                vec!["  a.c  b.c\t.c  c.h d.cc  ".to_string()],
            ),
            ("P".to_string(), vec!["a".to_string(), "b".to_string()]),
        ]);
        let expander = Expander {
            values: &values,
            variant: MakefileVariant::GNUMake,
        };
        let expand = |text: &str| expander.expand(text, &mut HashSet::new());
        let one = |text: &str| Some(vec![text.to_string()]);
        assert_eq!(expand("$(X:.c=.o)"), one("a.o b.o .o c.h d.cc"));
        assert_eq!(expand("$(X:%.c=o/%.o)"), one("o/a.o o/b.o o/.o c.h d.cc"));
        assert_eq!(expand("$(X:.c=%.o)"), one("a%.o b%.o %.o c.h d.cc"));
        assert_eq!(expand("$(X:%.c=%%.o)"), one("a%.o b%.o %.o c.h d.cc"));
        assert_eq!(expand("$(X:=.x)"), one("a.c.x b.c.x .c.x c.h.x d.cc.x"));
        assert_eq!(expand("$(X:a%=%z)"), one(".cz b.c .c c.h d.cc"));
        assert_eq!(expand("$(X:%.c=x)"), one("x x x c.h d.cc"));
        assert_eq!(expand("$(patsubst a.c,b.o,a.c xa.c)"), one("b.o xa.c"));
        assert_eq!(expand("$(patsubst %.c ,x,a.c)"), one("a.c"));
        assert_eq!(expand("$(subst ,Z,ab)"), one("abZ"));
        assert_eq!(expand("$(filter a%b%,a1b2 a1b%)"), one("a1b%"));
        assert_eq!(expand("$(sort b a  b c)"), one("a b c"));
        assert_eq!(expand("$(notdir a/b c/ d)"), one("b  d"));
        assert_eq!(expand("$(dir a/b c/ d)"), one("a/ c/ ./"));
        assert_eq!(
            expand("$(basename a.b/c d.e f.g.h .i)"),
            one("a.b/c d f.g ")
        );
        assert_eq!(
            expand("pre$(P)post"),
            Some(vec!["preapost".to_string(), "prebpost".to_string()])
        );
        assert_eq!(
            expand("$(addprefix $(P)/,x)"),
            Some(vec!["a/x".to_string(), "b/x".to_string()])
        );
        assert_eq!(expand("$(patsubst \\%.c,x,%.c)"), None);
        assert_eq!(expand("$(wildcard *.c)"), None);
        assert_eq!(expand("$(addprefix a,b,c)"), one("ab,c"));
    }

    #[test]
    fn test_recursive_variable_prerequisites() {
        let text = format!("{PHONY}all: prog\nA = $(B)\nB = $(A) x.o\nprog: $(A)\nstale.o: x\n");
        assert_eq!(makefile(&text), vec![(5, "stale.o".to_string())]);
    }

    #[test]
    fn test_recursive_make() {
        let text = format!("{PHONY}all:\n\t$(MAKE) docs.pdf\ndocs.pdf: docs.tex\n\tlatex $<\n");
        assert_eq!(makefile(&text), none());
        let text = format!("{PHONY}all:\n\techo docs.pdf\ndocs.pdf: docs.tex\n\tlatex $<\n");
        assert_eq!(makefile(&text), vec![(3, "docs.pdf".to_string())]);
    }

    #[test]
    fn test_makecmdgoals_skipped() {
        let text =
            format!("{PHONY}all:\nifeq ($(MAKECMDGOALS),stale.o)\nX = 1\nendif\nstale.o: x\n");
        assert_eq!(makefile(&text), none());
        let text =
            format!("{PHONY}all:\nifneq ($(filter stale.o,$(MAKECMDGOALS)),)\nendif\nstale.o: x\n");
        assert_eq!(makefile(&text), none());
    }

    #[test]
    fn test_eval_skipped() {
        let text = format!("{PHONY}all:\n$(eval $(RULES))\nstale.o: x\n");
        assert_eq!(makefile(&text), none());
    }

    #[test]
    fn test_remade_makefiles_not_reported() {
        let text = format!("{PHONY}all:\ninclude deps.mk\ndeps.mk: srcs\n\tgen > $@\n");
        assert_eq!(
            unreachable(&[("Makefile", &text), ("deps.mk", "")], "Makefile"),
            none()
        );
    }

    #[test]
    fn test_unfollowed_include_skipped() {
        let text = format!("{PHONY}all:\n-include missing.mk\nstale.o: x\n");
        assert_eq!(makefile(&text), none());
    }

    #[test]
    fn test_included_fragment() {
        let files = [
            (
                "Makefile",
                ".PHONY: all\nall: lib.a\ninclude rules.mk\nlib.a: frag.o\n",
            ),
            ("rules.mk", "frag.o: frag.c\nstale.o: stale.c\n"),
        ];
        // Fragments aren't checked, since what they define is up to the
        // makefiles including them.
        assert_eq!(unreachable(&files, "rules.mk"), none());
        let files = [
            (
                "Makefile",
                ".PHONY: all\nall: lib.a\ninclude rules.mk\nlib.a: frag.o\nown.o: x\n",
            ),
            ("rules.mk", "frag.o: frag.c\nOBJS = own.o\n"),
        ];
        assert_eq!(unreachable(&files, "Makefile"), none());
        let files = [
            ("Makefile", "include rules.mk\nlib.a: frag.o\nstale.o: x\n"),
            ("rules.mk", ".PHONY: all\nall: lib.a\n"),
        ];
        assert_eq!(
            unreachable(&files, "Makefile"),
            vec![(2, "stale.o".to_string())]
        );
    }

    #[test]
    fn test_other_names_not_checked() {
        let text = format!("{PHONY}all:\nout.txt: in.txt\n\tcp $< $@\n");
        assert_eq!(unreachable(&[("rules", &text)], "rules"), none());
    }
}
