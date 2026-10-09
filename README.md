# makefile-lsp

A Language Server Protocol (LSP) implementation for Makefiles, built in Rust.

## Features

All analysis is done on a lossless syntax tree of the makefile and the
makefiles it includes (see below); the Makefile is never executed.

- **Diagnostics** - parse errors plus a set of lint checks (listed below)
- **Code actions** - quick fixes and refactorings (listed below)
- **Completion** - directives, targets and special targets at the start of
  a line, conditional directives after `else`,
  variable names, built-in functions and variables after `$(`, automatic
  variables after `$`, targets and file paths in prerequisite lists, and file
  paths after `include`
- **Hover** - the definition of user-defined variables, documentation for
  directives such as `ifeq` and `include`, automatic variables, built-in
  variables, built-in functions and special targets such as `.PHONY`, the
  prerequisites and recipe of a target, where it is defined or referenced as a
  prerequisite, and the comment lines directly above the definition of a
  target or variable
- **Signature help** - parameter information inside built-in function calls
  like `$(subst from,to,text)`
- **Go to definition** - from a variable reference to its assignment, from a
  prerequisite to the rule defining it, and from an include path to the file
- **Find references** and **document highlights** - for targets and variables
- **Rename** - targets and variables, with prepare-rename support
- **Document links** - `include`, `-include` and `sinclude` paths are clickable
- **Inlay hints** - the value of simply-expanded (`:=`) variables at their
  references, and the dependency depth of top-level targets
- **Document symbols** - outline of targets and variable assignments
- **Folding ranges** - rules, conditionals and comment blocks
- **Selection ranges** - expand selection from a word to its expression, the
  enclosing rule, variable or conditional, and the whole file
- **Semantic tokens** - highlighting for targets, variables, prerequisites,
  recipes, directive keywords and comments
- **Formatting** - converts space-indented recipes to tabs, trims trailing
  whitespace where make ignores it, and normalizes the final newline
- **On-type formatting** - inserts a tab after pressing enter on a rule line
- **Command-line checking** - report diagnostics in CI, as text or SARIF
  (see below)
- **Command-line formatting** - format Makefiles in place, or check that they
  are formatted (see below)
