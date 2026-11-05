# Makefile LSP for VS Code

Language support for Makefiles, backed by
[makefile-lsp](https://github.com/jelmer/makefile-lsp). The extension ships
with a prebuilt server for Linux (x64, arm64), macOS (x64, arm64) and Windows
(x64), so nothing else needs to be installed.

The server analyses the makefile and the makefiles it includes without ever
running make. GNU make, BSD make and nmake syntax are understood, and checks
adapt to the variant in use.

## Features

- **Diagnostics** - parse errors and lint checks such as undefined or unused
  variables, recipes indented with spaces, missing `.PHONY` declarations,
  circular dependencies, prerequisites that can't be resolved, missing
  include files and shell syntax errors in recipes. See the
  [list of checks](https://github.com/jelmer/makefile-lsp#diagnostics).
- **Quick fixes** - add a target to `.PHONY`, define an undefined variable,
  replace spaces with tabs, add a missing `endif`, inline a variable and
  [more](https://github.com/jelmer/makefile-lsp#code-actions)
- **Completion** - directives, targets, variables, built-in functions,
  automatic variables, prerequisites and include paths, plus snippets for
  rules, conditionals and `define` blocks
- **Hover** - documentation for directives, built-in functions and
  variables, automatic variables and special targets, and the definition,
  recipe and comment of user-defined targets and variables
- **Signature help** inside built-in function calls like `$(subst ...)`
- **Go to definition**, **find references** and **document highlights** for
  targets and variables, and go to definition on include paths
- **Rename** targets and variables across included makefiles
- **Linked editing** of a variable name and its other occurrences
- **Call hierarchy** of targets and their prerequisites
- **Run target** code lens above each target, which runs it as a VS Code
  task
- **Clickable include paths**
- **Inlay hints** with the value of `:=` variables and the dependency depth
  of top-level targets
- **Outline and workspace symbols** for targets and variables
- **Folding** of rules, conditionals and comment blocks, and **smart
  selection**
- **Semantic highlighting** of targets, variables, prerequisites, recipes,
  directives and comments
- **Formatting** of the document or a selection (tabs for recipe lines,
  trailing whitespace, final newline), and a tab inserted after pressing
  enter on a rule line

Targets and variables from included makefiles are taken into account, and a
fragment like `rules.mk` opened on its own picks up the makefile that
includes it. See
[included makefiles](https://github.com/jelmer/makefile-lsp#included-makefiles)
for how include paths are resolved.

## Settings

| Setting | Default | Description |
|---------|---------|-------------|
| `makefile.enable` | `true` | Enable the language server |
| `makefile.serverPath` | `""` | Path to a `makefile-lsp` binary; empty uses the bundled server, falling back to `makefile-lsp` on `PATH` |
| `makefile.codeLens.runTarget` | `true` | Show the "Run" code lens above each target |
| `makefile.makeProgram` | `"make"` | Program used to run targets, such as `gmake`, `bmake` or `nmake` |
| `makefile.includeDirs` | `[]` | Directories to search for included makefiles, as with `make -I`; relative paths are taken relative to the top-level makefile |
| `makefile.trace.server` | `"off"` | Trace the communication with the server |

Changes to `makefile.codeLens.runTarget` and `makefile.includeDirs` take
effect when the server restarts.

The extension also enables `editor.formatOnType` for makefiles by default.

## Running targets

Clicking "Run" above a target runs `make -f <makefile> <target>` (or
`nmake /F ...`) as a task in the makefile's directory. For a fragment
included from another makefile, the including makefile is used.

## Command line

The same server binary can check and format makefiles from the command line,
e.g. in CI; see the
[makefile-lsp README](https://github.com/jelmer/makefile-lsp#checking-from-the-command-line).

## License

Apache-2.0
