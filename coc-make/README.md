# coc-make

A coc.nvim extension that wires up [makefile-lsp](https://github.com/jelmer/makefile-lsp)
for Makefile editing.

## Installation

```vim
:CocInstall coc-make
```

The extension expects `makefile-lsp` on `$PATH`. Install it with:

```sh
cargo install makefile-lsp
```

## Configuration

Settings in `coc-settings.json`:

- `make.enable` (boolean, default `true`): enable the extension
- `make.serverPath` (string, default `"makefile-lsp"`): path to the LSP binary
- `make.codeLens.runTarget` (boolean, default `true`): show a "Run" code lens
  above each target, which runs it in a terminal; coc.nvim only shows code
  lenses when `codeLens.enable` is set
- `make.makeProgram` (string, default `"make"`): the make program used to run
  targets, such as `gmake`, `bmake` or `nmake`
- `make.includeDirs` (array of strings, default `[]`): directories to search
  for included makefiles, as with `make -I`

## Development

```sh
npm install
npm run build
```

To install the local checkout in coc.nvim:

```vim
:CocInstall file:///absolute/path/to/makefile-lsp/coc-make
```