- **SCIP indexing** - generate a [SCIP](https://github.com/sourcegraph/scip)
  index for code navigation (see below)

### Diagnostics

Each diagnostic carries a code, so it can be identified in editors:

| Code | Severity | Description |
|------|----------|-------------|
| `undefined-variable` | warning | reference to a variable that is never assigned and not built in (for nmake, its predefined and filename macros; for BSD make, its built-in and local variables) |
| `recursive-variable-reference` | warning | `=` assignment that references itself |
| `empty-variable-reference` | warning | `$()` or `${}` |
| `empty-automatic-variable` | warning | `$<`, `$^`, `$+` or `$?` in a rule without prerequisites, or `$*` outside a pattern rule |
| `automatic-variable-outside-recipe` | warning | automatic variable like `$@` in a target list, prerequisite list, `:=` assignment or conditional, where it is always empty (BSD make sets `$@`, `$*` and `$%` in prerequisites, and does not expand them in `:=` assignments) |
| `unused-variable` | hint | variable that is assigned but never referenced |
| `mixed-assignment-operators` | warning | variable assigned with both `=` and `:=` |
| `shell-in-recursive-assignment` | warning | `$(shell ...)` in an `=` assignment, which runs on every expansion (GNU make only) |
| `trailing-whitespace-in-value` | warning | trailing whitespace that becomes part of a variable value (not for BSD make, which strips it) |
| `duplicate-target` | warning | target defined by more than one single-colon rule |
| `mixed-rule-separator` | error | target with both `:` and `::` rules |
| `self-dependency` | warning | target that lists itself as a prerequisite |
| `circular-dependency` | warning | cycle between targets |
| `duplicate-prerequisite` | warning | prerequisite listed more than once in the same rule |
| `redundant-prerequisite` | hint | prerequisite already reached through another prerequisite |
| `unresolved-prerequisite` | warning | prerequisite that is not a target, not phony and not an existing file (skipped when rules or files may come from elsewhere, e.g. with an `include` that can't be followed or `vpath`) |
| `undefined-phony-target` | warning | `.PHONY` entry without a matching rule |
| `unused-phony-target` | warning | phony target with no recipe that nothing depends on |
| `unreferenced-phony-target` | hint | phony target that nothing depends on, other than conventional ones like `all` or `install` |
| `empty-rule-probably-phony` | hint | rule without prerequisites or recipe that should probably be phony |
| `missing-phony` | hint | conventional non-file target like `clean` or `install` that is not declared `.PHONY` |
| `spaces-instead-of-tab` | error | recipe line indented with spaces |
| `orphan-recipe-line` | error | recipe line outside of any rule |
| `invalid-shell-syntax` | warning | recipe line rejected by `sh -n` (or the shell `SHELL` names); checked on open and save only |
| `unterminated-conditional` | error | `ifdef`/`ifeq` without a matching `endif` |
| `malformed-condition` | error | BSD make `.if` or nmake `!IF` condition that does not parse |
| `include-missing-path` | error | `include` without a path |
| `missing-include-file` | warning | `include` of a file that does not exist |
| `unreadable-include-file` | warning | `include` of a file that exists but cannot be read |

nmake has no `.PHONY`, so the checks for phony targets are skipped for nmake
makefiles.

### Code actions

- Add a target to `.PHONY`, or remove an undefined one from it
- Sort the prerequisites of `.PHONY`
- Define an undefined variable
- Replace spaces with a tab in a recipe line, or in all recipe lines
- Remove trailing whitespace from a variable value
- Use `:=` for an assignment containing `$(shell ...)`
- Add a missing `endif` or `endef`
- Change `include` to `-include`, so make ignores a missing file
- Inline a variable
- Add a target that nothing depends on as a prerequisite of the default goal
- Remove a prerequisite that is already reached through another one
- Add an empty rule for a prerequisite that is neither a target nor an
  existing file

### Included makefiles

The server follows `include`, `-include` and `sinclude` directives, so
definitions, references, hover, completion, rename and diagnostics see
targets and variables from included makefiles. Paths are resolved relative
to the directory of the top-level makefile and of the including file.
Variable references in include paths are expanded when the variable has a
single plain value, as in `TOPDIR := ..` followed by `include
$(TOPDIR)/rules.mk`; other paths, wildcards and `-I` directories are not
resolved.

A fragment such as `rules.mk` opened on its own also sees the makefile that
includes it, if that makefile is open or is the `GNUmakefile`, `makefile`
or `Makefile` in the fragment's directory or one above it (up to the
workspace folder).

Open editor buffers take precedence over files on disk. Rename only edits
open files and files inside the workspace folders.

## Installation

```sh
cargo install makefile-lsp
```

Or build from source:

```sh
cargo build --release
```

## Usage

The server communicates over stdin/stdout using the LSP protocol. Configure your
editor to launch `makefile-lsp` as the language server for Makefile files.

### Neovim (nvim-lspconfig)

```lua
vim.api.nvim_create_autocmd("FileType", {
  pattern = "make",
  callback = function()
    vim.lsp.start({
      name = "makefile-lsp",
      cmd = { "makefile-lsp" },
    })
  end,
})
```

### VS Code

The `vscode-makefile` directory contains a VS Code extension that runs
`makefile-lsp`. Set `makefile.serverPath` to use a specific binary.

### coc.nvim

The `coc-make` directory contains a [coc.nvim](https://github.com/neoclide/coc.nvim)
extension; see its README for details.

## Checking from the command line

The `check` subcommand reports the same diagnostics the language server
publishes, which is useful in CI. Directories are searched recursively
(skipping hidden directories) for `Makefile`, `makefile`, `GNUmakefile`, `*.mk`
and `*.mak`; with no arguments the current directory is searched:

```sh
makefile-lsp check                       # check all Makefiles under .
makefile-lsp check Makefile build/
makefile-lsp check --format sarif > makefile-lsp.sarif
```

Like the language server, `check` follows `include` directives: variables,
targets and `.PHONY` declarations in included makefiles, and in makefiles
nearby (up to the current directory) that include a checked file, are taken
into account. Diagnostics are still only reported for the checked files. Use
`--no-follow-includes` to check each file on its own.

Diagnostics are printed as `path:line:column: severity: message [code]`. Use
`--format sarif` to produce SARIF 2.1.0, e.g. for GitHub code scanning.

Only errors and warnings are reported by default, as hints flag stylistic
issues that are often intentional. Use `--severity hint` to include them, or
`--severity error` to report errors only.

The exit status is 0 when nothing was reported, 1 when diagnostics were
reported and 2 on usage or I/O errors.

## Formatting from the command line

The `fmt` subcommand applies the same formatting as the language server. Files
are rewritten in place; directories are searched as for `check`. With no
arguments, or with `-`, it reads a Makefile from stdin and writes the formatted
version to stdout:

```sh
makefile-lsp fmt Makefile build/
makefile-lsp fmt --check .               # list unformatted files, for CI
makefile-lsp fmt < Makefile
```

Files with parse errors are left alone and reported. The exit status is 0 on
success, 1 when `--check` found unformatted files and 2 on usage, parse or I/O
errors.

## SCIP indexing

The `scip` subcommand produces a [SCIP](https://github.com/sourcegraph/scip)
index covering targets and variables, which tools like Sourcegraph can use for
code navigation. Each occurrence is tagged with a syntax kind so the index can
drive syntax highlighting, built-in variables and special targets carry their
documentation, and lint and parse diagnostics are included in the index too, so
they can be surfaced inline:

```sh
makefile-lsp scip                       # index ./Makefile into index.scip
makefile-lsp scip Makefile build/Rules.mk -o out.scip
```

Includes are followed as in `check`, so diagnostics take included and
including makefiles into account, and references to variables defined in
another makefile use the same symbol as the definition. Only the given files
are indexed.

Use `--project-root` to override the root directory recorded in the index
(defaults to the current directory).

SCIP support is gated behind the `scip` feature, which is enabled by default.
Build with `--no-default-features` to drop the `scip` dependency and the
subcommand.

## License

Apache-2.0
