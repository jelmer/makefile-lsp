//! Built-in GNU Make functions, variables, and targets.
//!
//! Single authoritative source for all built-in knowledge used across
//! diagnostics, completions, hover, and signature help.

/// A built-in GNU Make function.
pub struct BuiltinFunction {
    /// Function name (e.g. "subst").
    pub name: &'static str,
    /// Short description.
    pub doc: &'static str,
    /// Parameter labels for signature help.
    pub params: &'static [&'static str],
}

/// All built-in GNU Make functions.
pub const BUILTIN_FUNCTIONS: &[BuiltinFunction] = &[
    BuiltinFunction {
        name: "subst",
        doc: "Replace occurrences of *from* with *to* in *text*.",
        params: &["from", "to", "text"],
    },
    BuiltinFunction {
        name: "patsubst",
        doc: "Pattern substitution: replace *pattern* with *replacement* in *text*.",
        params: &["pattern", "replacement", "text"],
    },
    BuiltinFunction {
        name: "strip",
        doc: "Remove leading and trailing whitespace.",
        params: &["string"],
    },
    BuiltinFunction {
        name: "findstring",
        doc: "Search for *find* in *in*. Returns *find* if found, empty otherwise.",
        params: &["find", "in"],
    },
    BuiltinFunction {
        name: "filter",
        doc: "Keep only words in *text* matching any *pattern*.",
        params: &["pattern...", "text"],
    },
    BuiltinFunction {
        name: "filter-out",
        doc: "Remove words in *text* matching any *pattern*.",
        params: &["pattern...", "text"],
    },
    BuiltinFunction {
        name: "sort",
        doc: "Sort words and remove duplicates.",
        params: &["list"],
    },
    BuiltinFunction {
        name: "word",
        doc: "Extract the *n*th word (1-based) from *text*.",
        params: &["n", "text"],
    },
    BuiltinFunction {
        name: "wordlist",
        doc: "Extract words *s* through *e* (1-based) from *text*.",
        params: &["s", "e", "text"],
    },
    BuiltinFunction {
        name: "words",
        doc: "Count the number of words.",
        params: &["text"],
    },
    BuiltinFunction {
        name: "firstword",
        doc: "Return the first word.",
        params: &["names..."],
    },
    BuiltinFunction {
        name: "lastword",
        doc: "Return the last word.",
        params: &["names..."],
    },
    BuiltinFunction {
        name: "dir",
        doc: "Extract the directory part of file names.",
        params: &["names..."],
    },
    BuiltinFunction {
        name: "notdir",
        doc: "Extract the non-directory part of file names.",
        params: &["names..."],
    },
    BuiltinFunction {
        name: "suffix",
        doc: "Extract the suffix of file names.",
        params: &["names..."],
    },
    BuiltinFunction {
        name: "basename",
        doc: "Extract the basename of file names.",
        params: &["names..."],
    },
    BuiltinFunction {
        name: "addsuffix",
        doc: "Append *suffix* to each word in *names*.",
        params: &["suffix", "names..."],
    },
    BuiltinFunction {
        name: "addprefix",
        doc: "Prepend *prefix* to each word in *names*.",
        params: &["prefix", "names..."],
    },
    BuiltinFunction {
        name: "join",
        doc: "Join two lists pairwise.",
        params: &["list1", "list2"],
    },
    BuiltinFunction {
        name: "wildcard",
        doc: "Expand file name wildcards.",
        params: &["pattern"],
    },
    BuiltinFunction {
        name: "realpath",
        doc: "Canonical absolute path names.",
        params: &["names..."],
    },
    BuiltinFunction {
        name: "abspath",
        doc: "Absolute path names.",
        params: &["names..."],
    },
    BuiltinFunction {
        name: "if",
        doc: "If *condition* is non-empty, expand *then-part*, otherwise *else-part*.",
        params: &["condition", "then-part", "else-part"],
    },
    BuiltinFunction {
        name: "or",
        doc: "Short-circuiting OR: return the first non-empty argument.",
        params: &["cond1", "cond2..."],
    },
    BuiltinFunction {
        name: "and",
        doc: "Short-circuiting AND: return the last argument if all are non-empty.",
        params: &["cond1", "cond2..."],
    },
    BuiltinFunction {
        name: "foreach",
        doc: "Iterate: for each word in *list*, expand *text* with *var* set to the word.",
        params: &["var", "list", "text"],
    },
    BuiltinFunction {
        name: "call",
        doc: "Call a user-defined function stored in *variable* with the given parameters.",
        params: &["variable", "param..."],
    },
    BuiltinFunction {
        name: "eval",
        doc: "Evaluate *text* as makefile syntax.",
        params: &["text"],
    },
    BuiltinFunction {
        name: "origin",
        doc: "Return how *variable* was defined (undefined, default, environment, file, etc.).",
        params: &["variable"],
    },
    BuiltinFunction {
        name: "flavor",
        doc: "Return the flavor of *variable* (undefined, recursive, or simple).",
        params: &["variable"],
    },
    BuiltinFunction {
        name: "value",
        doc: "Return the unexpanded value of *variable*.",
        params: &["variable"],
    },
    BuiltinFunction {
        name: "error",
        doc: "Generate a fatal error with *text*.",
        params: &["text..."],
    },
    BuiltinFunction {
        name: "warning",
        doc: "Print a warning message.",
        params: &["text..."],
    },
    BuiltinFunction {
        name: "info",
        doc: "Print an informational message.",
        params: &["text..."],
    },
    BuiltinFunction {
        name: "shell",
        doc: "Execute *command* in a shell and return its stdout.",
        params: &["command"],
    },
    BuiltinFunction {
        name: "file",
        doc: "Read from or write to a file.",
        params: &["op filename", "text"],
    },
    BuiltinFunction {
        name: "guile",
        doc: "Evaluate GNU Guile code (if compiled with Guile support).",
        params: &["code"],
    },
    BuiltinFunction {
        name: "let",
        doc: "Bind words from *list* to *var...* and expand *text*.",
        params: &["var...", "list", "text"],
    },
];

