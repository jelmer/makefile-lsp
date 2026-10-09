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
//! `$(eval)` and when a prerequisite expands anything but plain variables.
//!
//! TODO: a parent directory's makefile may run `$(MAKE) -C dir target`,
//! which isn't seen.
//! TODO: support nmake, whose inference rules aren't taken into account.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use makefile_lossless::{split_references, Makefile, MakefileVariant, ParsedReference, TextPart};
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
                for prereq in &prereqs {
                    if !expands_to_words(prereq, &values, variant, &mut HashSet::new()) {
                        return None;
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

/// Whether `text` only references variables that are assigned somewhere and
/// whose values in turn only do so, so that the words it expands to appear
/// literally in the makefiles.
///
/// TODO: expand substitution references like `$(SRCS:.c=.o)` and simple
/// functions like `$(addprefix)`.
fn expands_to_words(
    text: &str,
    values: &HashMap<String, Vec<String>>,
    variant: MakefileVariant,
    visiting: &mut HashSet<String>,
) -> bool {
    split_references(text, variant).into_iter().all(|part| {
        let reference: ParsedReference = match part {
            TextPart::Literal(_) => return true,
            TextPart::Reference {
                parsed: Ok(reference),
                ..
            } if reference.modifiers.is_empty() => reference,
            _ => return false,
        };
        let Some(defs) = values.get(&reference.name) else {
            return false;
        };
        if !visiting.insert(reference.name.clone()) {
            return true;
        }
        let ok = defs
            .iter()
            .all(|value| expands_to_words(value, values, variant, visiting));
        visiting.remove(&reference.name);
        ok
    })
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
        let text = format!("{PHONY}all: prog\nprog: $(SRCS:.c=.o)\nSRCS = a.c\nstale.o: x\n");
        assert_eq!(makefile(&text), none());
        let text = format!("{PHONY}all: prog\nprog: $(UNDEFINED)\nstale.o: x\n");
        assert_eq!(makefile(&text), none());
        let text = format!("{PHONY}all: prog\nOBJS = $(shell ls)\nprog: $(OBJS)\nstale.o: x\n");
        assert_eq!(makefile(&text), none());
        let text = format!("{PHONY}.SECONDEXPANSION:\nall: prog\nprog: $$@.o\nstale.o: x\n");
        assert_eq!(makefile(&text), none());
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
