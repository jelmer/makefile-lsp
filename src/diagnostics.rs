//! Diagnostics for Makefile files.

use std::collections::{HashMap, HashSet};

use makefile_lossless::{
    ConditionalBranch, Makefile, MakefileVariant, Modifier, Parse, ParseErrorKind, ParsedReference,
    PositionedParseError, ReferenceLocation, Rule, TextRange, VariableReference,
};
use tower_lsp_server::ls_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Range};

use crate::builtins;
use crate::dep_graph::mutually_exclusive;
use crate::position::text_range_to_lsp_range;
use crate::targets::targets_with_ranges;
use crate::workspace::{parsed_variant, FileSet, Resolution, ResolvedInclude};

fn make_diagnostic(
    range: Range,
    severity: DiagnosticSeverity,
    code: &str,
    message: String,
) -> Diagnostic {
    Diagnostic {
        range,
        severity: Some(severity),
        code: Some(NumberOrString::String(code.to_string())),
        source: Some("makefile-lsp".to_string()),
        message,
        ..Default::default()
    }
}

/// Symbols defined or used in the other makefiles of a file set, which
/// cross-file checks take into account to avoid false positives.
#[derive(Debug, Default)]
pub struct ExternalSymbols {
    variables_defined: HashSet<String>,
    variables_referenced: HashSet<String>,
    targets: HashSet<String>,
    /// Names used as prerequisites of ordinary (non-special) rules.
    prerequisites: HashSet<String>,
    phony: HashSet<String>,
    exports_all_variables: bool,
}

impl ExternalSymbols {
    pub fn add(&mut self, makefile: &Makefile) {
        self.variables_defined
            .extend(makefile.variable_definitions().filter_map(|v| v.name()));
        self.variables_referenced
            .extend(referenced_variables(makefile));
        self.exports_all_variables |= exports_all_variables(makefile);
        for rule in makefile.rules() {
            let targets: Vec<String> = rule.targets().collect();
            if targets.iter().any(|t| t == ".PHONY") {
                self.phony.extend(rule.prerequisites());
            }
            if targets.iter().any(|t| crate::dep_graph::is_graph_target(t)) {
                self.prerequisites.extend(rule.prerequisites());
            }
            self.targets.extend(targets);
        }
    }
}

/// Collect diagnostics for a single makefile.
///
/// `base_dir`, when provided, is used to resolve relative include paths for
/// the missing-include-file check. Pass `None` to skip filesystem-touching
/// checks (e.g. in tests where the makefile doesn't live on disk).
pub fn get_diagnostics(
    source_text: &str,
    parsed: &Parse<makefile_lossless::Makefile>,
    base_dir: Option<&std::path::Path>,
) -> Vec<Diagnostic> {
    let makefile = parsed.tree();
    let includes: Vec<ResolvedInclude> = match base_dir {
        Some(dir) => {
            use crate::workspace::{include_paths, resolve_include, LiteralVariables};

            let variant = parsed_variant(parsed);
            let mut vars = LiteralVariables::default();
            vars.add(&makefile, variant);
            include_paths(&makefile)
                .into_iter()
                .map(|path| {
                    let resolution =
                        resolve_include(&path, &vars, variant, Some(dir), Some(dir), &|p| {
                            p.is_file()
                        });
                    ResolvedInclude { path, resolution }
                })
                .collect()
        }
        None => Vec::new(),
    };
    collect_diagnostics(
        source_text,
        parsed,
        &ExternalSymbols::default(),
        &includes,
        OtherMakefiles::Unknown,
        base_dir,
    )
}

/// Collect diagnostics for the current document of a file set, taking the
/// definitions and uses in the other makefiles into account.
pub fn get_file_set_diagnostics(files: &FileSet) -> Vec<Diagnostic> {
    let others: Vec<(Makefile, MakefileVariant)> = files
        .others()
        .map(|doc| (doc.makefile(), doc.variant()))
        .collect();
    let mut external = ExternalSymbols::default();
    for (makefile, _) in &others {
        external.add(makefile);
    }
    let current = files.current();
    collect_diagnostics(
        current.text(),
        current.parsed(),
        &external,
        files.includes(),
        if files.is_complete() {
            OtherMakefiles::Complete(&others)
        } else {
            OtherMakefiles::Incomplete
        },
        current.dir(),
    )
}

/// The other makefiles of a file set, for checks that need to know all rules.
#[derive(Clone, Copy)]
enum OtherMakefiles<'a> {
    /// Includes weren't followed.
    Unknown,
    /// Every include in the file set was followed.
    Complete(&'a [(Makefile, MakefileVariant)]),
    /// Some include in the file set couldn't be followed.
    Incomplete,
}

fn collect_diagnostics(
    source_text: &str,
    parsed: &Parse<makefile_lossless::Makefile>,
    external: &ExternalSymbols,
    includes: &[ResolvedInclude],
    others: OtherMakefiles,
    base_dir: Option<&std::path::Path>,
) -> Vec<Diagnostic> {
    let mut diagnostics: Vec<Diagnostic> = parsed
        .positioned_errors()
        .iter()
        .map(|error| parse_error_diagnostic(source_text, error))
        .collect();

    let makefile = parsed.tree();
    let variant = parsed_variant(parsed);
    diagnostics.extend(check_undefined_variables(
        source_text,
        &makefile,
        variant,
        external,
    ));
    diagnostics.extend(check_recursive_variable_self_reference(
        source_text,
        &makefile,
    ));
    diagnostics.extend(check_empty_variable_references(source_text, &makefile));
    diagnostics.extend(check_self_dependency(source_text, &makefile));
    diagnostics.extend(check_circular_dependencies(source_text, &makefile));
    diagnostics.extend(check_duplicate_targets(source_text, &makefile));
    diagnostics.extend(check_mixed_rule_separators(source_text, &makefile));
    // nmake has no .PHONY: a target that is not a file is always built.
    let has_phony = variant != MakefileVariant::NMake;
    if has_phony {
        diagnostics.extend(check_missing_phony_targets(
            source_text,
            &makefile,
            external,
        ));
        diagnostics.extend(check_unused_phony_targets(source_text, &makefile, external));
    }
    diagnostics.extend(check_include_missing_path(source_text, &makefile));
    // BSD make strips trailing whitespace from variable values.
    if variant != MakefileVariant::BSDMake {
        diagnostics.extend(check_trailing_whitespace_in_value(source_text, &makefile));
    }
    diagnostics.extend(check_duplicate_prerequisites(source_text, &makefile));
    diagnostics.extend(check_redundant_transitive_prerequisites(
        source_text,
        &makefile,
    ));
    if variant == MakefileVariant::GNUMake {
        diagnostics.extend(check_shell_in_recursive_assignment(source_text, &makefile));
    }
    diagnostics.extend(check_empty_automatic_variables(source_text, &makefile));
    diagnostics.extend(check_unterminated_conditionals(source_text, &makefile));
    diagnostics.extend(check_malformed_conditions(source_text, &makefile));
    diagnostics.extend(check_unused_variables(source_text, &makefile, external));
    diagnostics.extend(check_mixed_assignment_operators(source_text, &makefile));
    if has_phony {
        diagnostics.extend(check_empty_rule_probably_phony(
            source_text,
            &makefile,
            external,
        ));
    }
    diagnostics.extend(check_include_files(source_text, includes));
    diagnostics.extend(check_automatic_variable_outside_recipe(
        source_text,
        &makefile,
        variant,
    ));
    if let Some(dir) = base_dir {
        if has_phony {
            diagnostics.extend(check_missing_phony(source_text, &makefile, external, dir));
        }
        diagnostics.extend(check_unresolved_prerequisites(
            source_text,
            &makefile,
            variant,
            others,
            dir,
        ));
    }

    diagnostics
}

/// Convert a parse error to a diagnostic.
///
/// Recipe lines indented with spaces and recipe lines outside any rule are
/// both parse errors, but common enough mistakes to get their own codes (and
/// quick fixes in code_actions).
fn parse_error_diagnostic(source_text: &str, error: &PositionedParseError) -> Diagnostic {
    if let Some(indent) = error.space_indent_range() {
        return make_diagnostic(
            text_range_to_lsp_range(source_text, indent),
            DiagnosticSeverity::ERROR,
            "spaces-instead-of-tab",
            "recipe lines must start with a tab, not spaces".to_string(),
        );
    }
    if error.kind() == ParseErrorKind::RecipeBeforeFirstTarget {
        return make_diagnostic(
            text_range_to_lsp_range(source_text, error.line_range()),
            DiagnosticSeverity::ERROR,
            "orphan-recipe-line",
            "recipe line is not attached to any target".to_string(),
        );
    }
    make_diagnostic(
        text_range_to_lsp_range(source_text, error.range),
        DiagnosticSeverity::ERROR,
        error.code.as_deref().unwrap_or("parse-error"),
        error.message.clone(),
    )
}

/// Check for references to undefined variables.
fn check_undefined_variables(
    source_text: &str,
    makefile: &Makefile,
    variant: MakefileVariant,
    external: &ExternalSymbols,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    let mut defined_vars: HashSet<String> = makefile
        .variable_definitions()
        .filter_map(|v| v.name())
        .collect();
    defined_vars.extend(external.variables_defined.iter().cloned());

    for var_ref in makefile.variable_references() {
        // TODO: also check recipes and define bodies. Variables there are
        // often set on the command line or in the environment, as DESTDIR
        // is, so that needs a way to tell those apart first.
        if crate::references::in_recipe(&var_ref) || crate::references::in_define_body(&var_ref) {
            continue;
        }
        let Some(name) = var_ref.name() else {
            continue;
        };
        let known = match variant {
            MakefileVariant::NMake => builtins::is_nmake_known_macro(&name),
            MakefileVariant::BSDMake => builtins::is_bsd_known_variable(&name),
            _ => builtins::is_known_variable(&name),
        };
        if known || defined_vars.contains(&name) {
            continue;
        }
        let range = text_range_to_lsp_range(source_text, var_ref.text_range());
        diagnostics.push(make_diagnostic(
            range,
            DiagnosticSeverity::WARNING,
            "undefined-variable",
            format!("variable '{}' is not defined", name),
        ));
    }

    diagnostics
}

/// Check for recursive variables that reference themselves, which causes infinite expansion.
///
/// Only flags `=` (recursively-expanded) assignments, since `:=`/`::=`/`:::=` expand
/// immediately and self-references are valid there (they refer to the previous value).
fn check_recursive_variable_self_reference(
    source_text: &str,
    makefile: &Makefile,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    for var_def in makefile.variable_definitions() {
        let Some(name) = var_def.name() else {
            continue;
        };
        let op = var_def.assignment_operator().unwrap_or_default();
        if op != "=" {
            continue;
        }

        for var_ref in var_def.value_references() {
            if var_ref.name().as_deref() == Some(&name) {
                let range = text_range_to_lsp_range(source_text, var_ref.text_range());
                diagnostics.push(make_diagnostic(
                    range,
                    DiagnosticSeverity::WARNING,
                    "recursive-variable-reference",
                    format!(
                        "variable '{}' references itself in a recursively-expanded definition",
                        name
                    ),
                ));
            }
        }
    }

    diagnostics
}

/// Special targets where duplicate definitions are expected (they accumulate prerequisites).
const ACCUMULATING_TARGETS: &[&str] = &[
    ".PHONY",
    ".SUFFIXES",
    ".PRECIOUS",
    ".INTERMEDIATE",
    ".SECONDARY",
    ".IGNORE",
    ".SILENT",
    ".NOTPARALLEL",
];

/// Rules that have a `:` separator.
///
/// A line that make rejects with "missing separator" (such as a
/// space-indented recipe) is parsed as a rule without one; its words are
/// not targets.
fn separated_rules(makefile: &Makefile) -> impl Iterator<Item = Rule> + '_ {
    makefile.rules().filter(|rule| rule.operator().is_some())
}

/// Check for duplicate target definitions.
///
/// In GNU Make, when the same target appears in multiple single-colon rules,
/// only the last one's recipe is used, which is almost always a mistake.
/// Double-colon rules (`::`) are intentionally excluded since they allow
/// multiple recipe blocks, as are rules in different branches of a
/// conditional, since only one of those takes effect.
fn check_duplicate_targets(source_text: &str, makefile: &Makefile) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let mut seen: HashMap<String, Vec<(Range, Vec<ConditionalBranch>)>> = HashMap::new();

    for rule in separated_rules(makefile) {
        let branches = rule.enclosing_branches();
        for (target, range) in targets_with_ranges(&rule) {
            // Skip pattern rules (contain %)
            if target.contains('%') {
                continue;
            }
            // Skip special targets that accumulate prerequisites
            if ACCUMULATING_TARGETS.contains(&target.as_str()) {
                continue;
            }
            // Skip double-colon rules (they intentionally allow multiple definitions)
            if rule.is_double_colon() {
                continue;
            }

            let target_range = text_range_to_lsp_range(source_text, range);

            let previous = seen.entry(target.clone()).or_default();
            let first = previous
                .iter()
                .find(|(_, b)| !mutually_exclusive(b, &branches));
            if let Some((first_range, _)) = first {
                diagnostics.push(make_diagnostic(
                    target_range,
                    DiagnosticSeverity::WARNING,
                    "duplicate-target",
                    format!(
                        "target '{}' already defined on line {}",
                        target,
                        first_range.start.line + 1
                    ),
                ));
            } else {
                previous.push((target_range, branches.clone()));
            }
        }
    }

    diagnostics
}

/// Check for empty variable references like `$()` or `${}`.
fn check_empty_variable_references(source_text: &str, makefile: &Makefile) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    for var_ref in makefile.variable_references() {
        if var_ref.name().is_none() {
            let range = text_range_to_lsp_range(source_text, var_ref.text_range());
            diagnostics.push(make_diagnostic(
                range,
                DiagnosticSeverity::WARNING,
                "empty-variable-reference",
                "empty variable reference".to_string(),
            ));
        }
    }

    diagnostics
}

/// Check for targets that list themselves as prerequisites.
fn check_self_dependency(source_text: &str, makefile: &Makefile) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    for rule in makefile.rules() {
        let targets: Vec<String> = rule.targets().collect();
        for prereq in rule.prerequisites() {
            if targets.contains(&prereq) {
                // Find the prerequisite position within the PREREQUISITES node
                let rule_range = text_range_to_lsp_range(source_text, rule.text_range());
                diagnostics.push(make_diagnostic(
                    rule_range,
                    DiagnosticSeverity::WARNING,
                    "self-dependency",
                    format!("target '{}' lists itself as a prerequisite", prereq),
                ));
            }
        }
    }

    diagnostics
}

/// Check for circular dependencies in the target graph.
///
/// Each cycle is reported once, anchored on the rule of the first target in
/// its canonical rotation. Self-loops are left to `check_self_dependency`.
fn check_circular_dependencies(source_text: &str, makefile: &Makefile) -> Vec<Diagnostic> {
    let graph = crate::dep_graph::DependencyGraph::from_makefile(makefile);
    let cycles = graph.find_cycles();
    if cycles.is_empty() {
        return Vec::new();
    }

    // Map each target in a reported cycle to a rule range to anchor on.
    let mut target_range: HashMap<String, Range> = HashMap::new();
    for rule in makefile.rules() {
        let rule_range = text_range_to_lsp_range(source_text, rule.text_range());
        for target in rule.targets() {
            target_range.entry(target).or_insert(rule_range);
        }
    }

    cycles
        .into_iter()
        .filter_map(|cycle| {
            let range = *target_range.get(&cycle[0])?;
            let mut display = cycle.clone();
            display.push(cycle[0].clone());
            Some(make_diagnostic(
                range,
                DiagnosticSeverity::WARNING,
                "circular-dependency",
                format!("circular dependency: {}", display.join(" -> ")),
            ))
        })
        .collect()
}

/// Check for `.PHONY` prerequisites that are never defined as targets.
fn check_missing_phony_targets(
    source_text: &str,
    makefile: &Makefile,
    external: &ExternalSymbols,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    let mut defined_targets: HashSet<String> = makefile
        .rules()
        .flat_map(|r| r.targets().collect::<Vec<_>>())
        .collect();
    defined_targets.extend(external.targets.iter().cloned());

    for rule in makefile.rules_by_target(".PHONY") {
        let rule_range = text_range_to_lsp_range(source_text, rule.text_range());
        for prereq in rule.prerequisites() {
            if !defined_targets.contains(&prereq) {
                diagnostics.push(make_diagnostic(
                    rule_range,
                    DiagnosticSeverity::WARNING,
                    "undefined-phony-target",
                    format!(
                        "target '{}' is declared .PHONY but is never defined",
                        prereq
                    ),
                ));
            }
        }
    }

    diagnostics
}