/// Check if a name is a built-in function.
pub fn is_builtin_function(name: &str) -> bool {
    BUILTIN_FUNCTIONS.iter().any(|f| f.name == name)
}

/// Find a built-in function by name.
pub fn find_builtin_function(name: &str) -> Option<&'static BuiltinFunction> {
    BUILTIN_FUNCTIONS.iter().find(|f| f.name == name)
}

/// GNU Make automatic variables (single-character after $).
///
/// The `D`/`F` variants (`$(@D)`, `$(^F)`, ...) are in
/// [`AUTOMATIC_VARIABLE_VARIANTS`]; [`find_automatic_variable`] synthesizes
/// their descriptions from the base variable.
pub const AUTOMATIC_VARIABLES: &[(&str, &str)] = &[
    ("@", "The file name of the target of the rule."),
    ("<", "The name of the first prerequisite."),
    ("^", "The names of all the prerequisites, with spaces between them (no duplicates)."),
    ("+", "Like `$^`, but prerequisites listed more than once are duplicated in the order they were listed."),
    ("?", "The names of all the prerequisites that are newer than the target."),
    ("*", "The stem with which an implicit rule matches."),
    ("%", "The stem of a static pattern rule."),
];

/// GNU Make automatic variable variants (e.g. $(@D), $(@F)).
pub const AUTOMATIC_VARIABLE_VARIANTS: &[&str] = &[
    "@D", "@F", "<D", "<F", "^D", "^F", "+D", "+F", "?D", "?F", "*D", "*F",
];

