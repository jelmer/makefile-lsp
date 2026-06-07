# makefile-lsp

A Language Server Protocol (LSP) implementation for Makefiles, built in Rust.

## Features

- **Diagnostics** - reports parse errors as you type
- **Completions** - suggests targets, variables, and built-in functions
- **Document symbols** - outline of targets and variable assignments
- **Folding ranges** - collapse rules and multi-line definitions
- **Semantic tokens** - syntax highlighting for targets, variables, prerequisites, recipes, and comments
- **SCIP indexing** - generate a [SCIP](https://github.com/sourcegraph/scip) index for code navigation

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

Use a generic LSP client extension and configure it to run `makefile-lsp` for
`Makefile` files.

## SCIP indexing

The `scip` subcommand produces a [SCIP](https://github.com/sourcegraph/scip)
index covering targets and variables, which tools like Sourcegraph can use for
code navigation:

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