/// Check for `.PHONY` targets that nothing references.
///
/// Two related diagnostics:
///
/// * `unused-phony-target` (warning): the target is declared `.PHONY`, has a
///   rule with no recipe and no prerequisites, AND nothing depends on it.
///   High-confidence typo or stale declaration — there's nothing the rule
///   could do and no caller wants it.
/// * `unreferenced-phony-target` (hint): the target is declared `.PHONY` and
///   has a real rule, but nothing depends on it AND the name isn't a
///   conventional entry point (`all`, `install`, …). The author may have
///   forgotten to wire it into a top-level target like `all`.
///
/// Self-references don't count as incoming edges. Targets that don't appear
/// as the head of any rule are handled by `check_missing_phony_targets`.
fn check_unused_phony_targets(
    source_text: &str,
    makefile: &Makefile,
    external: &ExternalSymbols,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    let graph = crate::dep_graph::DependencyGraph::from_makefile(makefile);

    // Collect every name declared in any `.PHONY:` rule, keyed to that rule's
    // range so the diagnostic anchors on the declaration site.
    let mut phony_decls: HashMap<String, Range> = HashMap::new();
    for rule in makefile.rules_by_target(".PHONY") {
        let range = text_range_to_lsp_range(source_text, rule.text_range());
        for name in rule.prerequisites() {
            phony_decls.entry(name).or_insert(range);
        }
    }

    // Index target rules so we can ask "does this name have a rule, and is
    // that rule empty?" without rescanning. A name with multiple rules is
    // empty only if *every* rule is empty (any non-empty rule means there's
    // real work attached).
    let mut has_rule: HashSet<String> = HashSet::new();
    let mut nonempty_rule: HashSet<String> = HashSet::new();
    for rule in makefile.rules() {
        let prereqs: Vec<String> = rule.prerequisites().collect();
        let has_recipe = rule.recipe_nodes().next().is_some();
        for target in rule.targets() {
            has_rule.insert(target.clone());
            // Self-prerequisites don't count as real work — they can't build
            // anything and are flagged separately by check_self_dependency.
            let has_real_prereqs = prereqs.iter().any(|p| p != &target);
            if has_recipe || has_real_prereqs {
                nonempty_rule.insert(target);
            }
        }
    }

    let mut names: Vec<&String> = phony_decls.keys().collect();
    names.sort();

    for name in names {
        if !has_rule.contains(name) {
            // No rule at all -> handled by check_missing_phony_targets.
            continue;
        }
        let referenced =
            graph.referrers(name).any(|r| r != name) || external.prerequisites.contains(name);
        if referenced {
            continue;
        }

        let range = phony_decls[name];
        if !nonempty_rule.contains(name) {
            diagnostics.push(make_diagnostic(
                range,
                DiagnosticSeverity::WARNING,
                "unused-phony-target",
                format!(
                    "'.PHONY' target '{}' has no recipe and nothing depends on it",
                    name
                ),
            ));
        } else if !crate::dep_graph::is_conventional_entry_point(name) {
            diagnostics.push(make_diagnostic(
                range,
                DiagnosticSeverity::HINT,
                "unreferenced-phony-target",
                format!("nothing depends on '.PHONY' target '{}'", name),
            ));
        }
    }

    diagnostics
}

/// Check for `include` directives with missing paths.
fn check_include_missing_path(source_text: &str, makefile: &Makefile) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    for inc in makefile.includes() {
        let path = inc.path().unwrap_or_default();
        if path.is_empty() {
            let range = text_range_to_lsp_range(source_text, inc.text_range());
            diagnostics.push(make_diagnostic(
                range,
                DiagnosticSeverity::ERROR,
                "include-missing-path",
                "include directive has no file path".to_string(),
            ));
        }
    }

    diagnostics
}

/// Check for automatic variables that expand to empty in their context.
///
/// `$<`, `$^`, `$+`, `$?` all expand to (part of) the prerequisite list, so
/// they're empty in a rule with no prerequisites. `$*` expands to the stem of
/// a pattern rule, so it's empty in a non-pattern rule. The same goes for
/// their `D` and `F` forms, such as `$(<D)`.
fn check_empty_automatic_variables(source_text: &str, makefile: &Makefile) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    for rule in makefile.rules() {
        let has_prereqs = rule.prerequisites().next().is_some();
        let is_pattern = rule.targets().any(|t| t.contains('%'));

        if has_prereqs && is_pattern {
            continue;
        }

        for var_ref in rule.recipe_nodes().flat_map(|recipe| recipe.references()) {
            let Some(name) = var_ref.name() else {
                continue;
            };
            let mut chars = name.chars();
            let Some(var) = chars.next() else {
                continue;
            };
            if !matches!(chars.as_str(), "" | "D" | "F") {
                continue;
            }
            let reason = match var {
                '<' | '^' | '+' | '?' if !has_prereqs => "rule has no prerequisites",
                '*' if !is_pattern => "non-pattern rule",
                _ => continue,
            };
            let shown = if name.len() == 1 {
                format!("${}", name)
            } else {
                format!("$({})", name)
            };
            diagnostics.push(make_diagnostic(
                text_range_to_lsp_range(source_text, var_ref.text_range()),
                DiagnosticSeverity::WARNING,
                "empty-automatic-variable",
                format!("{} expands to empty: {}", shown, reason),
            ));
        }
    }

    diagnostics
}

/// Collect the names of all variables a makefile references.
fn referenced_variables(makefile: &Makefile) -> HashSet<String> {
    let mut referenced: HashSet<String> = crate::references::variable_references(makefile)
        .into_iter()
        .map(|(name, _)| name)
        .collect();

    // `ifdef NAME`, `.if defined(NAME)` and the like reference NAME but
    // don't show up as references because NAME is a bare identifier.
    for cond in makefile.all_conditionals() {
        for branch in cond.branches() {
            referenced.extend(branch.tested_variables().into_iter().map(|(name, _)| name));
        }
    }

    // `export NAME` passes NAME to recipe environments.
    for var_def in makefile.variable_definitions() {
        if var_def.is_export() {
            referenced.extend(var_def.names());
        }
    }

    referenced
}

/// Whether a bare `export` or `.EXPORT_ALL_VARIABLES:` exports every variable.
fn exports_all_variables(makefile: &Makefile) -> bool {
    makefile
        .variable_definitions()
        .any(|v| v.is_export() && v.names().next().is_none())
        || makefile
            .rules()
            .any(|r| r.targets().any(|t| t == ".EXPORT_ALL_VARIABLES"))
}

/// Check for variables that are defined but never referenced.
///
/// Emits a hint (not a warning) to keep the noise low: the check has known
/// false-positive sources (`$(eval)` and `$(call)` can reference variables in
/// ways we can't see statically) and many makefiles intentionally export
/// variables for sub-makes or external tooling.
///
/// Skipped:
/// - Variables exported with `export` (consumed outside the makefile),
///   including all variables if a bare `export` or `.EXPORT_ALL_VARIABLES`
///   is present.
/// - Variables overriding a builtin (would change make's behaviour).
/// - Variables defined inside conditionals (often configuration toggles).
fn check_unused_variables(
    source_text: &str,
    makefile: &Makefile,
    external: &ExternalSymbols,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    if external.exports_all_variables || exports_all_variables(makefile) {
        return diagnostics;
    }
    let referenced = referenced_variables(makefile);

    for var_def in makefile.variable_definitions() {
        let Some(name) = var_def.name() else {
            continue;
        };
        if referenced.contains(&name) || external.variables_referenced.contains(&name) {
            continue;
        }
        // `unexport NAME` doesn't define NAME.
        if var_def.is_unexport() {
            continue;
        }
        // `undefine NAME` doesn't define NAME. It doesn't read it either, so
        // it doesn't count as a use of an earlier definition.
        if var_def.is_undefine() {
            continue;
        }
        if builtins::is_known_variable(&name) {
            continue;
        }
        // Inside a conditional? Skip — likely a configuration toggle.
        if !var_def.enclosing_branches().is_empty() {
            continue;
        }

        let name_range = var_def.name_range().unwrap_or_else(|| var_def.text_range());

        let range = text_range_to_lsp_range(source_text, name_range);
        diagnostics.push(make_diagnostic(
            range,
            DiagnosticSeverity::HINT,
            "unused-variable",
            format!("variable '{}' is defined but never used", name),
        ));
    }

    diagnostics
}

fn is_valid_var_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
}

/// Check for `include` directives whose file doesn't exist or can't be read.
///
/// Optional includes (`-include` / `sinclude`) explicitly tolerate a missing
/// file. Names that can't be resolved statically (`include $(shell ...)`)
/// are skipped.
fn check_include_files(source_text: &str, includes: &[ResolvedInclude]) -> Vec<Diagnostic> {
    includes
        .iter()
        .filter_map(|inc| {
            let range = text_range_to_lsp_range(source_text, inc.path.range);
            match &inc.resolution {
                Resolution::Missing(_) if !inc.path.optional => Some(make_diagnostic(
                    range,
                    DiagnosticSeverity::WARNING,
                    "missing-include-file",
                    format!("included file '{}' does not exist", inc.path.name),
                )),
                Resolution::Unreadable(path, error) => Some(make_diagnostic(
                    range,
                    DiagnosticSeverity::WARNING,
                    "unreadable-include-file",
                    format!(
                        "included file '{}' could not be read: {}",
                        path.display(),
                        error
                    ),
                )),
                _ => None,
            }
        })
        .collect()
}

/// Check for rules with no prerequisites and no recipe lines — these are
/// often phony declarations the author forgot to mark with `.PHONY`.
///
/// Hint-level: a regular target that already exists as a file is fine to
/// declare empty (it relies on the file's existence). The hint nudges the
/// common case where the author meant to make it phony.
fn check_empty_rule_probably_phony(
    source_text: &str,
    makefile: &Makefile,
    external: &ExternalSymbols,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    for rule in separated_rules(makefile) {
        // Skip rules with prerequisites — they're meta-targets, not phony candidates.
        if rule.prerequisites().next().is_some() {
            continue;
        }
        // Skip rules with any recipe lines.
        if rule.recipe_nodes().next().is_some() {
            continue;
        }
        let targets: Vec<String> = rule.targets().collect();
        if targets.is_empty() {
            continue;
        }
        for target in &targets {
            // Special targets, pattern rules, and already-phony targets are fine empty.
            if target.starts_with('.') || target.contains('%') {
                continue;
            }
            if makefile.is_phony(target) || external.phony.contains(target) {
                continue;
            }

            let rule_range = text_range_to_lsp_range(source_text, rule.text_range());
            diagnostics.push(make_diagnostic(
                rule_range,
                DiagnosticSeverity::HINT,
                "empty-rule-probably-phony",
                format!(
                    "target '{}' has no prerequisites and no recipe; \
                     declare it with .PHONY if it's not a file",
                    target
                ),
            ));
        }
    }

    diagnostics
}

/// Check for the same variable being assigned with both `=` (recursive) and
/// `:=`/`::=`/`:::=` (immediate) flavours.
///
/// These flavours have different evaluation semantics; mixing them means the
/// later assignment silently wins and changes how earlier-referencing code
/// behaves. `+=` (append) and `?=` (conditional) are not flagged — they're
/// normal companions to either flavour. Assignments in different branches of
/// a conditional never both take effect, so they do not mix.
fn check_mixed_assignment_operators(source_text: &str, makefile: &Makefile) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    // For each name, collect (flavour, range, branches) for each
    // non-`+=`/`?=` assignment. Compare flavours within each name.
    #[derive(PartialEq)]
    enum Flavour {
        Recursive, // `=`
        Immediate, // `:=`, `::=`, `:::=`
    }
    let classify = |op: &str| match op {
        "=" => Some(Flavour::Recursive),
        ":=" | "::=" | ":::=" => Some(Flavour::Immediate),
        _ => None,
    };

    let mut by_name: HashMap<String, Vec<(Flavour, Range, Vec<ConditionalBranch>)>> =
        HashMap::new();
    for var_def in makefile.variable_definitions() {
        let Some(name) = var_def.name() else { continue };
        let Some(op) = var_def.assignment_operator() else {
            continue;
        };
        let Some(flavour) = classify(&op) else {
            continue;
        };
        let range = text_range_to_lsp_range(source_text, var_def.text_range());
        let branches = var_def.enclosing_branches();
        by_name
            .entry(name)
            .or_default()
            .push((flavour, range, branches));
    }

    for (name, assignments) in by_name {
        // Flag every assignment that mixes with another one that can take
        // effect alongside it, so the user sees each problem assignment.
        for (flavour, range, branches) in &assignments {
            let mixed = assignments
                .iter()
                .any(|(f, _, b)| f != flavour && !mutually_exclusive(b, branches));
            if !mixed {
                continue;
            }
            diagnostics.push(make_diagnostic(
                *range,
                DiagnosticSeverity::WARNING,
                "mixed-assignment-operators",
                format!(
                    "variable '{}' is assigned with both `=` and `:=`; the later assignment silently wins",
                    name
                ),
            ));
        }
    }
    diagnostics.sort_by_key(|d| d.range.start);

    diagnostics
}

/// Check for conditional blocks that are missing their `endif`.
///
/// Bare `else` or `endif` outside a conditional are already reported by the
/// parser as "unknown conditional directive". The case the parser silently
/// accepts is an `ifdef`/`ifeq` that runs to end-of-file without a matching
/// `endif`.
fn check_unterminated_conditionals(source_text: &str, makefile: &Makefile) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    for cond in makefile.all_conditionals() {
        // Skip orphans that don't even have a recognized opener — the parser
        // already complains about those (e.g. bare `else`/`endif`).
        if cond.conditional_type().is_none() {
            continue;
        }
        if cond.has_endif() {
            continue;
        }

        // Point at the opening directive for clarity.
        let opener_range = cond
            .branches()
            .next()
            .map(|b| b.directive_line_range())
            .unwrap_or_else(|| cond.text_range());

        let range = text_range_to_lsp_range(source_text, opener_range);
        let kind = cond.conditional_type().unwrap_or_default();
        diagnostics.push(make_diagnostic(
            range,
            DiagnosticSeverity::ERROR,
            "unterminated-conditional",
            format!("'{}' is missing a matching 'endif'", kind),
        ));
    }

    diagnostics
}

/// Check for BSD make `.if` and nmake `!IF` conditions that don't parse.
fn check_malformed_conditions(source_text: &str, makefile: &Makefile) -> Vec<Diagnostic> {
    makefile
        .all_conditionals()
        .flat_map(|cond| cond.branches().collect::<Vec<_>>())
        .filter_map(|branch| {
            let message = match (branch.bsd_condition(), branch.nmake_condition()) {
                (Some(Err(error)), _) => error.message,
                (_, Some(Err(error))) => error.message,
                _ => return None,
            };
            let range = branch.condition_range()?;
            // The parser already reports a missing condition.
            if range.is_empty() {
                return None;
            }
            Some(make_diagnostic(
                text_range_to_lsp_range(source_text, range),
                DiagnosticSeverity::ERROR,
                "malformed-condition",
                format!("malformed condition: {message}"),
            ))
        })
        .collect()
}

/// Check for `$(shell ...)` inside a recursively-expanded (`=`) assignment.
///
/// With `=`, the shell command is re-executed every time the variable is
/// expanded — once per recipe line that mentions it, often many times. Using
/// `:=` (or `::=`) runs it once at parse time. This is both a performance trap
/// and a correctness trap when the shell command has side effects or its
/// output changes between invocations.
fn check_shell_in_recursive_assignment(source_text: &str, makefile: &Makefile) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    for var_def in makefile.variable_definitions() {
        let op = var_def.assignment_operator().unwrap_or_default();
        if op != "=" {
            continue;
        }

        for var_ref in var_def.value_references() {
            if !var_ref.is_function_call() {
                continue;
            }
            if var_ref.name().as_deref() != Some("shell") {
                continue;
            }
            let range = text_range_to_lsp_range(source_text, var_ref.text_range());
            diagnostics.push(make_diagnostic(
                range,
                DiagnosticSeverity::WARNING,
                "shell-in-recursive-assignment",
                "$(shell ...) in a recursively-expanded (=) variable is re-run \
                 on every expansion; use := to run it once"
                    .to_string(),
            ));
        }
    }

    diagnostics
}

/// Check for targets that have both single-colon and double-colon rules.
///
/// GNU Make refuses to run such a makefile ("target file 'x' has both : and
/// :: entries"). Pattern rules are exempt, as `::` there marks a terminal
/// rule rather than a separate kind of entry. Rules in different branches
/// of a conditional never both take effect, so they do not conflict.
fn check_mixed_rule_separators(source_text: &str, makefile: &Makefile) -> Vec<Diagnostic> {
    struct Seen {
        double_colon: bool,
        line: u32,
        branches: Vec<ConditionalBranch>,
    }

    let mut diagnostics = Vec::new();
    let mut seen: HashMap<String, Vec<Seen>> = HashMap::new();

    // A line without a separator is a parse error, not a rule.
    for rule in makefile.rules().filter(|r| r.operator().is_some()) {
        let double_colon = rule.is_double_colon();
        let branches = rule.enclosing_branches();
        for (target, range) in targets_with_ranges(&rule) {
            if target.contains('%') {
                continue;
            }
            let range = text_range_to_lsp_range(source_text, range);
            let previous = seen.entry(target.clone()).or_default();
            let conflict = previous.iter().find(|p| {
                p.double_colon != double_colon && !mutually_exclusive(&p.branches, &branches)
            });
            if let Some(first) = conflict {
                let separator = |dc| if dc { "::" } else { ":" };
                diagnostics.push(make_diagnostic(
                    range,
                    DiagnosticSeverity::ERROR,
                    "mixed-rule-separator",
                    format!(
                        "target '{}' has both : and :: rules (first defined with '{}' on line {})",
                        target,
                        separator(first.double_colon),
                        first.line + 1
                    ),
                ));
            }
            previous.push(Seen {
                double_colon,
                line: range.start.line,
                branches: branches.clone(),
            });
        }
    }

    diagnostics
}

/// Check for duplicate prerequisites within a single rule.
///
/// `foo: a b a` is harmless but always a mistake — the duplicate adds no
/// information and usually means the author intended a different name.
fn check_duplicate_prerequisites(source_text: &str, makefile: &Makefile) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    for rule in makefile.rules() {
        let prereqs: Vec<String> = rule.prerequisites().collect();
        let mut seen: HashSet<&str> = HashSet::new();
        let mut reported: HashSet<&str> = HashSet::new();
        for prereq in &prereqs {
            if !seen.insert(prereq.as_str()) && reported.insert(prereq.as_str()) {
                let rule_range = text_range_to_lsp_range(source_text, rule.text_range());
                diagnostics.push(make_diagnostic(
                    rule_range,
                    DiagnosticSeverity::WARNING,
                    "duplicate-prerequisite",
                    format!("prerequisite '{}' is listed more than once", prereq),
                ));
            }
        }
    }

    diagnostics
}