/// Well-known GNU Make built-in variables with descriptions.
pub const BUILTIN_VARIABLES: &[(&str, &str)] = &[
    ("MAKE", "The name of the make program being run."),
    ("MAKECMDGOALS", "The targets given on the command line."),
    (
        "MAKEFLAGS",
        "Flags given to make, plus flags from MAKEFLAGS in the environment.",
    ),
    (
        "MAKEFILES",
        "Makefiles to be read on every invocation of make.",
    ),
    (
        "MAKELEVEL",
        "The number of levels of recursion (sub-makes).",
    ),
    (
        "MAKEOVERRIDES",
        "Variable definitions from the command line.",
    ),
    ("MAKESHELL", "The shell to use (Windows only)."),
    (
        "MAKE_RESTARTS",
        "The number of times this instance of make has restarted.",
    ),
    ("MAKE_TERMERR", "Non-empty if stderr is a terminal."),
    ("MAKE_TERMOUT", "Non-empty if stdout is a terminal."),
    ("MAKE_VERSION", "The version of GNU Make."),
    ("MFLAGS", "Same as MAKEFLAGS; for historical compatibility."),
    ("SHELL", "The shell used to run recipes."),
    (
        "SUFFIXES",
        "The default list of suffixes before make reads any makefiles.",
    ),
    (
        "VPATH",
        "Search path for prerequisites not found in the current directory.",
    ),
    (
        ".DEFAULT_GOAL",
        "The default goal target if none is specified.",
    ),
    (
        ".EXTRA_PREREQS",
        "Extra prerequisites to add to every target.",
    ),
    (".FEATURES", "List of supported GNU Make features."),
    (
        ".INCLUDE_DIRS",
        "Directories searched for included makefiles.",
    ),
    (".LOADED", "List of loaded dynamic objects."),
    (
        ".RECIPEPREFIX",
        "The character used instead of tab to introduce recipes.",
    ),
    (".SHELLFLAGS", "Flags passed to the shell (default: -c)."),
    (
        ".VARIABLES",
        "The names of all global variables defined so far.",
    ),
    // Implicit rule variables
    ("AR", "Archive-maintaining program (default: ar)."),
    ("ARFLAGS", "Flags for the archive program (default: rv)."),
    ("AS", "Assembler (default: as)."),
    ("ASFLAGS", "Extra flags for the assembler."),
    ("CC", "C compiler (default: cc)."),
    ("CFLAGS", "Extra flags for the C compiler."),
    ("CO", "Program for checking out RCS files (default: co)."),
    ("COFLAGS", "Extra flags for the RCS co program."),
    ("CPP", "C preprocessor (default: $(CC) -E)."),
    (
        "CPPFLAGS",
        "Extra flags for the C preprocessor and programs that use it.",
    ),
    (
        "CTANGLE",
        "Program for turning CWEB into C (default: ctangle).",
    ),
    (
        "CWEAVE",
        "Program for turning CWEB into TeX (default: cweave).",
    ),
    ("CXX", "C++ compiler (default: g++)."),
    ("CXXFLAGS", "Extra flags for the C++ compiler."),
    ("FC", "Fortran compiler (default: f77)."),
    ("FFLAGS", "Extra flags for the Fortran compiler."),
    ("GET", "Program for extracting SCCS files (default: get)."),
    ("GFLAGS", "Extra flags for the SCCS get program."),
    ("LDFLAGS", "Extra flags for the linker (ld)."),
    ("LDLIBS", "Libraries to link against."),
    ("LEX", "Lex scanner generator (default: lex)."),
    ("LFLAGS", "Extra flags for Lex."),
    ("LINT", "Lint program (default: lint)."),
    ("LINTFLAGS", "Extra flags for Lint."),
    ("M2C", "Modula-2 compiler (default: m2c)."),
    (
        "MAKEINFO",
        "Program to convert Texinfo to Info (default: makeinfo).",
    ),
    ("PC", "Pascal compiler (default: pc)."),
    ("PFLAGS", "Extra flags for the Pascal compiler."),
    ("RFLAGS", "Extra flags for the Ratfor compiler."),
    ("RM", "Command to remove a file (default: rm -f)."),
    (
        "TANGLE",
        "Program for turning WEB into Pascal (default: tangle).",
    ),
    ("TEX", "TeX program (default: tex)."),
    (
        "TEXI2DVI",
        "Program to make DVI from Texinfo (default: texi2dvi).",
    ),
    (
        "WEAVE",
        "Program for turning WEB into TeX (default: weave).",
    ),
    ("YACC", "Parser generator (default: yacc)."),
    ("YFLAGS", "Extra flags for Yacc."),
    // Common environment variables
    (
        "CURDIR",
        "The absolute pathname of the current working directory.",
    ),
    ("HOME", "The user's home directory."),
    ("PATH", "The executable search path."),
    (
        "PWD",
        "The current working directory (from the environment).",
    ),
    ("TERM", "The terminal type."),
    ("USER", "The current user's login name."),
    // Output sync
    (
        "OUTPUT_OPTION",
        "Option passed to compilers for specifying the output file.",
    ),
    (
        ".LIBPATTERNS",
        "Patterns for library name lookup (default: lib%.so lib%.a).",
    ),
];

