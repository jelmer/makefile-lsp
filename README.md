# makefile-lsp

A Language Server Protocol (LSP) implementation for Makefiles, built in Rust.

## Features

All analysis is done on a lossless syntax tree of the current file; the
Makefile is never executed.

- **Diagnostics** - parse errors plus a set of lint checks (listed below)
- **Code actions** - quick fixes and refactorings (listed below)
- **Completion** - directives, targets and special targets at the start of
  a line, conditional directives after `else`,
  variable names, built-in functions and variables after `$(`, automatic
  variables after `$`, targets and file paths in prerequisite lists, and file
  paths after `include`
- **Hover** - the definition of user-defined variables, documentation for
  automatic variables, built-in variables, built-in functions and special
  targets such as `.PHONY`, the prerequisites and recipe of a target, where it
  is defined or referenced as a prerequisite, and the comment lines directly
  above the definition of a target or variable
- **Signature help** - parameter information inside built-in function calls
  like `$(subst from,to,text)`
- **Go to definition** - from a variable reference to its assignment, and from
  a prerequisite to the rule defining it
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
  recipes and comments
- **On-type formatting** - inserts a tab after pressing enter on a rule line
- **Command-line checking** - report diagnostics in CI, as text or SARIF
  (see below)
- **SCIP indexing** - generate a [SCIP](https://github.com/sourcegraph/scip)
  index for code navigation (see below)

### Diagnostics

Each diagnostic carries a code, so it can be identified in editors:

| Code | Severity | Description |
|------|----------|-------------|
| `undefined-variable` | warning | reference to a variable that is never assigned |
| `recursive-variable-reference` | warning | `=` assignment that references itself |
| `empty-variable-reference` | warning | `$()` or `${}` |
| `empty-automatic-variable` | warning | `$<`, `$^`, `$+` or `$?` in a rule without prerequisites, or `$*` outside a pattern rule |
| `unused-variable` | hint | variable that is assigned but never referenced |
| `mixed-assignment-operators` | warning | variable assigned with both `=` and `:=` |
| `shell-in-recursive-assignment` | warning | `$(shell ...)` in an `=` assignment, which runs on every expansion |
| `trailing-whitespace-in-value` | warning | trailing whitespace that becomes part of a variable value |
| `duplicate-target` | warning | target defined by more than one single-colon rule |
| `self-dependency` | warning | target that lists itself as a prerequisite |
| `circular-dependency` | warning | cycle between targets |
| `duplicate-prerequisite` | warning | prerequisite listed more than once in the same rule |
| `redundant-prerequisite` | hint | prerequisite already reached through another prerequisite |
| `undefined-phony-target` | warning | `.PHONY` entry without a matching rule |
| `unused-phony-target` | warning | phony target with no recipe that nothing depends on |
| `unreferenced-phony-target` | hint | phony target that nothing depends on, other than conventional ones like `all` or `install` |
| `empty-rule-probably-phony` | hint | rule without prerequisites or recipe that should probably be phony |
| `spaces-instead-of-tab` | error | recipe line indented with spaces |
| `orphan-recipe-line` | error | recipe line outside of any rule |
| `unterminated-conditional` | error | `ifdef`/`ifeq` without a matching `endif` |
| `include-missing-path` | error | `include` without a path |
| `missing-include-file` | warning | `include` of a file that does not exist |

### Code actions

- Add a target to `.PHONY`, or remove an undefined one from it
- Sort the prerequisites of `.PHONY`
- Define an undefined variable
- Replace spaces with a tab in a recipe line, or in all recipe lines
- Remove trailing whitespace from a variable value
- Use `:=` for an assignment containing `$(shell ...)`
- Add a missing `endif`
- Inline a variable
- Add a target that nothing depends on as a prerequisite of the default goal
- Remove a prerequisite that is already reached through another one

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

Diagnostics are printed as `path:line:column: severity: message [code]`. Use
`--format sarif` to produce SARIF 2.1.0, e.g. for GitHub code scanning.

Only errors and warnings are reported by default, as hints flag stylistic
issues that are often intentional. Use `--severity hint` to include them, or
`--severity error` to report errors only.

The exit status is 0 when nothing was reported, 1 when diagnostics were
reported and 2 on usage or I/O errors.

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

Use `--project-root` to override the root directory recorded in the index
(defaults to the current directory).

SCIP support is gated behind the `scip` feature, which is enabled by default.
Build with `--no-default-features` to drop the `scip` dependency and the
subcommand.

## License

Apache-2.0