/// Check for prerequisites that are already reachable transitively via another
/// prerequisite of the same rule.
///
/// `all: lib main` plus `main: lib` makes `lib` redundant in `all`'s list:
/// requesting `main` already pulls in `lib`. Removing it doesn't change build
/// order (make resolves the transitive closure anyway) but does eliminate a
/// confusing coupling. Hint-level: the duplication is sometimes intentional
/// when the author wants the explicit documentation.
///
/// Skips:
/// * the rule's own targets (handled by `check_self_dependency`)
/// * exact-duplicate prereqs (handled by `check_duplicate_prerequisites`)
/// * prereqs that aren't themselves defined targets
fn check_redundant_transitive_prerequisites(
    source_text: &str,
    makefile: &Makefile,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let graph = crate::dep_graph::DependencyGraph::from_makefile(makefile);

    for rule in makefile.rules() {
        let targets: HashSet<String> = rule.targets().collect();
        let prereqs: Vec<String> = rule.prerequisites().collect();
        if prereqs.len() < 2 {
            continue;
        }
        let branches = rule.enclosing_branches();

        let mut reported: HashSet<&str> = HashSet::new();
        for prereq in &prereqs {
            if targets.contains(prereq) || !reported.insert(prereq.as_str()) {
                continue;
            }
            // Reachable via any *other* prereq in this rule?
            let via = prereqs.iter().find(|other| {
                *other != prereq && graph.reachable_from(other, &branches).contains(prereq)
            });
            if let Some(via) = via {
                let rule_range = text_range_to_lsp_range(source_text, rule.text_range());
                diagnostics.push(make_diagnostic(
                    rule_range,
                    DiagnosticSeverity::HINT,
                    "redundant-prerequisite",
                    format!(
                        "prerequisite '{}' is already pulled in transitively via '{}'",
                        prereq, via
                    ),
                ));
            }
        }
    }

    diagnostics
}

/// Check for trailing whitespace in variable assignment values.
///
/// In GNU Make, trailing whitespace is part of the variable's value (up to the
/// `#` comment or end of line). This is almost always unintentional and a
/// classic source of bugs (e.g. comparing `$(FOO)` to `"bar"` silently fails).
fn check_trailing_whitespace_in_value(source_text: &str, makefile: &Makefile) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    for var_def in makefile.variable_definitions() {
        let Some(ws_range) = var_def.trailing_value_whitespace_range() else {
            continue;
        };

        let range = text_range_to_lsp_range(source_text, ws_range);
        diagnostics.push(make_diagnostic(
            range,
            DiagnosticSeverity::WARNING,
            "trailing-whitespace-in-value",
            "trailing whitespace is included in the variable value".to_string(),
        ));
    }

    diagnostics
}

/// The names of the plain (non-expression) targets of `rule`, with their
/// ranges.
pub fn target_name_ranges(rule: &makefile_lossless::Rule) -> Vec<(String, TextRange)> {
    targets_with_ranges(rule)
        .into_iter()
        .filter(|(name, _)| !name.contains('$'))
        .collect()
}

/// Is `name` a target that by convention is never a file?
///
/// Builds on the conventional entry points, leaving out `build`, `doc`,
/// `docs`, `release` and `tests`, which are often real directories.
fn is_conventional_non_file_target(name: &str) -> bool {
    const EXTRA: &[&str] = &[
        "coverage",
        "fmt",
        "format",
        "installcheck",
        "maintainer-clean",
        "mostlyclean",
    ];
    let is_directory_like = matches!(name, "build" | "doc" | "docs" | "release" | "tests");
    (crate::dep_graph::is_conventional_entry_point(name) && !is_directory_like)
        || EXTRA.contains(&name)
}

/// Check for targets that by convention aren't files (`clean`, `install`,
/// ...) but aren't declared `.PHONY`.
///
/// Without `.PHONY`, a file of that name makes the target appear up to date
/// and its recipe silently stops running. Conservative to avoid noise:
///
/// - Targets for which a file exists next to the makefile are skipped.
/// - Rules without prerequisites and recipe are left to
///   `empty-rule-probably-phony`.
/// - Targets declared `.PHONY` in another makefile of the file set are
///   skipped.
/// - The check is skipped entirely if `.PHONY` lists a variable reference,
///   or if the makefile includes others and declares nothing `.PHONY`
///   itself, since the declarations may then live elsewhere.
fn check_missing_phony(
    source_text: &str,
    makefile: &Makefile,
    external: &ExternalSymbols,
    base_dir: &std::path::Path,
) -> Vec<Diagnostic> {
    let phony_prereqs: Vec<String> = makefile
        .rules_by_target(".PHONY")
        .flat_map(|r| r.prerequisites().collect::<Vec<_>>())
        .collect();
    if phony_prereqs
        .iter()
        .chain(&external.phony)
        .any(|p| p.contains('$'))
    {
        return Vec::new();
    }
    if phony_prereqs.is_empty() && makefile.includes().next().is_some() {
        return Vec::new();
    }

    let mut seen = HashSet::new();
    let mut diagnostics = Vec::new();
    for rule in makefile.rules() {
        let is_empty =
            rule.prerequisites().next().is_none() && rule.recipe_nodes().next().is_none();
        if is_empty {
            continue;
        }
        for (name, range) in target_name_ranges(&rule) {
            if !is_conventional_non_file_target(&name)
                || makefile.is_phony(&name)
                || external.phony.contains(&name)
                || base_dir.join(&name).exists()
                || !seen.insert(name.clone())
            {
                continue;
            }
            diagnostics.push(make_diagnostic(
                text_range_to_lsp_range(source_text, range),
                DiagnosticSeverity::HINT,
                "missing-phony",
                format!(
                    "target '{}' is conventionally not a file; declare it .PHONY",
                    name
                ),
            ));
        }
    }
    diagnostics
}

/// Check for prerequisites that are neither a target nor an existing file.
///
/// make fails with "No rule to make target" for these. Since the makefile
/// is never evaluated, only clear cases are flagged:
///
/// - Prerequisites containing `$`, `%`, glob characters, backslashes or
///   archive members are skipped, as are prerequisites of special targets.
/// - A prerequisite is resolved if it is an explicit target, matches a
///   pattern rule target or is declared `.PHONY` (in this makefile or in
///   `others`), or exists relative to the makefile's directory.
/// - GNU make's built-in rules can make a file from another one with the
///   same stem (`foo.o` or `foo` from `foo.c`), so a prerequisite is also
///   resolved if any file with its stem and some extension exists.
/// - `-lNAME` prerequisites are skipped, as make looks them up as
///   libraries, and so are `~` paths.
/// - The check is skipped entirely when any of the makefiles could get
///   rules or files from elsewhere: `include` (unless `others` is complete),
///   `vpath`/`VPATH`, `$(eval)` (also in a recipe) or a line that expands
///   to makefile text, a `.DEFAULT` rule, or a target name that isn't a
///   plain variable with a literal value.
///
/// TODO: a fragment that is included from, or run with `make -f` from,
/// another directory resolves its paths relative to that directory, and a
/// fragment opened without its includer may rely on targets defined there.
/// Resolve paths relative to the top-level makefile and only check fragments
/// whose includer is known.
fn check_unresolved_prerequisites(
    source_text: &str,
    makefile: &Makefile,
    variant: MakefileVariant,
    others: OtherMakefiles,
    base_dir: &std::path::Path,
) -> Vec<Diagnostic> {
    let (includes_followed, others) = match others {
        OtherMakefiles::Unknown => (false, &[][..]),
        OtherMakefiles::Complete(others) => (true, others),
        OtherMakefiles::Incomplete => return Vec::new(),
    };
    let Some(mut targets) = resolvable_target_names(makefile, variant, includes_followed) else {
        return Vec::new();
    };
    for (other, other_variant) in others {
        let Some(names) = resolvable_target_names(other, *other_variant, true) else {
            return Vec::new();
        };
        targets.extend(names);
    }
    let makefiles: Vec<&Makefile> = std::iter::once(makefile)
        .chain(others.iter().map(|(other, _)| other))
        .collect();

    let mut diagnostics = Vec::new();
    for rule in makefile.rules() {
        // Special targets and suffix rules; `.stamp/foo` style paths are fine.
        if rule
            .targets()
            .any(|t| t.starts_with('.') && !t.contains('/'))
        {
            continue;
        }
        for range in rule
            .prerequisite_ranges()
            .chain(rule.order_only_prerequisite_ranges())
        {
            // The name as written, so that escaped names are skipped below.
            let name = source_text[range].trim_start_matches("./");
            // `-lNAME` is searched for in the linker's library path, and
            // `~` is expanded to a home directory.
            if name.is_empty()
                || name.starts_with("-l")
                || name.starts_with('~')
                || name.contains(['$', '%', '*', '?', '[', '\\', '('])
                || targets.contains(name)
                || makefiles
                    .iter()
                    .any(|m| m.find_rule_by_target_pattern(name).is_some() || m.is_phony(name))
                || base_dir.join(name).exists()
                || has_file_with_same_stem(&base_dir.join(name))
            {
                continue;
            }
            diagnostics.push(make_diagnostic(
                text_range_to_lsp_range(source_text, range),
                DiagnosticSeverity::WARNING,
                "unresolved-prerequisite",
                format!(
                    "no rule to make prerequisite '{}', and no such file exists",
                    name
                ),
            ));
        }
    }
    diagnostics
}

/// The names of all explicit targets in `makefile`, or `None` if rules or
/// files may come from somewhere we can't see. Include directives are only
/// taken to hide rules if `includes_followed` is false.
///
/// Targets that are a single reference to a variable whose values are
/// literal, like `$(PROG)`, are expanded.
fn resolvable_target_names(
    makefile: &Makefile,
    variant: MakefileVariant,
    includes_followed: bool,
) -> Option<HashSet<String>> {
    let defers_elsewhere = (!includes_followed && makefile.includes().next().is_some())
        || makefile.vpaths().next().is_some()
        // A line of only references is parsed as makefile text once expanded.
        || makefile.expression_statements().any(|stmt| {
            !stmt
                .references()
                .next()
                .and_then(|r| r.name())
                .is_some_and(|name| matches!(name.as_str(), "info" | "warning" | "error"))
        })
        || makefile
            .variable_references()
            .any(|r| r.name().as_deref() == Some("eval"));
    if defers_elsewhere
        || makefile
            .variable_definitions_by_name("VPATH")
            .next()
            .is_some()
        || makefile.rules_by_target(".DEFAULT").next().is_some()
    {
        return None;
    }

    let mut names = HashSet::new();
    for target in makefile
        .rules()
        .flat_map(|r| r.targets().collect::<Vec<_>>())
    {
        if !target.contains('$') {
            names.insert(target.trim_start_matches("./").to_string());
            continue;
        }
        let var = ParsedReference::parse(&target, variant)
            .ok()
            .filter(|r| r.modifiers.is_empty() && is_valid_var_name(&r.name))?
            .name;
        let mut defined = false;
        for def in makefile.variable_definitions_by_name(&var) {
            let value = def.raw_value()?;
            if value.contains('$') {
                return None;
            }
            names.extend(
                value
                    .split_whitespace()
                    .map(|v| v.trim_start_matches("./").to_string()),
            );
            defined = true;
        }
        if !defined {
            return None;
        }
    }
    Some(names)
}

/// Does a file with the same stem as `path` but some extension exist?
fn has_file_with_same_stem(path: &std::path::Path) -> bool {
    let (Some(dir), Some(stem)) = (path.parent(), path.file_stem()) else {
        return false;
    };
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
        Err(e) => {
            // Err on the side of not flagging anything.
            tracing::warn!("unable to list {}: {}", dir.display(), e);
            return true;
        }
    };
    for entry in entries {
        let entry_path = match entry {
            Ok(entry) => entry.path(),
            Err(e) => {
                tracing::warn!("unable to list {}: {}", dir.display(), e);
                return true;
            }
        };
        if entry_path.file_stem() == Some(stem) && entry_path.extension().is_some() {
            return true;
        }
    }
    false
}

/// Check for automatic variables (`$@`, `$<`, `$(@D)`, ...) used where make
/// never sets them.
///
/// Automatic variables only have a value while a recipe is expanded, or
/// during the second expansion of prerequisites when `.SECONDEXPANSION` is
/// in effect. Text that is expanded immediately while the makefile is read
/// sees them as empty, so these are flagged:
///
/// - targets of a rule
/// - prerequisites, unless `.SECONDEXPANSION` appears anywhere in the file
/// - values of global `:=`, `::=`, `:::=` and `!=` assignments
/// - `ifeq`/`ifneq` conditions
///
/// Recursive (`=`, `?=`) and appended (`+=`) values, target-specific
/// variables and `define` bodies may be expanded later in a recipe, and
/// arguments to `$(eval)` and `$(call)` usually build text that is, so those
/// are left alone.
///
/// BSD make sets `$@` (`.TARGET`), `$*` (`.PREFIX`) and `$%` (`.MEMBER`) in
/// prerequisites too, and leaves references to undefined variables in a
/// `:=` assignment unexpanded.
fn check_automatic_variable_outside_recipe(
    source_text: &str,
    makefile: &Makefile,
    variant: MakefileVariant,
) -> Vec<Diagnostic> {
    let second_expansion = makefile
        .rules_by_target(".SECONDEXPANSION")
        .next()
        .is_some();

    makefile
        .variable_references()
        .filter_map(|var_ref| {
            let text = var_ref.to_string();
            let name = automatic_variable_name(&text, variant)?;
            // TODO: nmake's `$$@` is the target on a dependency line, but its
            // reference can't be told apart from a plain `$@` yet.
            let set_in_prerequisites = second_expansion
                || variant == MakefileVariant::NMake
                || (variant == MakefileVariant::BSDMake && matches!(name, '@' | '*' | '%'));
            let context = immediate_expansion_context(&var_ref, set_in_prerequisites, variant)?;
            Some(make_diagnostic(
                text_range_to_lsp_range(source_text, var_ref.text_range()),
                DiagnosticSeverity::WARNING,
                "automatic-variable-outside-recipe",
                format!(
                    "automatic variable '{}' is only set in recipes and is empty in {}",
                    text, context
                ),
            ))
        })
        .collect()
}

/// If `text` is a reference to an automatic variable, such as `$@`, `$(<)`,
/// `${@D}` or `$(@:.c=.o)`, the character naming the variable.
fn automatic_variable_name(text: &str, variant: MakefileVariant) -> Option<char> {
    let reference = ParsedReference::parse(text, variant).ok()?;
    let mut chars = reference.name.chars();
    let name = chars
        .next()
        .filter(|c| matches!(c, '@' | '<' | '^' | '?' | '*' | '+' | '|' | '%'))?;
    let only_substitutions = reference
        .modifiers
        .iter()
        .all(|m| matches!(m, Modifier::SysVSubstitute { .. }));
    (only_substitutions && matches!(chars.as_str(), "" | "D" | "F")).then_some(name)
}