/// Well-known GNU Make special targets.
pub const SPECIAL_TARGETS: &[(&str, &str)] = &[
    (".PHONY", "Declare targets that do not represent files."),
    (
        ".SUFFIXES",
        "Define the list of suffixes for old-fashioned suffix rules.",
    ),
    (".DEFAULT", "Recipe for targets with no other rule."),
    (
        ".PRECIOUS",
        "Targets that should not be deleted if make is interrupted.",
    ),
    (".INTERMEDIATE", "Targets treated as intermediate files."),
    (
        ".SECONDARY",
        "Targets treated as secondary (intermediate but never auto-deleted).",
    ),
    (
        ".SECONDEXPANSION",
        "Enable second expansion of prerequisites.",
    ),
    (
        ".DELETE_ON_ERROR",
        "Delete the target file if its recipe exits with a nonzero status.",
    ),
    (".IGNORE", "Ignore errors in recipes for these targets."),
    (".SILENT", "Do not echo recipes for these targets."),
    (
        ".EXPORT_ALL_VARIABLES",
        "Export all variables to child processes.",
    ),
    (".NOTPARALLEL", "Disable parallel execution of recipes."),
    (
        ".ONESHELL",
        "Run entire recipe in a single shell invocation.",
    ),
    (".POSIX", "Enable POSIX-conforming mode."),
];

/// Find an automatic variable by name, returning its description.
///
/// Handles both the variables listed in [`AUTOMATIC_VARIABLES`] and the `D`/`F`
/// variants in [`AUTOMATIC_VARIABLE_VARIANTS`] (e.g. `^D`, `?F`), synthesizing a
/// description for the latter from the base variable.
pub fn find_automatic_variable(name: &str) -> Option<String> {
    if let Some((_, doc)) = AUTOMATIC_VARIABLES.iter().find(|(n, _)| *n == name) {
        return Some((*doc).to_string());
    }
    if AUTOMATIC_VARIABLE_VARIANTS.contains(&name) {
        let (base, suffix) = name.split_at(1);
        let part = match suffix {
            "D" => "directory part",
            "F" => "file-within-directory part",
            _ => return None,
        };
        return Some(format!("The {} of `${}`.", part, base));
    }
    None
}

/// Find a built-in variable by name, returning its description.
pub fn find_builtin_variable(name: &str) -> Option<&'static str> {
    BUILTIN_VARIABLES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, doc)| *doc)
}

/// Find a special target by name, returning its description.
pub fn find_special_target(name: &str) -> Option<&'static str> {
    SPECIAL_TARGETS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, doc)| *doc)
}

/// Check if a variable name is a known built-in (variable, automatic, or function).
pub fn is_known_variable(name: &str) -> bool {
    AUTOMATIC_VARIABLES.iter().any(|(n, _)| *n == name)
        || AUTOMATIC_VARIABLE_VARIANTS.contains(&name)
        || BUILTIN_VARIABLES.iter().any(|(n, _)| *n == name)
        || is_builtin_function(name)
}