/// Describe the immediately-expanded context `var_ref` sits in, or `None` if
/// it may be expanded later (or we can't tell).
fn immediate_expansion_context(
    var_ref: &VariableReference,
    set_in_prerequisites: bool,
    variant: MakefileVariant,
) -> Option<&'static str> {
    let mut current = var_ref.clone();
    loop {
        match current.location() {
            ReferenceLocation::FunctionArgument(outer)
            | ReferenceLocation::ReferenceName(outer)
            | ReferenceLocation::Modifier(outer) => {
                if matches!(outer.name().as_deref(), Some("eval" | "call")) {
                    return None;
                }
                current = outer;
            }
            ReferenceLocation::Target(_) => return Some("a target list"),
            ReferenceLocation::Prerequisite(_) => {
                return (!set_in_prerequisites).then_some("a prerequisite list");
            }
            ReferenceLocation::VariableName(var_def)
            | ReferenceLocation::VariableValue(var_def) => {
                if var_def.is_target_specific() || var_def.is_define() {
                    return None;
                }
                let op = var_def.assignment_operator()?;
                let immediate = match op.as_str() {
                    ":=" => variant != MakefileVariant::BSDMake,
                    "::=" | ":::=" | "!=" => true,
                    _ => false,
                };
                return immediate.then_some("an immediately-expanded assignment");
            }
            ReferenceLocation::Condition(_) => return Some("a conditional directive"),
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use makefile_lossless::Makefile;
    use tower_lsp_server::ls_types::Position;

    fn get_diags(text: &str) -> Vec<Diagnostic> {
        let parsed = Makefile::parse(text);
        get_diagnostics(text, &parsed, None)
    }

    fn diag_codes(text: &str) -> Vec<String> {
        get_diags(text)
            .into_iter()
            .filter_map(|d| d.code)
            .map(|c| match c {
                NumberOrString::String(s) => s,
                NumberOrString::Number(n) => n.to_string(),
            })
            .collect()
    }

    #[test]
    fn test_valid_makefile_no_diagnostics() {
        let text = "all: build\n\techo done\n";
        let diagnostics = get_diags(text);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_defined_variable_no_warning() {
        let text = "CC = gcc\nCFLAGS = $(CC) -Wall\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"undefined-variable".to_string()));
    }

    #[test]
    fn test_builtin_variable_no_warning() {
        // `$(MAKE)` is a builtin, so no undefined-variable warning. `CMD`
        // itself is unused (HINT), which is unrelated to this test.
        let text = "CMD = $(MAKE) -C subdir\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"undefined-variable".to_string()));
    }

    #[test]
    fn test_builtin_function_no_warning() {
        let text = "FILES = $(wildcard *.c)\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"undefined-variable".to_string()));
    }

    #[test]
    fn test_makefile_list_no_warning() {
        let text = "all:\n\techo $(lastword $(MAKEFILE_LIST))\n";
        assert_eq!(diag_codes(text), Vec::<String>::new());
    }

    #[test]
    fn test_make_host_no_warning() {
        let text = "all:\n\techo $(MAKE_HOST)\n";
        assert_eq!(diag_codes(text), Vec::<String>::new());
    }

    #[test]
    fn test_gnumakeflags_no_warning() {
        let text = "all:\n\techo $(GNUMAKEFLAGS)\n";
        assert_eq!(diag_codes(text), Vec::<String>::new());
    }

    #[test]
    fn test_undefined_variable_in_value() {
        let text = "CFLAGS = $(UNDEFINED_VAR) -Wall\n";
        let codes = diag_codes(text);
        assert_eq!(codes, vec!["undefined-variable"]);
    }

    #[test]
    fn test_undefined_variable_in_prerequisites() {
        let text = "all: $(MISSING_TARGETS)\n";
        let codes = diag_codes(text);
        assert_eq!(codes, vec!["undefined-variable"]);
    }

    #[test]
    fn test_undefined_variable_message() {
        let text = "CFLAGS = $(MISSING) -Wall\n";
        let diags = get_diags(text);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].message, "variable 'MISSING' is not defined");
    }

    #[test]
    fn test_multiple_undefined_variables() {
        let text = "CFLAGS = $(FOO) $(BAR)\n";
        let codes = diag_codes(text);
        assert_eq!(codes, vec!["undefined-variable", "undefined-variable"]);
    }

    // Recursive self-reference tests

    #[test]
    fn test_recursive_self_reference() {
        let text = "FOO = $(FOO) bar\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"recursive-variable-reference".to_string()));
    }

    #[test]
    fn test_simple_expand_self_reference_ok() {
        // := expands immediately, so self-reference is valid (refers to previous value)
        let text = "FOO := $(FOO) bar\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"recursive-variable-reference".to_string()));
    }

    #[test]
    fn test_append_self_reference_ok() {
        let text = "FOO += bar\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"recursive-variable-reference".to_string()));
    }

    #[test]
    fn test_recursive_self_reference_message() {
        let text = "FOO = $(FOO)\n";
        let diags = get_diags(text);
        let self_ref: Vec<_> = diags
            .iter()
            .filter(|d| {
                d.code
                    == Some(NumberOrString::String(
                        "recursive-variable-reference".to_string(),
                    ))
            })
            .collect();
        assert_eq!(self_ref.len(), 1);
        assert!(self_ref[0].message.contains("FOO"));
    }

    // Duplicate target tests

    #[test]
    fn test_duplicate_target() {
        let text = "all: build\n\techo first\n\nall: test\n\techo second\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"duplicate-target".to_string()));
    }

    #[test]
    fn test_duplicate_target_range_in_rule_with_several_targets() {
        let text = "b:\n\t@:\na b:\n\t@:\n";
        let ranges: Vec<Range> = get_diags(text)
            .into_iter()
            .filter(|d| d.code == Some(NumberOrString::String("duplicate-target".to_string())))
            .map(|d| d.range)
            .collect();
        assert_eq!(
            ranges,
            vec![Range::new(Position::new(2, 2), Position::new(2, 3))]
        );
    }

    #[test]
    fn test_no_duplicate_different_targets() {
        let text = "all: build\n\techo all\n\nbuild:\n\techo build\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"duplicate-target".to_string()));
    }

    #[test]
    fn test_duplicate_target_phony_ok() {
        // .PHONY can appear multiple times
        let text = ".PHONY: all\n.PHONY: build\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"duplicate-target".to_string()));
    }

    #[test]
    fn test_duplicate_target_double_colon_ok() {
        // Double-colon rules intentionally allow duplicates
        let text = "all:: dep1\n\techo first\n\nall:: dep2\n\techo second\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"duplicate-target".to_string()));
    }

    #[test]
    fn test_duplicate_target_message() {
        let text = "all: build\n\techo first\n\nall: test\n\techo second\n";
        let diags = get_diags(text);
        let dups: Vec<_> = diags
            .iter()
            .filter(|d| d.code == Some(NumberOrString::String("duplicate-target".to_string())))
            .collect();
        assert_eq!(dups.len(), 1);
        assert!(dups[0].message.contains("all"));
        assert!(dups[0].message.contains("line 1"));
    }

    fn duplicate_target_diags(text: &str) -> Vec<(Range, String)> {
        get_diags(text)
            .into_iter()
            .filter(|d| d.code == Some(NumberOrString::String("duplicate-target".to_string())))
            .map(|d| (d.range, d.message))
            .collect()
    }

    #[test]
    fn test_duplicate_target_in_conditional_branches_ok() {
        assert_eq!(
            duplicate_target_diags("ifdef X\nfoo:\n\techo a\nelse\nfoo:\n\techo b\nendif\n"),
            vec![]
        );
        assert_eq!(
            duplicate_target_diags(
                "ifdef A\nfoo:\n\techo a\nelse ifdef B\nfoo:\n\techo b\nelse\nfoo:\n\techo c\nendif\n"
            ),
            vec![]
        );
    }

    #[test]
    fn test_duplicate_target_same_conditional_branch() {
        assert_eq!(
            duplicate_target_diags("ifdef X\nfoo:\n\techo a\nfoo:\n\techo b\nendif\n"),
            vec![(
                Range::new(Position::new(3, 0), Position::new(3, 3)),
                "target 'foo' already defined on line 2".to_string()
            )]
        );
    }

    #[test]
    fn test_duplicate_target_inside_and_outside_conditional() {
        assert_eq!(
            duplicate_target_diags("foo:\n\techo a\nifdef X\nfoo:\n\techo b\nendif\n"),
            vec![(
                Range::new(Position::new(3, 0), Position::new(3, 3)),
                "target 'foo' already defined on line 1".to_string()
            )]
        );
    }

    #[test]
    fn test_duplicate_target_after_conditional_branches() {
        // The later definition clashes with whichever branch was taken.
        assert_eq!(
            duplicate_target_diags(
                "ifdef X\nfoo:\n\techo a\nelse\nfoo:\n\techo b\nendif\nfoo:\n\techo c\n"
            ),
            vec![(
                Range::new(Position::new(7, 0), Position::new(7, 3)),
                "target 'foo' already defined on line 2".to_string()
            )]
        );
    }

    #[test]
    fn test_pattern_rule_not_duplicate() {
        let text = "%.o: %.c\n\t$(CC) -c $<\n\n%.o: %.cpp\n\t$(CXX) -c $<\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"duplicate-target".to_string()));
    }

    // Empty variable reference tests

    #[test]
    fn test_empty_variable_reference() {
        let text = "FOO = $()\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"empty-variable-reference".to_string()));
    }

    #[test]
    fn test_non_empty_variable_reference_ok() {
        let text = "FOO = $(BAR)\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"empty-variable-reference".to_string()));
    }

    // Self-dependency tests

    #[test]
    fn test_self_dependency() {
        let text = "foo: foo bar\n\techo $@\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"self-dependency".to_string()));
    }

    #[test]
    fn test_no_self_dependency() {
        let text = "foo: bar baz\n\techo $@\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"self-dependency".to_string()));
    }

    #[test]
    fn test_self_dependency_message() {
        let text = "foo: foo\n\techo $@\n";
        let diags = get_diags(text);
        let self_deps: Vec<_> = diags
            .iter()
            .filter(|d| d.code == Some(NumberOrString::String("self-dependency".to_string())))
            .collect();
        assert_eq!(self_deps.len(), 1);
        assert!(self_deps[0].message.contains("foo"));
    }

    // Circular dependency tests

    fn circular_diags(text: &str) -> Vec<Diagnostic> {
        get_diags(text)
            .into_iter()
            .filter(|d| d.code == Some(NumberOrString::String("circular-dependency".to_string())))
            .collect()
    }

    #[test]
    fn test_circular_dependency_two_targets() {
        let text = "a: b\n\techo a\nb: a\n\techo b\n";
        let diags = circular_diags(text);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].message, "circular dependency: a -> b -> a");
    }

    #[test]
    fn test_circular_dependency_three_targets() {
        let text = "a: b\n\t@:\nb: c\n\t@:\nc: a\n\t@:\n";
        let diags = circular_diags(text);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].message, "circular dependency: a -> b -> c -> a");
    }

    #[test]
    fn test_no_circular_dependency_linear() {
        let text = "a: b\n\t@:\nb: c\n\t@:\nc:\n\t@:\n";
        let diags = circular_diags(text);
        assert!(diags.is_empty());
    }

    #[test]
    fn test_no_circular_dependency_diamond() {
        let text = "a: b c\n\t@:\nb: d\n\t@:\nc: d\n\t@:\nd:\n\t@:\n";
        let diags = circular_diags(text);
        assert!(diags.is_empty());
    }

    #[test]
    fn test_circular_dependency_self_loop_not_double_reported() {
        // Self-loops are reported by `check_self_dependency`, not here.
        let text = "a: a\n\t@:\n";
        let diags = circular_diags(text);
        assert!(diags.is_empty());
    }

    #[test]
    fn test_circular_dependency_via_accumulated_prereqs() {
        // Make merges prerequisites across multiple rules for the same target,
        // so this still forms a cycle a -> b -> a.
        let text = "a: b\n\t@:\nb:\n\t@:\nb: a\n";
        let diags = circular_diags(text);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].message, "circular dependency: a -> b -> a");
    }

    fn circular_messages(text: &str) -> Vec<String> {
        circular_diags(text)
            .into_iter()
            .map(|d| d.message)
            .collect()
    }

    #[test]
    fn test_no_circular_dependency_across_exclusive_branches() {
        let text = "ifdef X\na: b\nelse\nb: a\nendif\n";
        assert_eq!(circular_messages(text), Vec::<String>::new());
    }

    #[test]
    fn test_circular_dependency_within_one_branch() {
        let text = "ifdef X\na: b\nb: a\nendif\n";
        assert_eq!(
            circular_messages(text),
            vec!["circular dependency: a -> b -> a".to_string()]
        );
    }

    #[test]
    fn test_circular_dependency_behind_exclusive_one() {
        // a -> b -> a mixes branches, but a -> c -> b -> a does not.
        let text = "ifdef X\na: b\nelse\nb: a\nendif\na: c\nc: b\n";
        assert_eq!(
            circular_messages(text),
            vec!["circular dependency: a -> c -> b -> a".to_string()]
        );
    }

    #[test]
    fn test_circular_dependency_ignores_undefined_prereq() {
        // `missing` is a file on disk (or just an error), not a target —
        // no cycle possible through it.
        let text = "a: missing\n\t@:\n";
        let diags = circular_diags(text);
        assert!(diags.is_empty());
    }

    #[test]
    fn test_circular_dependency_dedupes_rotations() {
        // The graph has one cycle (a, b, c) which the DFS could discover from
        // any starting node — make sure we only report it once.
        let text = "a: b\n\t@:\nb: c\n\t@:\nc: a\n\t@:\nentry: a b c\n\t@:\n";
        let diags = circular_diags(text);
        assert_eq!(diags.len(), 1);
    }

    #[test]
    fn test_circular_dependency_two_disjoint_cycles() {
        let text = concat!("a: b\n\t@:\nb: a\n\t@:\n", "x: y\n\t@:\ny: x\n\t@:\n",);
        let diags = circular_diags(text);
        assert_eq!(diags.len(), 2);
    }

    // Undefined .PHONY target tests

    #[test]
    fn test_undefined_phony_target() {
        let text = ".PHONY: clean\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"undefined-phony-target".to_string()));
    }

    #[test]
    fn test_defined_phony_target_ok() {
        let text = ".PHONY: clean\nclean:\n\trm -f *.o\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"undefined-phony-target".to_string()));
    }

    #[test]
    fn test_phony_partially_defined() {
        let text = ".PHONY: all clean\nall: build\n\techo done\n";
        let diags = get_diags(text);
        let phony_diags: Vec<_> = diags
            .iter()
            .filter(|d| {
                d.code == Some(NumberOrString::String("undefined-phony-target".to_string()))
            })
            .collect();
        assert_eq!(phony_diags.len(), 1);
        assert!(phony_diags[0].message.contains("clean"));
    }

    // Unused / unreferenced .PHONY target tests

    #[test]
    fn test_unused_phony_target_empty_rule() {
        // .PHONY: clena has an empty rule and nothing references it — typo.
        let text = ".PHONY: clena\nclena:\n";
        let diags = get_diags(text);
        let unused: Vec<_> = diags
            .iter()
            .filter(|d| d.code == Some(NumberOrString::String("unused-phony-target".to_string())))
            .collect();
        assert_eq!(unused.len(), 1);
        assert!(unused[0].message.contains("clena"));
        assert_eq!(unused[0].severity, Some(DiagnosticSeverity::WARNING));
    }

    #[test]
    fn test_unused_phony_target_silenced_when_referenced() {
        // 'helper' has an empty rule but `all` depends on it -> ok.
        let text = ".PHONY: helper\nall: helper\n\t@:\nhelper:\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"unused-phony-target".to_string()));
    }

    #[test]
    fn test_unused_phony_target_silenced_when_recipe_present() {
        // Has a recipe -> falls through to unreferenced-phony-target instead,
        // which is a hint not a warning.
        let text = ".PHONY: foo\nfoo:\n\techo foo\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"unused-phony-target".to_string()));
        assert!(codes.contains(&"unreferenced-phony-target".to_string()));
    }

    #[test]
    fn test_unreferenced_phony_target_hint() {
        // 'lint' is not in PHONY_ENTRY_POINTS and nothing depends on it.
        let text = ".PHONY: format\nformat:\n\tfoo\n";
        let diags = get_diags(text);
        let hints: Vec<_> = diags
            .iter()
            .filter(|d| {
                d.code
                    == Some(NumberOrString::String(
                        "unreferenced-phony-target".to_string(),
                    ))
            })
            .collect();
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].severity, Some(DiagnosticSeverity::HINT));
    }

    #[test]
    fn test_unreferenced_phony_silenced_for_entry_points() {
        // 'all', 'install', 'clean' are conventional entry points -> no hint.
        let text = concat!(
            ".PHONY: all install clean\n",
            "all:\n\t@:\n",
            "install:\n\t@:\n",
            "clean:\n\t@:\n",
        );
        let codes = diag_codes(text);
        assert!(!codes.contains(&"unreferenced-phony-target".to_string()));
    }

    #[test]
    fn test_unreferenced_phony_silenced_when_referenced() {
        let text = ".PHONY: lint\nall: lint\n\t@:\nlint:\n\tfoo\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"unreferenced-phony-target".to_string()));
    }

    #[test]
    fn test_unused_phony_self_reference_does_not_count() {
        // A rule listing itself as prerequisite is not a real incoming edge —
        // and check_self_dependency will flag it separately. The unused-phony
        // warning should still fire.
        let text = ".PHONY: foo\nfoo: foo\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"unused-phony-target".to_string()));
    }

    #[test]
    fn test_unused_phony_multiple_rules_one_nonempty() {
        // 'foo' has two rules; one is empty, the other has a recipe. As long
        // as any rule does real work it's not "unused"; demote to hint only.
        let text = ".PHONY: foo\nfoo:\nfoo:\n\techo hi\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"unused-phony-target".to_string()));
        assert!(codes.contains(&"unreferenced-phony-target".to_string()));
    }

    // Include missing path tests

    #[test]
    fn test_include_with_path_ok() {
        let text = "include config.mk\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"include-missing-path".to_string()));
    }

    fn include_missing_path_lines(text: &str) -> Vec<u32> {
        get_diags(text)
            .into_iter()
            .filter(|d| d.code == Some(NumberOrString::String("include-missing-path".into())))
            .map(|d| d.range.start.line)
            .collect()
    }

    #[test]
    fn test_include_missing_path() {
        assert_eq!(
            include_missing_path_lines(
                "include
"
            ),
            vec![0]
        );
    }

    #[test]
    fn test_include_missing_path_in_conditional() {
        let text = "ifdef A
include
else ifdef B
include
else
ifdef C
include
endif
endif
";
        assert_eq!(include_missing_path_lines(text), vec![1, 3, 6]);
    }

    // Spaces instead of tab tests

    #[test]
    fn test_tab_indented_recipe_ok() {
        let text = "all:\n\techo done\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"spaces-instead-of-tab".to_string()));
    }

    #[test]
    fn test_spaces_instead_of_tab() {
        let text = "all:\n    echo done\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"spaces-instead-of-tab".to_string()));
    }

    #[test]
    fn test_spaces_instead_of_tab_message() {
        let text = "all:\n    echo done\n";
        let diags = get_diags(text);
        let space_diags: Vec<_> = diags
            .iter()
            .filter(|d| d.code == Some(NumberOrString::String("spaces-instead-of-tab".to_string())))
            .collect();
        assert_eq!(space_diags.len(), 1);
        assert_eq!(
            space_diags[0].message,
            "recipe lines must start with a tab, not spaces"
        );
        assert_eq!(space_diags[0].severity, Some(DiagnosticSeverity::ERROR));
    }

    #[test]
    fn test_multiple_space_indented_recipes() {
        let text = "all:\n    echo first\n    echo second\n";
        let diags = get_diags(text);
        let space_diags: Vec<_> = diags
            .iter()
            .filter(|d| d.code == Some(NumberOrString::String("spaces-instead-of-tab".to_string())))
            .collect();
        assert_eq!(space_diags.len(), 2);
    }

    // Redundant transitive prerequisite tests

    fn redundant_diags(text: &str) -> Vec<Diagnostic> {
        get_diags(text)
            .into_iter()
            .filter(|d| {
                d.code == Some(NumberOrString::String("redundant-prerequisite".to_string()))
            })
            .collect()
    }

    #[test]
    fn test_redundant_prereq_simple() {
        // `all: lib main`, `main: lib` -> `lib` is redundant in `all`.
        let text = "all: lib main\n\t@:\nmain: lib\n\t@:\nlib:\n\t@:\n";
        let diags = redundant_diags(text);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, Some(DiagnosticSeverity::HINT));
        assert!(diags[0].message.contains("'lib'"));
        assert!(diags[0].message.contains("'main'"));
    }

    #[test]
    fn test_redundant_prereq_silenced_when_independent() {
        let text = "all: a b\n\t@:\na:\n\t@:\nb:\n\t@:\n";
        let diags = redundant_diags(text);
        assert!(diags.is_empty());
    }

    #[test]
    fn test_redundant_prereq_transitive_chain() {
        // `all: a c`, `a: b`, `b: c` -> `c` is redundant in `all`.
        let text = "all: a c\n\t@:\na: b\n\t@:\nb: c\n\t@:\nc:\n\t@:\n";
        let diags = redundant_diags(text);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("'c'"));
    }

    #[test]
    fn test_redundant_prereq_skips_undefined() {
        // `lib` isn't a defined target, so we can't claim it's reachable.
        let text = "all: lib main\n\t@:\nmain:\n\t@:\n";
        let diags = redundant_diags(text);
        assert!(diags.is_empty());
    }

    fn redundant_messages(text: &str) -> Vec<String> {
        redundant_diags(text)
            .into_iter()
            .map(|d| d.message)
            .collect()
    }

    #[test]
    fn test_redundant_prereq_silenced_across_exclusive_branches() {
        let text = "ifdef X\nb: c\nelse\nall: b c\nendif\nc:\n";
        assert_eq!(redundant_messages(text), Vec::<String>::new());
    }

    #[test]
    fn test_redundant_prereq_within_one_branch() {
        let text = "ifdef X\nb: c\nall: b c\nendif\nc:\n";
        assert_eq!(
            redundant_messages(text),
            vec!["prerequisite 'c' is already pulled in transitively via 'b'".to_string()]
        );
    }

    #[test]
    fn test_redundant_prereq_via_first_prerequisite() {
        // Both `b` and `a` pull in `c`; report the first in the rule.
        let text = "all: b a c\na: c\nb: c\nc:\n";
        for _ in 0..20 {
            assert_eq!(
                redundant_messages(text),
                vec!["prerequisite 'c' is already pulled in transitively via 'b'".to_string()]
            );
        }
    }

    #[test]
    fn test_redundant_prereq_skips_single_prereq_rule() {
        let text = "all: only\n\t@:\nonly:\n\t@:\n";
        let diags = redundant_diags(text);
        assert!(diags.is_empty());
    }

    // Trailing whitespace in value tests

    #[test]
    fn test_trailing_whitespace_in_value() {
        let text = "FOO = bar \n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"trailing-whitespace-in-value".to_string()));
    }

    #[test]
    fn test_trailing_tab_in_value() {
        let text = "FOO = bar\t\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"trailing-whitespace-in-value".to_string()));
    }

    #[test]
    fn test_no_trailing_whitespace_clean_value() {
        let text = "FOO = bar\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"trailing-whitespace-in-value".to_string()));
    }

    #[test]
    fn test_internal_whitespace_not_flagged() {
        let text = "FOO = bar baz\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"trailing-whitespace-in-value".to_string()));
    }

    #[test]
    fn test_empty_value_not_flagged() {
        // `FOO = ` is an empty assignment; the whitespace is between `=` and EOL,
        // not part of the value.
        let text = "FOO = \n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"trailing-whitespace-in-value".to_string()));
    }

    #[test]
    fn test_trailing_whitespace_before_comment_flagged() {
        // `FOO = bar # comment` sets FOO to "bar " (trailing space captured).
        let text = "FOO = bar # comment\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"trailing-whitespace-in-value".to_string()));
    }

    #[test]
    fn test_line_continuation_not_flagged() {
        // The trailing whitespace before `\` is part of a continued line, not the
        // end of the value. We only care about the final value's tail.
        let text = "FOO = bar \\\n\tbaz\n";
        let codes = diag_codes(text);
        // The trailing token here is BACKSLASH, not WHITESPACE — so no warning.
        assert!(!codes.contains(&"trailing-whitespace-in-value".to_string()));
    }

    // Duplicate prerequisites tests

    fn mixed_separator_diags(text: &str) -> Vec<(Range, String)> {
        get_diags(text)
            .into_iter()
            .filter(|d| d.code == Some(NumberOrString::String("mixed-rule-separator".to_string())))
            .map(|d| (d.range, d.message))
            .collect()
    }

    #[test]
    fn test_mixed_rule_separator() {
        let diags = mixed_separator_diags("x: a\nx:: b\n");
        assert_eq!(
            diags,
            vec![(
                Range::new(Position::new(1, 0), Position::new(1, 1)),
                "target 'x' has both : and :: rules (first defined with ':' on line 1)".to_string()
            )]
        );
        let d = get_diags("x: a\nx:: b\n");
        assert_eq!(d[0].severity, Some(DiagnosticSeverity::ERROR));
    }

    #[test]
    fn test_mixed_rule_separator_double_colon_first() {
        let diags = mixed_separator_diags("a x:: y\n\techo 1\nx: z\n");
        assert_eq!(
            diags,
            vec![(
                Range::new(Position::new(2, 0), Position::new(2, 1)),
                "target 'x' has both : and :: rules (first defined with '::' on line 1)"
                    .to_string()
            )]
        );
    }

    #[test]
    fn test_mixed_rule_separator_consistent_ok() {
        assert_eq!(mixed_separator_diags("x:: a\nx:: b\n"), vec![]);
        assert_eq!(mixed_separator_diags("x: a\nx: b\n"), vec![]);
    }

    #[test]
    fn test_mixed_rule_separator_in_conditional_branches_ok() {
        assert_eq!(
            mixed_separator_diags("ifdef A\nx: a\nelse ifdef B\nx:: b\nelse\nx: c\nendif\n"),
            vec![]
        );
    }

    #[test]
    fn test_mixed_rule_separator_in_conditional() {
        assert_eq!(
            mixed_separator_diags("x: a\nifdef A\nx:: b\nendif\n"),
            vec![(
                Range::new(Position::new(2, 0), Position::new(2, 1)),
                "target 'x' has both : and :: rules (first defined with ':' on line 1)".to_string()
            )]
        );
    }

    #[test]
    fn test_mixed_rule_separator_archive_member() {
        assert_eq!(mixed_separator_diags("lib.a: x\nlib.a(m.o):: y\n"), vec![]);
        assert_eq!(
            mixed_separator_diags("lib.a(m.o): x\nlib.a(m.o):: y\n"),
            vec![(
                Range::new(Position::new(1, 0), Position::new(1, 10)),
                "target 'lib.a(m.o)' has both : and :: rules (first defined with ':' on line 1)"
                    .to_string()
            )]
        );
    }

    #[test]
    fn test_mixed_rule_separator_variable_in_target() {
        assert_eq!(mixed_separator_diags("foo: x\nfoo$(V):: y\n"), vec![]);
    }

    #[test]
    fn test_mixed_rule_separator_target_with_spaces() {
        assert_eq!(
            mixed_separator_diags("$(call f, a) b: x\n$(call f, a) b:: y\n"),
            vec![
                (
                    Range::new(Position::new(1, 0), Position::new(1, 12)),
                    "target '$(call f, a)' has both : and :: rules (first defined with ':' on line 1)"
                        .to_string()
                ),
                (
                    Range::new(Position::new(1, 13), Position::new(1, 14)),
                    "target 'b' has both : and :: rules (first defined with ':' on line 1)"
                        .to_string()
                )
            ]
        );
    }

    #[test]
    fn test_mixed_rule_separator_pattern_rule_ok() {
        assert_eq!(mixed_separator_diags("%.o: %.c\n%.o:: %.s\n"), vec![]);
    }

    #[test]
    fn test_duplicate_prerequisite() {
        let text = "foo: a b a\n\techo $@\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"duplicate-prerequisite".to_string()));
    }

    #[test]
    fn test_no_duplicate_prerequisites() {
        let text = "foo: a b c\n\techo $@\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"duplicate-prerequisite".to_string()));
    }

    #[test]
    fn test_duplicate_prerequisite_message() {
        let text = "foo: a b a\n\techo $@\n";
        let diags = get_diags(text);
        let dup_diags: Vec<_> = diags
            .iter()
            .filter(|d| {
                d.code == Some(NumberOrString::String("duplicate-prerequisite".to_string()))
            })
            .collect();
        assert_eq!(dup_diags.len(), 1);
        assert_eq!(
            dup_diags[0].message,
            "prerequisite 'a' is listed more than once"
        );
    }

    #[test]
    fn test_duplicate_prerequisite_reported_once_per_name() {
        // `a` appears three times — should report once, not twice.
        let text = "foo: a b a c a\n\techo $@\n";
        let diags = get_diags(text);
        let dup_diags: Vec<_> = diags
            .iter()
            .filter(|d| {
                d.code == Some(NumberOrString::String("duplicate-prerequisite".to_string()))
            })
            .collect();
        assert_eq!(dup_diags.len(), 1);
    }

    #[test]
    fn test_duplicate_prerequisites_distinct_names() {
        let text = "foo: a b a b\n\techo $@\n";
        let diags = get_diags(text);
        let dup_diags: Vec<_> = diags
            .iter()
            .filter(|d| {
                d.code == Some(NumberOrString::String("duplicate-prerequisite".to_string()))
            })
            .collect();
        assert_eq!(dup_diags.len(), 2);
    }

    #[test]
    fn test_duplicate_prerequisite_across_rules_ok() {
        // Same prereq in two different rules is fine.
        let text = "foo: shared\n\techo foo\n\nbar: shared\n\techo bar\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"duplicate-prerequisite".to_string()));
    }

    // Shell in recursive assignment tests

    #[test]
    fn test_shell_in_recursive_assignment() {
        let text = "FILES = $(shell ls)\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"shell-in-recursive-assignment".to_string()));
    }

    #[test]
    fn test_shell_in_simply_expanded_assignment_ok() {
        let text = "FILES := $(shell ls)\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"shell-in-recursive-assignment".to_string()));
    }

    #[test]
    fn test_shell_in_immediate_expand_assignment_ok() {
        let text = "FILES ::= $(shell ls)\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"shell-in-recursive-assignment".to_string()));
    }

    #[test]
    fn test_no_shell_in_recursive_ok() {
        let text = "FOO = $(BAR)\nBAR = baz\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"shell-in-recursive-assignment".to_string()));
    }

    #[test]
    fn test_shell_variable_reference_not_function_call_ok() {
        // `$(shell)` (no args) is a variable reference to a variable named
        // "shell", not a function call. The bug only applies to the function form.
        let text = "shell = bash\nFOO = $(shell)\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"shell-in-recursive-assignment".to_string()));
    }

    #[test]
    fn test_shell_in_recursive_message() {
        let text = "FILES = $(shell ls)\n";
        let diags = get_diags(text);
        let shell_diags: Vec<_> = diags
            .iter()
            .filter(|d| {
                d.code
                    == Some(NumberOrString::String(
                        "shell-in-recursive-assignment".to_string(),
                    ))
            })
            .collect();
        assert_eq!(shell_diags.len(), 1);
        assert!(shell_diags[0].message.contains("re-run on every expansion"));
        assert_eq!(shell_diags[0].severity, Some(DiagnosticSeverity::WARNING));
    }

    #[test]
    fn test_shell_nested_in_recursive_assignment() {
        // $(shell ...) wrapped in another function call still counts.
        let text = "FILES = $(strip $(shell ls))\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"shell-in-recursive-assignment".to_string()));
    }

    #[test]
    fn test_trailing_whitespace_message() {
        let text = "FOO = bar   \n";
        let diags = get_diags(text);
        let ws_diags: Vec<_> = diags
            .iter()
            .filter(|d| {
                d.code
                    == Some(NumberOrString::String(
                        "trailing-whitespace-in-value".to_string(),
                    ))
            })
            .collect();
        assert_eq!(ws_diags.len(), 1);
        assert_eq!(
            ws_diags[0].message,
            "trailing whitespace is included in the variable value"
        );
        assert_eq!(ws_diags[0].severity, Some(DiagnosticSeverity::WARNING));
    }

    // Empty automatic variable tests

    #[test]
    fn test_dollar_less_in_rule_with_no_prereqs() {
        let text = "foo:\n\techo $<\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"empty-automatic-variable".to_string()));
    }

    #[test]
    fn test_dollar_less_with_prereqs_ok() {
        let text = "foo: bar\n\techo $<\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"empty-automatic-variable".to_string()));
    }

    #[test]
    fn test_dollar_caret_in_rule_with_no_prereqs() {
        let text = "foo:\n\techo $^\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"empty-automatic-variable".to_string()));
    }

    #[test]
    fn test_dollar_plus_in_rule_with_no_prereqs() {
        let text = "foo:\n\techo $+\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"empty-automatic-variable".to_string()));
    }

    #[test]
    fn test_dollar_question_in_rule_with_no_prereqs() {
        let text = "foo:\n\techo $?\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"empty-automatic-variable".to_string()));
    }

    #[test]
    fn test_dollar_at_with_no_prereqs_ok() {
        // $@ is the target — always defined regardless of prereqs.
        let text = "foo:\n\techo $@\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"empty-automatic-variable".to_string()));
    }

    #[test]
    fn test_dollar_star_in_non_pattern_rule() {
        let text = "foo: bar\n\techo $*\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"empty-automatic-variable".to_string()));
    }

    #[test]
    fn test_dollar_star_in_pattern_rule_ok() {
        let text = "%.o: %.c\n\techo $*\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"empty-automatic-variable".to_string()));
    }

    #[test]
    fn test_dollar_less_in_pattern_rule_ok() {
        let text = "%.o: %.c\n\t$(CC) -c $<\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"empty-automatic-variable".to_string()));
    }

    #[test]
    fn test_parenthesized_form_flagged() {
        let text = "foo:\n\techo $(<)\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"empty-automatic-variable".to_string()));
    }

    #[test]
    fn test_braced_form_flagged() {
        let text = "foo:\n\techo ${<}\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"empty-automatic-variable".to_string()));
    }

    #[test]
    fn test_escaped_dollar_not_flagged() {
        let text = "foo:\n\techo $$<\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"empty-automatic-variable".to_string()));
    }

    #[test]
    fn test_empty_auto_var_message() {
        let text = "foo:\n\techo $<\n";
        let diags = get_diags(text);
        let auto: Vec<_> = diags
            .iter()
            .filter(|d| {
                d.code
                    == Some(NumberOrString::String(
                        "empty-automatic-variable".to_string(),
                    ))
            })
            .collect();
        assert_eq!(auto.len(), 1);
        assert!(auto[0].message.contains("$<"));
        assert!(auto[0].message.contains("no prerequisites"));
    }

    #[test]
    fn test_dir_form_flagged() {
        let text = "foo:\n\techo $(<D) ${*F}\n";
        let messages: Vec<_> = get_diags(text)
            .into_iter()
            .filter(|d| {
                d.code
                    == Some(NumberOrString::String(
                        "empty-automatic-variable".to_string(),
                    ))
            })
            .map(|d| (d.range, d.message))
            .collect();
        assert_eq!(
            messages,
            vec![
                (
                    Range::new(Position::new(1, 6), Position::new(1, 11)),
                    "$(<D) expands to empty: rule has no prerequisites".to_string()
                ),
                (
                    Range::new(Position::new(1, 12), Position::new(1, 17)),
                    "$(*F) expands to empty: non-pattern rule".to_string()
                ),
            ]
        );
    }

    // Unused variable tests

    #[test]
    fn test_unused_variable() {
        let text = "FOO = bar\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"unused-variable".to_string()));
    }

    #[test]
    fn test_used_in_other_variable_ok() {
        let text = "FOO = bar\nBAZ = $(FOO)\nused: \n\techo $(BAZ)\n";
        let codes = diag_codes(text);
        // BAZ is used in recipe; FOO is used in BAZ.
        assert!(!codes.contains(&"unused-variable".to_string()));
    }

    #[test]
    fn test_used_in_recipe_ok() {
        let text = "FOO = bar\nall:\n\techo $(FOO)\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"unused-variable".to_string()));
    }

    #[test]
    fn test_used_in_recipe_braces_ok() {
        let text = "FOO = bar\nall:\n\techo ${FOO}\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"unused-variable".to_string()));
    }

    #[test]
    fn test_used_in_recipe_single_char_ok() {
        let text = "X = bar\nall:\n\techo $X\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"unused-variable".to_string()));
    }

    #[test]
    fn test_undefined_in_recipe_and_define_body_not_flagged() {
        let text = "define F\n$(1) $(A)\nendef\nall:\n\techo $(B) $@ $(notdir $<)\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"undefined-variable".to_string()));
    }

    #[test]
    fn test_used_in_ifdef_ok() {
        let text = "FOO = bar\nifdef FOO\nVAR = x\nendif\n";
        let codes = diag_codes(text);
        // FOO is referenced by `ifdef FOO`. VAR is inside a conditional, so
        // skipped from the unused check.
        assert!(!codes.contains(&"unused-variable".to_string()));
    }

    #[test]
    fn test_used_in_else_branch_condition_ok() {
        let empty: Vec<String> = vec![];
        assert_eq!(
            unused_variable_messages("X = 1\nifdef A\nelse ifdef X\nendif\n"),
            empty
        );
        assert_eq!(
            unused_variable_messages("X = 1\nifdef A\nelse ifndef X\nendif\n"),
            empty
        );
        assert_eq!(
            unused_variable_messages("X = 1\nifdef A\nelse ifeq ($(X),1)\nendif\n"),
            empty
        );
        assert_eq!(
            unused_variable_messages("X = 1\nifdef A\nifdef B\nelse ifdef X\nendif\nendif\n"),
            empty
        );
        assert_eq!(
            unused_variable_messages(
                "X = 1\nall:\nifdef A\n\techo a\nelse ifdef X\n\techo x\nendif\n"
            ),
            empty
        );
    }

    #[test]
    fn test_used_in_bsd_ifdef_ok() {
        let empty: Vec<String> = vec![];
        for text in [
            "X = 1\n.ifdef X\n.endif\n",
            "X = 1\n.ifndef X\n.endif\n",
            "X = 1\n.if 1\n.elifdef X\n.endif\n",
            "X = 1\n.if 1\n.elifndef X\n.endif\n",
            "X = 1\n.ifdef A || X\n.endif\n",
        ] {
            assert_eq!(unused_variable_messages(text), empty, "{text:?}");
        }
    }

    #[test]
    fn test_used_in_bsd_if_defined_ok() {
        let empty: Vec<String> = vec![];
        for text in [
            "X = 1\n.if defined(X)\n.endif\n",
            "X = 1\n.if !defined(X)\n.endif\n",
            "X = 1\n.if 1\n.elif defined(A) && !defined(X)\n.endif\n",
            "X = 1\n.if X\n.endif\n",
            "X = 1\n.if empty(X:Mfoo)\n.endif\n",
        ] {
            assert_eq!(unused_variable_messages(text), empty, "{text:?}");
        }
    }

    #[test]
    fn test_bsd_ifmake_not_a_use() {
        assert_eq!(
            unused_variable_messages("X = 1\n.ifmake X\n.endif\n"),
            vec!["variable 'X' is defined but never used".to_string()]
        );
    }

    fn nmake_unused_variable_messages(text: &str) -> Vec<String> {
        let parsed = Makefile::parse_with_variant(text, MakefileVariant::NMake);
        get_diagnostics(text, &parsed, None)
            .into_iter()
            .filter(|d| d.code == Some(NumberOrString::String("unused-variable".to_string())))
            .map(|d| d.message)
            .collect()
    }

    #[test]
    fn test_used_in_nmake_ifdef_ok() {
        let empty: Vec<String> = vec![];
        for text in [
            "X = 1\n!IFDEF X\n!ENDIF\n",
            "X = 1\n!IFNDEF X\n!ENDIF\n",
            "X = 1\n!IF 1\n!ELSEIFDEF X\n!ENDIF\n",
            "X = 1\n!IF 1\n!ELSE IFNDEF X\n!ENDIF\n",
        ] {
            assert_eq!(nmake_unused_variable_messages(text), empty, "{text:?}");
        }
    }

    #[test]
    fn test_used_in_nmake_if_defined_ok() {
        let empty: Vec<String> = vec![];
        for text in [
            "X = 1\n!IF DEFINED(X)\n!ENDIF\n",
            "X = 1\n!IF !DEFINED(X)\n!ENDIF\n",
            "X = 1\n!IF 1\n!ELSEIF DEFINED(A) && !DEFINED(X)\n!ENDIF\n",
        ] {
            assert_eq!(nmake_unused_variable_messages(text), empty, "{text:?}");
        }
    }

    #[test]
    fn test_nmake_unused_variable() {
        assert_eq!(
            nmake_unused_variable_messages("X = 1\n!IF DEFINED(Y)\n!ENDIF\n"),
            vec!["variable 'X' is defined but never used".to_string()]
        );
    }

    #[test]
    fn test_exported_variable_skipped() {
        let text = "export FOO = bar\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"unused-variable".to_string()));
    }

    fn unused_variable_messages(text: &str) -> Vec<String> {
        get_diags(text)
            .into_iter()
            .filter(|d| d.code == Some(NumberOrString::String("unused-variable".to_string())))
            .map(|d| d.message)
            .collect()
    }

    #[test]
    fn test_export_directive_counts_as_use() {
        let empty: Vec<String> = vec![];
        assert_eq!(unused_variable_messages("Z = 1\nexport Z\n"), empty);
        assert_eq!(unused_variable_messages("export Z\nZ = 1\n"), empty);
        assert_eq!(
            unused_variable_messages("Z = 1\nY = 2\nexport Z Y\n"),
            empty
        );
    }

    #[test]
    fn test_unexport_does_not_count_as_use() {
        // `unexport` removes Z from recipe environments; it doesn't read it.
        assert_eq!(
            unused_variable_messages("Z = 1\nunexport Z\n"),
            vec!["variable 'Z' is defined but never used"]
        );
    }

    #[test]
    fn test_undefine_is_not_a_definition() {
        assert_eq!(
            unused_variable_messages("undefine Z\n"),
            Vec::<String>::new()
        );
        // `undefine` doesn't read Z, so it doesn't count as a use either.
        assert_eq!(
            unused_variable_messages("Z = 1\nundefine Z\n"),
            vec!["variable 'Z' is defined but never used"]
        );
    }

    #[test]
    fn test_export_all_variables_counts_as_use() {
        let empty: Vec<String> = vec![];
        assert_eq!(
            unused_variable_messages("Z = 1\n.EXPORT_ALL_VARIABLES:\n"),
            empty
        );
        assert_eq!(unused_variable_messages("Z = 1\nexport\n"), empty);
    }

    #[test]
    fn test_overriding_builtin_skipped() {
        let text = "CC = my-special-gcc\n";
        let codes = diag_codes(text);
        // CC is a builtin; assigning it overrides make's default. Not unused.
        assert!(!codes.contains(&"unused-variable".to_string()));
    }

    #[test]
    fn test_inside_conditional_skipped() {
        let text = "ifdef DEBUG\nFOO = bar\nendif\n";
        let codes = diag_codes(text);
        // FOO is defined inside a conditional — likely a config toggle.
        assert!(!codes.contains(&"unused-variable".to_string()));
    }

    #[test]
    fn test_unused_variable_message() {
        let text = "FOO = bar\n";
        let diags = get_diags(text);
        let unused: Vec<_> = diags
            .iter()
            .filter(|d| d.code == Some(NumberOrString::String("unused-variable".to_string())))
            .collect();
        assert_eq!(unused.len(), 1);
        assert_eq!(
            unused[0].message,
            "variable 'FOO' is defined but never used"
        );
        assert_eq!(unused[0].severity, Some(DiagnosticSeverity::HINT));
    }

    #[test]
    fn test_used_in_target_or_prereq_ok() {
        let text = "SOURCES = a.c\n$(SOURCES):\n\techo $@\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"unused-variable".to_string()));
    }

    #[test]
    fn test_dollar_dollar_in_recipe_does_not_count() {
        // `$$FOO` is shell variable expansion, not a make variable reference.
        let text = "FOO = bar\nall:\n\techo $$FOO\n";
        let codes = diag_codes(text);
        // FOO is unused — `$$FOO` is the shell's FOO, not make's.
        assert!(codes.contains(&"unused-variable".to_string()));
    }

    #[test]
    fn test_used_in_define_body_ok() {
        let text = "FOO = bar\ndefine F\necho $(FOO)\nendef\nall:\n\t$(F)\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"unused-variable".to_string()), "{codes:?}");
    }

    #[test]
    fn test_used_in_orphan_recipe_ok() {
        let text = "FOO = bar\nifdef X\n\techo $(FOO)\nendif\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"unused-variable".to_string()), "{codes:?}");
    }

    // Missing include file tests

    fn diags_with_dir(text: &str, dir: &std::path::Path) -> Vec<Diagnostic> {
        let parsed = Makefile::parse(text);
        get_diagnostics(text, &parsed, Some(dir))
    }

    fn codes_with_dir(text: &str, dir: &std::path::Path) -> Vec<String> {
        diags_with_dir(text, dir)
            .into_iter()
            .filter_map(|d| d.code)
            .map(|c| match c {
                NumberOrString::String(s) => s,
                NumberOrString::Number(n) => n.to_string(),
            })
            .collect()
    }

    #[test]
    fn test_missing_include_file_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let codes = codes_with_dir("include config.mk\n", dir.path());
        assert!(codes.contains(&"missing-include-file".to_string()));
    }

    #[test]
    fn test_existing_include_file_ok() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.mk"), "").unwrap();
        let codes = codes_with_dir("include config.mk\n", dir.path());
        assert!(!codes.contains(&"missing-include-file".to_string()));
    }

    #[test]
    fn test_optional_include_not_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let codes = codes_with_dir("-include config.mk\n", dir.path());
        assert!(!codes.contains(&"missing-include-file".to_string()));
    }

    #[test]
    fn test_include_with_literal_variable_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let codes = codes_with_dir("CONFIG = config.mk\ninclude $(CONFIG)\n", dir.path());
        assert!(codes.contains(&"missing-include-file".to_string()));
    }

    #[test]
    fn test_include_with_variable_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let codes = codes_with_dir("CONFIG ?= config.mk\ninclude $(CONFIG)\n", dir.path());
        // Can't resolve $(CONFIG) at lint time, since it may be overridden.
        assert!(!codes.contains(&"missing-include-file".to_string()));
    }

    #[test]
    fn test_missing_include_file_among_several() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.mk"), "").unwrap();
        let diags = diags_with_dir("include a.mk b.mk\n", dir.path());
        let missing: Vec<(Range, String)> = diags
            .into_iter()
            .filter(|d| d.code == Some(NumberOrString::String("missing-include-file".to_string())))
            .map(|d| (d.range, d.message))
            .collect();
        assert_eq!(
            missing,
            vec![(
                Range::new(Position::new(0, 13), Position::new(0, 17)),
                "included file 'b.mk' does not exist".to_string()
            )]
        );
    }

    #[test]
    fn test_no_base_dir_skips_check() {
        // When no base_dir is passed, the check doesn't run. (Used by the
        // default diag_codes helper.)
        let codes = diag_codes("include nonexistent.mk\n");
        assert!(!codes.contains(&"missing-include-file".to_string()));
    }

    #[test]
    fn test_missing_include_file_message() {
        let dir = tempfile::tempdir().unwrap();
        let diags = diags_with_dir("include config.mk\n", dir.path());
        let missing: Vec<_> = diags
            .iter()
            .filter(|d| d.code == Some(NumberOrString::String("missing-include-file".to_string())))
            .collect();
        assert_eq!(missing.len(), 1);
        assert_eq!(
            missing[0].message,
            "included file 'config.mk' does not exist"
        );
        assert_eq!(missing[0].severity, Some(DiagnosticSeverity::WARNING));
    }

    // Empty rule probably phony tests

    #[test]
    fn test_empty_rule_flagged() {
        let text = "clean:\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"empty-rule-probably-phony".to_string()));
    }

    #[test]
    fn test_rule_with_recipe_not_flagged() {
        let text = "clean:\n\trm -f *.o\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"empty-rule-probably-phony".to_string()));
    }

    #[test]
    fn test_rule_with_prereqs_not_flagged() {
        // `all: build test` is a meta-target — common and fine.
        let text = "all: build test\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"empty-rule-probably-phony".to_string()));
    }

    #[test]
    fn test_phony_special_target_not_flagged() {
        let text = ".PHONY:\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"empty-rule-probably-phony".to_string()));
    }

    #[test]
    fn test_pattern_rule_empty_ok() {
        let text = "%.o:\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"empty-rule-probably-phony".to_string()));
    }

    #[test]
    fn test_already_phony_not_flagged() {
        let text = ".PHONY: clean\nclean:\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"empty-rule-probably-phony".to_string()));
    }

    #[test]
    fn test_empty_rule_message() {
        let text = "clean:\n";
        let diags = get_diags(text);
        let hints: Vec<_> = diags
            .iter()
            .filter(|d| {
                d.code
                    == Some(NumberOrString::String(
                        "empty-rule-probably-phony".to_string(),
                    ))
            })
            .collect();
        assert_eq!(hints.len(), 1);
        assert!(hints[0].message.contains("clean"));
        assert!(hints[0].message.contains(".PHONY"));
        assert_eq!(hints[0].severity, Some(DiagnosticSeverity::HINT));
    }

    // Orphan recipe line tests

    #[test]
    fn test_orphan_recipe_at_top() {
        let text = "\techo orphan\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"orphan-recipe-line".to_string()));
    }

    #[test]
    fn test_orphan_recipe_after_variable() {
        let text = "VAR = 1\n\techo orphan\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"orphan-recipe-line".to_string()));
    }

    #[test]
    fn test_space_indented_recipe_continuation_ok() {
        let text = "all:\n\techo a \\\n    b\n";
        assert_eq!(diag_codes(text), Vec::<String>::new());
    }

    #[test]
    fn test_spaces_instead_of_tab_range() {
        let diags = get_diags("all:\n    echo done\n");
        assert_eq!(
            diags
                .iter()
                .filter(
                    |d| d.code == Some(NumberOrString::String("spaces-instead-of-tab".to_string()))
                )
                .map(|d| d.range)
                .collect::<Vec<_>>(),
            vec![Range::new(Position::new(1, 0), Position::new(1, 4))]
        );
    }

    #[test]
    fn test_spaces_instead_of_tab_range_continued_line() {
        let diags = get_diags("all:\n    echo a \\\n      b \\\n  c\n");
        assert_eq!(
            diags
                .iter()
                .filter(
                    |d| d.code == Some(NumberOrString::String("spaces-instead-of-tab".to_string()))
                )
                .map(|d| d.range)
                .collect::<Vec<_>>(),
            vec![Range::new(Position::new(1, 0), Position::new(1, 4))]
        );
    }

    #[test]
    fn test_line_without_separator_is_not_a_target() {
        let diags = get_diags("all:\n    echo hi\n    echo hi\n\techo\n");
        assert_eq!(
            diags
                .iter()
                .map(|d| (d.code.clone(), d.range))
                .collect::<Vec<_>>(),
            vec![
                (
                    Some(NumberOrString::String("spaces-instead-of-tab".to_string())),
                    Range::new(Position::new(1, 0), Position::new(1, 4))
                ),
                (
                    Some(NumberOrString::String("spaces-instead-of-tab".to_string())),
                    Range::new(Position::new(2, 0), Position::new(2, 4))
                ),
            ]
        );
    }

    #[test]
    fn test_automatic_variables_in_value_not_empty_references() {
        let text = "Z = $@ $< $(@D)\nall:\n\techo $(Z)\n";
        assert_eq!(diag_codes(text), Vec::<String>::new());
    }

    #[test]
    fn test_unused_define_names_variable() {
        let diags = get_diags("define FOO\nbar\nendef\n");
        assert_eq!(
            diags
                .iter()
                .map(|d| (d.message.as_str(), d.range))
                .collect::<Vec<_>>(),
            vec![(
                "variable 'FOO' is defined but never used",
                Range::new(Position::new(0, 7), Position::new(0, 10))
            )]
        );
    }

    #[test]
    fn test_recipe_inside_rule_ok() {
        let text = "all:\n\techo good\n\techo also-good\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"orphan-recipe-line".to_string()));
    }

    #[test]
    fn test_orphan_recipe_between_rules() {
        let text = "all:\n\techo good\n\nVAR = 2\n\techo orphan\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"orphan-recipe-line".to_string()));
    }

    #[test]
    fn test_orphan_recipe_message() {
        let text = "\techo orphan\n";
        let diags = get_diags(text);
        let orphans: Vec<_> = diags
            .iter()
            .filter(|d| d.code == Some(NumberOrString::String("orphan-recipe-line".to_string())))
            .collect();
        assert_eq!(orphans.len(), 1);
        assert_eq!(
            orphans[0].message,
            "recipe line is not attached to any target"
        );
        assert_eq!(orphans[0].severity, Some(DiagnosticSeverity::ERROR));
    }

    fn orphan_ranges(text: &str) -> Vec<Range> {
        get_diags(text)
            .into_iter()
            .filter(|d| d.code == Some(NumberOrString::String("orphan-recipe-line".to_string())))
            .map(|d| d.range)
            .collect()
    }

    #[test]
    fn test_orphan_recipe_range() {
        assert_eq!(
            orphan_ranges("VAR = 1\n\techo orphan\n"),
            vec![Range::new(Position::new(1, 0), Position::new(1, 12))]
        );
    }

    #[test]
    fn test_orphan_recipe_range_crlf() {
        assert_eq!(
            orphan_ranges("VAR = 1\r\n\techo orphan\r\n"),
            vec![Range::new(Position::new(1, 0), Position::new(1, 12))]
        );
    }

    #[test]
    fn test_orphan_recipe_range_continued_line() {
        assert_eq!(
            orphan_ranges("VAR = 1\n\techo a \\\n\t  b\n"),
            vec![Range::new(Position::new(1, 0), Position::new(2, 4))]
        );
    }

    #[test]
    fn test_spaces_instead_of_tab_range_crlf_continued_line() {
        let diags = get_diags("all:\r\n    echo a \\\r\n  b\r\n");
        assert_eq!(
            diags
                .iter()
                .filter(
                    |d| d.code == Some(NumberOrString::String("spaces-instead-of-tab".to_string()))
                )
                .map(|d| d.range)
                .collect::<Vec<_>>(),
            vec![Range::new(Position::new(1, 0), Position::new(1, 4))]
        );
    }

    #[test]
    fn test_multiple_orphan_recipes() {
        let text = "\techo a\n\techo b\n";
        let diags = get_diags(text);
        let orphans: Vec<_> = diags
            .iter()
            .filter(|d| d.code == Some(NumberOrString::String("orphan-recipe-line".to_string())))
            .collect();
        assert_eq!(orphans.len(), 2);
    }

    // Mixed assignment operator tests

    #[test]
    fn test_mixed_recursive_and_immediate() {
        let text = "FOO = a\nFOO := b\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"mixed-assignment-operators".to_string()));
    }

    #[test]
    fn test_same_recursive_twice_not_mixed() {
        let text = "FOO = a\nFOO = b\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"mixed-assignment-operators".to_string()));
    }

    #[test]
    fn test_immediate_and_append_ok() {
        let text = "FOO := a\nFOO += b\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"mixed-assignment-operators".to_string()));
    }

    #[test]
    fn test_recursive_and_conditional_ok() {
        let text = "FOO = a\nFOO ?= b\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"mixed-assignment-operators".to_string()));
    }

    #[test]
    fn test_immediate_triple_colon_mix() {
        let text = "FOO = a\nFOO ::= b\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"mixed-assignment-operators".to_string()));
    }

    #[test]
    fn test_different_variables_not_mixed() {
        let text = "FOO = a\nBAR := b\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"mixed-assignment-operators".to_string()));
    }

    #[test]
    fn test_mixed_assignment_flags_both() {
        let text = "FOO = a\nFOO := b\n";
        let diags = get_diags(text);
        let mixed: Vec<_> = diags
            .iter()
            .filter(|d| {
                d.code
                    == Some(NumberOrString::String(
                        "mixed-assignment-operators".to_string(),
                    ))
            })
            .collect();
        assert_eq!(mixed.len(), 2);
        assert!(mixed[0].message.contains("FOO"));
        assert_eq!(mixed[0].severity, Some(DiagnosticSeverity::WARNING));
    }

    fn mixed_assignment_lines(text: &str) -> Vec<u32> {
        get_diags(text)
            .iter()
            .filter(|d| {
                d.code
                    == Some(NumberOrString::String(
                        "mixed-assignment-operators".to_string(),
                    ))
            })
            .map(|d| d.range.start.line)
            .collect()
    }

    #[test]
    fn test_mixed_assignment_in_exclusive_branches_ok() {
        let text = "ifdef X\nFOO = a\nelse\nFOO := b\nendif\n";
        assert_eq!(mixed_assignment_lines(text), Vec::<u32>::new());
    }

    #[test]
    fn test_mixed_assignment_in_same_branch() {
        let text = "ifdef X\nFOO = a\nFOO := b\nendif\n";
        assert_eq!(mixed_assignment_lines(text), vec![1, 2]);
    }

    #[test]
    fn test_mixed_assignment_outside_and_inside_conditional() {
        let text = "FOO = a\nifdef X\nFOO := b\nelse\nFOO = c\nendif\n";
        assert_eq!(mixed_assignment_lines(text), vec![0, 2]);
    }

    #[test]
    fn test_mixed_assignment_in_nested_exclusive_branches_ok() {
        let text = "ifdef X\nifdef Y\nFOO = a\nendif\nelse\nFOO := b\nendif\n";
        assert_eq!(mixed_assignment_lines(text), Vec::<u32>::new());
    }

    #[test]
    fn test_mixed_assignment_in_source_order() {
        let text = "A = 1\nB = 1\nC = 1\nA := 2\nB := 2\nC := 2\n";
        for _ in 0..20 {
            assert_eq!(mixed_assignment_lines(text), vec![0, 1, 2, 3, 4, 5]);
        }
    }

    // Unterminated conditional tests

    #[test]
    fn test_unterminated_ifdef() {
        let text = "ifdef DEBUG\nFOO = bar\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"unterminated-conditional".to_string()));
    }

    #[test]
    fn test_terminated_ifdef_ok() {
        let text = "ifdef DEBUG\nFOO = bar\nendif\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"unterminated-conditional".to_string()));
    }

    #[test]
    fn test_unterminated_ifeq() {
        let text = "ifeq ($(CC),gcc)\nFOO = bar\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"unterminated-conditional".to_string()));
    }

    #[test]
    fn test_unterminated_with_else() {
        let text = "ifdef DEBUG\nFOO = bar\nelse\nFOO = baz\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"unterminated-conditional".to_string()));
    }

    #[test]
    fn test_unterminated_nested_outer() {
        // Inner is closed, outer is not.
        let text = "ifdef OUTER\nifdef INNER\nFOO = bar\nendif\n";
        let codes = diag_codes(text);
        let count = codes
            .iter()
            .filter(|c| c.as_str() == "unterminated-conditional")
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_bare_else_not_double_flagged() {
        // Bare `else` already produces a parse error; we should not also flag it
        // as unterminated (it has no conditional_type).
        let text = "else\nFOO = bar\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"unterminated-conditional".to_string()));
    }

    #[test]
    fn test_unterminated_conditional_message() {
        let text = "ifdef DEBUG\nFOO = bar\n";
        let diags = get_diags(text);
        let unt: Vec<_> = diags
            .iter()
            .filter(|d| {
                d.code
                    == Some(NumberOrString::String(
                        "unterminated-conditional".to_string(),
                    ))
            })
            .collect();
        assert_eq!(unt.len(), 1);
        assert_eq!(unt[0].message, "'ifdef' is missing a matching 'endif'");
        assert_eq!(
            unt[0].range,
            Range::new(Position::new(0, 0), Position::new(1, 0))
        );
        assert_eq!(unt[0].severity, Some(DiagnosticSeverity::ERROR));
    }

    fn malformed_conditions(parsed: &Parse<Makefile>, text: &str) -> Vec<(Range, String)> {
        get_diagnostics(text, parsed, None)
            .into_iter()
            .filter(|d| d.code == Some(NumberOrString::String("malformed-condition".to_string())))
            .map(|d| (d.range, d.message))
            .collect()
    }

    fn lsp_range(line: u32, start: u32, end: u32) -> Range {
        Range::new(Position::new(line, start), Position::new(line, end))
    }

    #[test]
    fn test_malformed_bsd_condition() {
        let text = ".if a & b\nX = 1\n.elif (x\n.endif\n";
        assert_eq!(
            malformed_conditions(&Makefile::parse(text), text),
            vec![
                (
                    lsp_range(0, 4, 9),
                    "malformed condition: unknown operator \"&\"".to_string()
                ),
                (
                    lsp_range(2, 6, 8),
                    "malformed condition: unclosed \"(\"".to_string()
                ),
            ]
        );
    }

    #[test]
    fn test_malformed_bsd_ifdef_condition() {
        let text = ".ifdef A ||\n.endif\n";
        assert_eq!(
            malformed_conditions(&Makefile::parse(text), text),
            vec![(
                lsp_range(0, 7, 11),
                "malformed condition: missing operand".to_string()
            )]
        );
    }

    #[test]
    fn test_malformed_nmake_condition() {
        let text = "!IF $(X) ==\n!ELSEIF DEFINED(X)\n!ENDIF\n";
        let parsed = Makefile::parse_with_variant(text, MakefileVariant::NMake);
        assert_eq!(
            malformed_conditions(&parsed, text),
            vec![(
                lsp_range(0, 4, 11),
                "malformed condition: missing operand".to_string()
            )]
        );
    }

    #[test]
    fn test_well_formed_conditions_ok() {
        let text = ".if defined(A) && ${B} == 1\n.elifdef C\n.endif\nifdef D\nendif\n";
        assert_eq!(malformed_conditions(&Makefile::parse(text), text), vec![]);
    }

    #[test]
    fn test_missing_condition_not_double_flagged() {
        // The parser already reports a missing condition.
        let text = ".if\n.endif\n";
        assert_eq!(malformed_conditions(&Makefile::parse(text), text), vec![]);
    }

    fn missing_phony_diags(text: &str, dir: &std::path::Path) -> Vec<Diagnostic> {
        diags_with_dir(text, dir)
            .into_iter()
            .filter(|d| d.code == Some(NumberOrString::String("missing-phony".to_string())))
            .collect()
    }

    #[test]
    fn test_missing_phony() {
        let dir = tempfile::tempdir().unwrap();
        let diags = missing_phony_diags(
            "all: foo\nfoo:\n\ttouch foo\nclean:\n\trm -f foo\n",
            dir.path(),
        );
        assert_eq!(diags.len(), 2);
        assert_eq!(
            diags[0].message,
            "target 'all' is conventionally not a file; declare it .PHONY"
        );
        assert_eq!(diags[0].severity, Some(DiagnosticSeverity::HINT));
        assert_eq!(
            diags[0].range,
            Range::new(Position::new(0, 0), Position::new(0, 3))
        );
        assert_eq!(
            diags[1].range,
            Range::new(Position::new(3, 0), Position::new(3, 5))
        );
    }

    #[test]
    fn test_missing_phony_second_target() {
        let dir = tempfile::tempdir().unwrap();
        let diags = missing_phony_diags(".PHONY: clean\nclean distclean:\n\trm -f x\n", dir.path());
        assert_eq!(diags.len(), 1);
        assert_eq!(
            diags[0].range,
            Range::new(Position::new(1, 6), Position::new(1, 15))
        );
    }

    #[test]
    fn test_missing_phony_variable_in_target_ok() {
        let dir = tempfile::tempdir().unwrap();
        let text = "check$(EXEEXT): foo\n\ttouch $@\n";
        assert_eq!(missing_phony_diags(text, dir.path()).len(), 0);
    }

    #[test]
    fn test_missing_phony_reported_once() {
        let dir = tempfile::tempdir().unwrap();
        let diags = missing_phony_diags("clean:: a\n\trm a\nclean:: b\n\trm b\n", dir.path());
        assert_eq!(diags.len(), 1);
    }

    #[test]
    fn test_missing_phony_declared_ok() {
        let dir = tempfile::tempdir().unwrap();
        let text = ".PHONY: all clean\nall: foo\nclean:\n\trm -f foo\n";
        assert_eq!(missing_phony_diags(text, dir.path()).len(), 0);
    }

    #[test]
    fn test_missing_phony_file_exists_ok() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("install"), "").unwrap();
        let text = "install: foo\n\tcp foo /usr/bin\n";
        assert_eq!(missing_phony_diags(text, dir.path()).len(), 0);
    }

    #[test]
    fn test_missing_phony_unconventional_name_ok() {
        let dir = tempfile::tempdir().unwrap();
        let text = "foo: bar\n\ttouch foo\nbuild: foo\n\tmkdir build\ndocs: foo\n\tmkdir docs\n";
        assert_eq!(missing_phony_diags(text, dir.path()).len(), 0);
    }

    #[test]
    fn test_missing_phony_empty_rule_left_to_other_check() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(missing_phony_diags("clean:\n", dir.path()).len(), 0);
    }

    #[test]
    fn test_missing_phony_variable_in_phony_ok() {
        let dir = tempfile::tempdir().unwrap();
        let text = "PHONIES = clean\n.PHONY: $(PHONIES)\nclean:\n\trm -f x\n";
        assert_eq!(missing_phony_diags(text, dir.path()).len(), 0);
    }

    #[test]
    fn test_missing_phony_include_without_phony_ok() {
        let dir = tempfile::tempdir().unwrap();
        let text = "-include common.mk\nclean:\n\trm -f x\n";
        assert_eq!(missing_phony_diags(text, dir.path()).len(), 0);
        let text = "ifdef X\ninclude common.mk\nendif\nclean:\n\trm -f x\n";
        assert_eq!(missing_phony_diags(text, dir.path()).len(), 0);
    }

    #[test]
    fn test_missing_phony_include_with_phony() {
        let dir = tempfile::tempdir().unwrap();
        let text = "-include common.mk\n.PHONY: all\nall: clean\nclean:\n\trm -f x\n";
        assert_eq!(missing_phony_diags(text, dir.path()).len(), 1);
    }

    #[test]
    fn test_missing_phony_needs_base_dir() {
        let codes = diag_codes("clean:\n\trm -f x\n");
        assert!(!codes.contains(&"missing-phony".to_string()));
    }
    fn file_set_codes(fx: &crate::workspace::tests::Fixture, name: &str) -> Vec<String> {
        let (mut ws, makefile) = fx.open("Makefile");
        ws.file_set(&makefile).unwrap();
        let uri = if name == "Makefile" {
            makefile
        } else {
            fx.open_in(&mut ws, name)
        };
        let mut codes: Vec<String> = get_file_set_diagnostics(&ws.file_set(&uri).unwrap())
            .into_iter()
            .filter_map(|d| d.code)
            .map(|c| match c {
                NumberOrString::String(s) => s,
                NumberOrString::Number(n) => n.to_string(),
            })
            .collect();
        codes.sort();
        codes
    }

    #[test]
    fn test_cross_file_variables() {
        let fx = crate::workspace::tests::Fixture::new(&[
            (
                "Makefile",
                "TOOL = x\ninclude rules.mk\nall:\n\t$(FROM_RULES)\n",
            ),
            ("rules.mk", "FROM_RULES = y\nOUT = $(TOOL)\n"),
        ]);
        let empty: Vec<String> = vec![];
        // Without the includer, rules.mk would report undefined-variable for
        // TOOL and unused-variable for FROM_RULES and OUT.
        assert_eq!(file_set_codes(&fx, "Makefile"), empty);
        assert_eq!(file_set_codes(&fx, "rules.mk"), vec!["unused-variable"]);
    }

    #[test]
    fn test_cross_file_export_directive() {
        let fx = crate::workspace::tests::Fixture::new(&[
            ("Makefile", "Z = 1\ninclude rules.mk\n"),
            ("rules.mk", "export Z\n"),
        ]);
        let empty: Vec<String> = vec![];
        assert_eq!(file_set_codes(&fx, "Makefile"), empty);
        assert_eq!(file_set_codes(&fx, "rules.mk"), empty);

        let fx = crate::workspace::tests::Fixture::new(&[
            ("Makefile", "Z = 1\ninclude rules.mk\n"),
            ("rules.mk", ".EXPORT_ALL_VARIABLES:\n"),
        ]);
        assert_eq!(file_set_codes(&fx, "Makefile"), empty);
    }

    #[test]
    fn test_cross_file_phony_targets() {
        let fx = crate::workspace::tests::Fixture::new(&[
            (
                "Makefile",
                "include rules.mk\n.PHONY: all build\nall: lint\n",
            ),
            ("rules.mk", ".PHONY: lint\nbuild:\n\techo\nlint:\n"),
        ]);
        let empty: Vec<String> = vec![];
        assert_eq!(file_set_codes(&fx, "Makefile"), empty);
        assert_eq!(file_set_codes(&fx, "rules.mk"), empty);
    }

    #[test]
    fn test_cross_file_missing_phony() {
        let fx = crate::workspace::tests::Fixture::new(&[
            (
                "Makefile",
                "include rules.mk\n.PHONY: all install\nall: x\n\techo\nclean:\n\trm -f x\n",
            ),
            ("rules.mk", ".PHONY: clean\ninstall: x\n\tcp x /usr/bin\n"),
        ]);
        for name in ["Makefile", "rules.mk"] {
            let codes = file_set_codes(&fx, name);
            assert!(
                !codes.contains(&"missing-phony".to_string()),
                "{name}: {codes:?}"
            );
        }
    }

    #[test]
    fn test_unreadable_include_file() {
        let fx = crate::workspace::tests::Fixture::new(&[("Makefile", "include bad.mk\n")]);
        std::fs::write(fx.path("bad.mk"), [0xff]).unwrap();
        let (mut ws, uri) = fx.open("Makefile");
        let diags = get_file_set_diagnostics(&ws.file_set(&uri).unwrap());
        let messages: Vec<String> = diags.into_iter().map(|d| d.message).collect();
        assert_eq!(
            messages,
            vec![format!(
                "included file '{}' could not be read: stream did not contain valid UTF-8",
                fx.path("bad.mk").display()
            )]
        );
    }

    fn unresolved_prereq_messages(text: &str, dir: &std::path::Path) -> Vec<String> {
        diags_with_dir(text, dir)
            .into_iter()
            .filter(|d| {
                d.code
                    == Some(NumberOrString::String(
                        "unresolved-prerequisite".to_string(),
                    ))
            })
            .map(|d| d.message)
            .collect()
    }

    #[test]
    fn test_unresolved_prerequisite() {
        let dir = tempfile::tempdir().unwrap();
        let text = "prog: main.o helper\n\tcc -o $@ $^\n";
        let diags: Vec<_> = diags_with_dir(text, dir.path())
            .into_iter()
            .filter(|d| {
                d.code
                    == Some(NumberOrString::String(
                        "unresolved-prerequisite".to_string(),
                    ))
            })
            .collect();
        assert_eq!(diags.len(), 2);
        assert_eq!(
            diags[0].message,
            "no rule to make prerequisite 'main.o', and no such file exists"
        );
        assert_eq!(diags[0].severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(
            diags[0].range,
            Range::new(Position::new(0, 6), Position::new(0, 12))
        );
        assert_eq!(
            diags[1].range,
            Range::new(Position::new(0, 13), Position::new(0, 19))
        );
    }

    #[test]
    fn test_unresolved_order_only_prerequisite() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            unresolved_prereq_messages("out: | builddir\n\ttouch out\n", dir.path()),
            vec!["no rule to make prerequisite 'builddir', and no such file exists".to_string()]
        );
    }

    #[test]
    fn test_unresolved_prerequisite_resolved_by_target_or_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("input.txt"), "").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/data"), "").unwrap();
        let text = concat!(
            ".PHONY: check\n",
            "all: gen input.txt ./input.txt sub/data sub check | sub\n",
            "gen:\n\ttouch gen\n",
            "check:\n",
        );
        assert_eq!(
            unresolved_prereq_messages(text, dir.path()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn test_unresolved_prerequisite_resolved_by_pattern_rule() {
        let dir = tempfile::tempdir().unwrap();
        let text = "prog: main.o\n\tcc -o $@ $^\n%.o: %.c\n\tcc -c $<\n";
        assert_eq!(
            unresolved_prereq_messages(text, dir.path()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn test_unresolved_prerequisite_resolved_by_builtin_rule() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.c"), "").unwrap();
        std::fs::write(dir.path().join("tool.c"), "").unwrap();
        let text = "prog: main.o tool\n\tcc -o $@ main.o\n";
        assert_eq!(
            unresolved_prereq_messages(text, dir.path()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn test_unresolved_prerequisite_variable_target() {
        let dir = tempfile::tempdir().unwrap();
        let text = "PROG = foo bar\nall: foo bar baz\n$(PROG):\n\ttouch $@\n";
        assert_eq!(
            unresolved_prereq_messages(text, dir.path()),
            vec!["no rule to make prerequisite 'baz', and no such file exists".to_string()]
        );
    }

    #[test]
    fn test_unresolved_prerequisite_single_character_variable_target() {
        let dir = tempfile::tempdir().unwrap();
        let text = "P = foo bar\nall: foo bar baz\n$P:\n\ttouch $@\n";
        assert_eq!(
            unresolved_prereq_messages(text, dir.path()),
            vec!["no rule to make prerequisite 'baz', and no such file exists".to_string()]
        );
    }

    #[test]
    fn test_unresolved_prerequisite_skipped_for_unresolvable_target() {
        let dir = tempfile::tempdir().unwrap();
        let text = "PROG = $(NAME)$(EXT)\nall: foo\n$(PROG):\n\ttouch $@\n";
        assert_eq!(
            unresolved_prereq_messages(text, dir.path()),
            Vec::<String>::new()
        );
        let text = "all: foo\n$(UNDEFINED):\n\ttouch $@\n";
        assert_eq!(
            unresolved_prereq_messages(text, dir.path()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn test_unresolved_prerequisite_skipped_names() {
        let dir = tempfile::tempdir().unwrap();
        let text = concat!(
            "all: $(OBJS) *.c foo?.c [ab].c -lm ~/x lib.a(x.o) a\\#b\n",
            "\ttouch all\n",
            "%.o: %.h\n",
            ".SUFFIXES: .x\n",
            ".PRECIOUS: missing\n",
            ".c.o:\n\tcc -c $<\n",
        );
        assert_eq!(
            unresolved_prereq_messages(text, dir.path()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn test_unresolved_prerequisite_dot_directory_target() {
        let dir = tempfile::tempdir().unwrap();
        let text = ".stamp/done: missing\n\ttouch $@\n";
        assert_eq!(
            unresolved_prereq_messages(text, dir.path()),
            vec!["no rule to make prerequisite 'missing', and no such file exists".to_string()]
        );
    }

    #[test]
    fn test_unresolved_prerequisite_skipped_when_rules_may_come_from_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        for text in [
            "-include deps.mk\nall: missing\n",
            "ifdef X\ninclude deps.mk\nendif\nall: missing\n",
            "vpath %.c src\nall: missing.c\n",
            "ifdef X\nvpath %.c src\nendif\nall: missing.c\n",
            "ifdef X\n$(eval $(call rules))\nendif\nall: missing\n",
            "VPATH = src\nall: missing.c\n",
            "$(eval $(call rules))\nall: missing\n",
            "all: missing\n\t$(eval $(call rules))\n",
            "$(foreach t,a b,$(call rule,$(t)))\nall: missing\n",
            ".DEFAULT:\n\t@echo $@\nall: missing\n",
            "%:\n\ttouch $@\nall: missing\n",
        ] {
            assert_eq!(
                unresolved_prereq_messages(text, dir.path()),
                Vec::<String>::new(),
                "{}",
                text
            );
        }
    }

    #[test]
    fn test_unresolved_prerequisite_info_does_not_skip() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            unresolved_prereq_messages("$(info hello)\nall: missing\n", dir.path()).len(),
            1
        );
    }

    #[test]
    fn test_unresolved_prerequisite_needs_base_dir() {
        let codes = diag_codes("all: missing\n");
        assert!(!codes.contains(&"unresolved-prerequisite".to_string()));
    }

    fn file_set_unresolved_prereqs(files: &[(&str, &str)], name: &str) -> Vec<String> {
        let fx = crate::workspace::tests::Fixture::new(files);
        let (mut ws, makefile) = fx.open("Makefile");
        ws.file_set(&makefile).unwrap();
        let uri = if name == "Makefile" {
            makefile
        } else {
            fx.open_in(&mut ws, name)
        };
        get_file_set_diagnostics(&ws.file_set(&uri).unwrap())
            .into_iter()
            .filter(|d| {
                d.code
                    == Some(NumberOrString::String(
                        "unresolved-prerequisite".to_string(),
                    ))
            })
            .map(|d| d.message)
            .collect()
    }

    #[test]
    fn test_unresolved_prerequisite_across_includes() {
        let files = [
            (
                "Makefile",
                "include rules.mk\nall: build lint missing\n\techo\n",
            ),
            (
                "rules.mk",
                ".PHONY: lint\nbuild: gen\n\techo\nlint:\ngen: all\n\techo\n",
            ),
        ];
        assert_eq!(
            file_set_unresolved_prereqs(&files, "Makefile"),
            vec!["no rule to make prerequisite 'missing', and no such file exists".to_string()]
        );
        // `all` is defined by the including makefile.
        assert_eq!(
            file_set_unresolved_prereqs(&files, "rules.mk"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn test_unresolved_prerequisite_pattern_rule_in_included_file() {
        let files = [
            (
                "Makefile",
                "include rules.mk\nprog: main.o\n\tcc -o $@ $^\n",
            ),
            ("rules.mk", "%.o: %.c\n\tcc -c $<\n"),
        ];
        assert_eq!(
            file_set_unresolved_prereqs(&files, "Makefile"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn test_unresolved_prerequisite_skipped_with_unfollowed_include() {
        for files in [
            &[("Makefile", "-include deps.mk\nall: missing\n")][..],
            &[
                ("Makefile", "include rules.mk\nall: missing\n"),
                ("rules.mk", "-include deps.mk\n"),
            ][..],
            &[
                ("Makefile", "include rules.mk\nall: missing\n"),
                ("rules.mk", "vpath %.c src\n"),
            ][..],
            &[
                ("Makefile", "include $(shell echo rules.mk)\nall: missing\n"),
                ("rules.mk", ""),
            ][..],
        ] {
            assert_eq!(
                file_set_unresolved_prereqs(files, "Makefile"),
                Vec::<String>::new(),
                "{:?}",
                files
            );
        }
        // An includer that includes a file that wasn't found.
        let files = [
            ("Makefile", "include rules.mk\n-include deps.mk\n"),
            ("rules.mk", "all: missing\n"),
        ];
        assert_eq!(
            file_set_unresolved_prereqs(&files, "rules.mk"),
            Vec::<String>::new()
        );
    }

    fn auto_var_messages(text: &str) -> Vec<String> {
        get_diags(text)
            .into_iter()
            .filter(|d| {
                d.code
                    == Some(NumberOrString::String(
                        "automatic-variable-outside-recipe".to_string(),
                    ))
            })
            .map(|d| d.message)
            .collect()
    }

    #[test]
    fn test_automatic_variable_in_immediate_assignment() {
        let diags: Vec<_> = get_diags("OBJ := $@.o\n")
            .into_iter()
            .filter(|d| {
                d.code
                    == Some(NumberOrString::String(
                        "automatic-variable-outside-recipe".to_string(),
                    ))
            })
            .collect();
        assert_eq!(diags.len(), 1);
        assert_eq!(
            diags[0].message,
            "automatic variable '$@' is only set in recipes and is empty in \
             an immediately-expanded assignment"
        );
        assert_eq!(diags[0].severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(
            diags[0].range,
            Range::new(Position::new(0, 7), Position::new(0, 9))
        );
    }

    #[test]
    fn test_automatic_variable_forms_in_immediate_assignment() {
        assert_eq!(
            auto_var_messages("X := $(@D) ${<F} $(notdir $^) $? $*\n").len(),
            5
        );
    }

    #[test]
    fn test_automatic_variable_in_other_immediate_operators() {
        assert_eq!(auto_var_messages("X ::= $<\n").len(), 1);
        assert_eq!(auto_var_messages("X :::= $<\n").len(), 1);
        assert_eq!(auto_var_messages("X != echo $@\n").len(), 1);
    }

    #[test]
    fn test_automatic_variable_in_target_list() {
        assert_eq!(
            auto_var_messages("$@: foo\n\ttouch $@\n"),
            vec![
                "automatic variable '$@' is only set in recipes and is empty in a target list"
                    .to_string()
            ]
        );
    }

    #[test]
    fn test_automatic_variable_in_prerequisites() {
        assert_eq!(
            auto_var_messages("foo: $(@D)/bar\n\ttouch $@\n"),
            vec![
                "automatic variable '$(@D)' is only set in recipes and is empty in a \
                 prerequisite list"
                    .to_string()
            ]
        );
    }

    #[test]
    fn test_automatic_variable_in_conditional() {
        assert_eq!(auto_var_messages("ifeq ($@,foo)\nX = 1\nendif\n").len(), 1);
    }

    #[test]
    fn test_automatic_variable_in_archive_members() {
        assert_eq!(
            auto_var_messages("lib.a($@): x\nall: lib.a($<)\n"),
            vec![
                "automatic variable '$@' is only set in recipes and is empty in a target list",
                "automatic variable '$<' is only set in recipes and is empty in a prerequisite list",
            ]
        );
    }

    #[test]
    fn test_automatic_variable_in_variable_name() {
        assert_eq!(
            auto_var_messages("$@X := 1\n$<Y = 1\n"),
            vec![
                "automatic variable '$@' is only set in recipes and is empty in an immediately-expanded assignment"
            ]
        );
    }

    #[test]
    fn test_automatic_variable_in_conditional_body_assignment() {
        assert_eq!(auto_var_messages("ifdef X\nY := $@\nendif\n").len(), 1);
    }

    #[test]
    fn test_automatic_variable_in_conditional_in_rule_body() {
        // GNU make treats this assignment as global, not target-specific.
        assert_eq!(
            auto_var_messages("all:\nifdef X\n\techo\nY := $@\nendif\n"),
            auto_var_messages("ifdef X\nY := $@\nendif\n")
        );
        assert_eq!(auto_var_messages("all: Y := $@\n"), Vec::<String>::new());
    }

    #[test]
    fn test_automatic_variable_in_recursive_assignment_ok() {
        assert_eq!(auto_var_messages("X = $@\nY ?= $<\nZ += $^\n").len(), 0);
    }

    #[test]
    fn test_automatic_variable_in_target_specific_variable_ok() {
        assert_eq!(auto_var_messages("foo: X := $@\nfoo: Y = $<\n").len(), 0);
    }

    #[test]
    fn test_automatic_variable_in_recipe_ok() {
        assert_eq!(
            auto_var_messages("foo: bar\n\tcc -o $@ $< $(@D)\nall: ; echo $@\n").len(),
            0
        );
    }

    #[test]
    fn test_automatic_variable_in_define_ok() {
        assert_eq!(
            auto_var_messages("define RULE\n$@: $<\n\ttouch $@\nendef\n").len(),
            0
        );
    }

    #[test]
    fn test_automatic_variable_in_eval_ok() {
        assert_eq!(auto_var_messages("$(eval foo: ; echo $@)\n").len(), 0);
        assert_eq!(auto_var_messages("X := $(call tmpl,$@)\n").len(), 0);
    }

    #[test]
    fn test_escaped_automatic_variable_ok() {
        assert_eq!(auto_var_messages("X := $$@\n").len(), 0);
    }

    #[test]
    fn test_automatic_variable_with_second_expansion_ok() {
        assert_eq!(
            auto_var_messages(".SECONDEXPANSION:\nfoo: $$(@D)/x $(@D)/y\n\ttouch $@\n").len(),
            0
        );
    }

    #[test]
    fn test_automatic_variable_substitution_reference() {
        assert_eq!(
            auto_var_messages("X := $(@:.c=.o) ${@:.c=.o} $(^:%.c=%.o) $(@D:%=%/x)\n"),
            vec![
                "automatic variable '$(@:.c=.o)' is only set in recipes and is empty in an \
                 immediately-expanded assignment",
                "automatic variable '${@:.c=.o}' is only set in recipes and is empty in an \
                 immediately-expanded assignment",
                "automatic variable '$(^:%.c=%.o)' is only set in recipes and is empty in an \
                 immediately-expanded assignment",
                "automatic variable '$(@D:%=%/x)' is only set in recipes and is empty in an \
                 immediately-expanded assignment",
            ]
        );
        assert_eq!(
            auto_var_messages("foo: $(@:.c=.o)\n\ttrue\n"),
            vec![
                "automatic variable '$(@:.c=.o)' is only set in recipes and is empty in a \
                 prerequisite list"
            ]
        );
    }

    #[test]
    fn test_automatic_variable_substitution_reference_in_recipe_ok() {
        assert_eq!(
            diag_codes("%.o: %.c\n\techo $(@:.c=.o) $(@D) $(<F) $(^:%.c=%.o) ${@:.c=.o}\n"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn test_non_automatic_single_char_variable_ok() {
        assert_eq!(auto_var_messages("X := $A $(@X) $(DD)\n").len(), 0);
    }

    fn bsd_messages(text: &str, dir: &std::path::Path, code: &str) -> Vec<String> {
        let parsed = Makefile::parse_with_variant(text, MakefileVariant::BSDMake);
        get_diagnostics(text, &parsed, Some(dir))
            .into_iter()
            .filter(|d| d.code == Some(NumberOrString::String(code.to_string())))
            .map(|d| d.message)
            .collect()
    }

    #[test]
    fn test_bsd_include_path_uses_bsd_references() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            bsd_messages(
                "TOP = sub\n.include \"${TOP:}/x.mk\"\n",
                dir.path(),
                "missing-include-file"
            ),
            vec!["included file '${TOP:}/x.mk' does not exist".to_string()]
        );
    }

    #[test]
    fn test_bsd_target_uses_bsd_references() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            bsd_messages(
                "PROG = foo\n${PROG:}:\n\ttouch foo\nall: foo missing\n",
                dir.path(),
                "unresolved-prerequisite"
            ),
            vec!["no rule to make prerequisite 'missing', and no such file exists".to_string()]
        );
    }

    #[test]
    fn test_bsd_automatic_variable_uses_bsd_references() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            bsd_messages(
                "X != echo ${@:}\n",
                dir.path(),
                "automatic-variable-outside-recipe"
            ),
            vec![
                "automatic variable '${@:}' is only set in recipes and is empty in \
                 an immediately-expanded assignment"
                    .to_string()
            ]
        );
    }

    /// Codes of the diagnostics for `text`, which `Makefile::parse` detects
    /// as an nmake makefile.
    fn nmake_codes(text: &str, dir: &std::path::Path) -> Vec<String> {
        let parsed = Makefile::parse(text);
        assert_eq!(parsed.variant(), Some(MakefileVariant::NMake));
        get_diagnostics(text, &parsed, Some(dir))
            .into_iter()
            .filter_map(|d| d.code)
            .map(|c| match c {
                NumberOrString::String(s) => s,
                NumberOrString::Number(n) => n.to_string(),
            })
            .collect()
    }

    #[test]
    fn test_nmake_no_phony_checks() {
        // nmake has no .PHONY; a target that is not a file is always built.
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            nmake_codes(
                "!IFDEF X\n!ENDIF\nall: clean\nclean:\n\tdel x\nempty:\n",
                dir.path()
            ),
            Vec::<String>::new()
        );
        assert_eq!(
            nmake_codes(
                "!IFDEF X\n!ENDIF\n.PHONY: missing unused\nunused:\n",
                dir.path()
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn test_nmake_shell_not_flagged() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            nmake_codes(
                "!IFDEF X\n!ENDIF\nS = $(shell ls)\nall:\n\techo $(S)\n",
                dir.path()
            ),
            // nmake has no functions, so `$(shell ls)` is not a call.
            vec!["undefined-variable".to_string()]
        );
    }

    #[test]
    fn test_bsd_shell_not_flagged() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            bsd_messages(
                "S = $(shell ls)\n",
                dir.path(),
                "shell-in-recursive-assignment"
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn test_bsd_trailing_whitespace_not_flagged() {
        // BSD make strips trailing whitespace from the value.
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            bsd_messages("A = b  \n", dir.path(), "trailing-whitespace-in-value"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn test_bsd_target_variables_in_prerequisites() {
        // BSD make sets .TARGET, .PREFIX, .ARCHIVE and .MEMBER for the
        // sources of a dependency line, but not .IMPSRC.
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            bsd_messages(
                "all: $@.c $(@F).h $*.o $<.x\n",
                dir.path(),
                "automatic-variable-outside-recipe"
            ),
            vec![
                "automatic variable '$<' is only set in recipes and is empty in \
                 a prerequisite list"
                    .to_string()
            ]
        );
    }

    #[test]
    fn test_bsd_automatic_variable_in_immediate_assignment() {
        // BSD make does not expand references to undefined variables in a
        // `:=` assignment, so they are expanded when the variable is used.
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            bsd_messages(
                "X := $@ $<\n",
                dir.path(),
                "automatic-variable-outside-recipe"
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn test_nmake_target_as_dependent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            nmake_codes(
                "!IFDEF X\n!ENDIF\na.obj: $$@.c $$(@F).h\n\tcl $**\n",
                dir.path()
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn test_nmake_predefined_macros() {
        let dir = tempfile::tempdir().unwrap();
        let text = "!IFDEF X\n!ENDIF\nD = $(MAKEDIR) $(CFLAGS) $(RFLAGS) $(CURDIR)\n\
                    all: $$(@B).c\n\techo $(D)\n";
        let parsed = Makefile::parse(text);
        assert_eq!(parsed.variant(), Some(MakefileVariant::NMake));
        let messages: Vec<String> = get_diagnostics(text, &parsed, Some(dir.path()))
            .into_iter()
            .filter(|d| d.code == Some(NumberOrString::String("undefined-variable".to_string())))
            .map(|d| d.message)
            .collect();
        assert_eq!(
            messages,
            vec!["variable 'CURDIR' is not defined".to_string()]
        );
    }

    #[test]
    fn test_bsd_builtin_variables() {
        let dir = tempfile::tempdir().unwrap();
        let text = "D = ${.CURDIR} ${.OBJDIR} ${.PARSEDIR}/${.PARSEFILE} ${.MAKE.LEVEL}\n\
                    M = ${MAKE} ${.MAKE} ${.MAKEFLAGS} ${MACHINE} ${MACHINE_ARCH} ${.MAKE.OS}\n\
                    N = ${.SHELL} ${.newline} ${.INCLUDEDFROMDIR} ${CC} ${CFLAGS}\n\
                    G = $(CURDIR) $(MAKECMDGOALS) ${UNKNOWN}\n\
                    all: ${D} ${M} ${N} ${G}\n";
        assert_eq!(
            bsd_messages(text, dir.path(), "undefined-variable"),
            vec![
                "variable 'CURDIR' is not defined".to_string(),
                "variable 'MAKECMDGOALS' is not defined".to_string(),
                "variable 'UNKNOWN' is not defined".to_string(),
            ]
        );
    }

    #[test]
    fn test_bsd_local_variables() {
        let dir = tempfile::tempdir().unwrap();
        let text = "X = ${.TARGET} ${.ALLSRC} ${.IMPSRC} ${.OODATE} ${.PREFIX} ${.MEMBER} \
                    ${.ARCHIVE} $@ $> $< $? $* $% $! ${@D} ${>F} ${^}\nall: ${X}\n";
        assert_eq!(
            bsd_messages(text, dir.path(), "undefined-variable"),
            vec!["variable '^' is not defined".to_string()]
        );
    }
}
