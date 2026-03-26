# coc-make

A coc.nvim extension that provides Language Server Protocol support for Makefiles.

## Features

- Diagnostics for parse errors
- Completions for targets, variables, and built-in functions
- Document symbols (outline of targets and variables)
- Folding ranges
- Semantic highlighting for targets, variables, prerequisites, recipes, and comments

## Installation

### Prerequisites

- [coc.nvim](https://github.com/neoclide/coc.nvim) installed in Vim/Neovim
- Node.js and npm
- The makefile-lsp server built and available

### Local Installation

1. **Build the makefile-lsp server first:**
   ```bash
   cd /path/to/makefile-lsp
   cargo build --release
   ```

2. **Install and build the coc extension:**
   ```bash
   cd coc-make
   npm install
   npm run build
   ```

3. **Install the extension in coc.nvim:**
   ```vim
   :CocInstall file:///absolute/path/to/makefile-lsp/coc-make
   ```

   Or alternatively, create a symlink in your coc extensions directory:
   ```bash
   ln -s /absolute/path/to/makefile-lsp/coc-make ~/.config/coc/extensions/node_modules/coc-make
   ```

4. **Configure the LSP server path:**

   Add the following to your coc-settings.json (`:CocConfig` in Vim):
   ```json
   {
     "make.enable": true,
     "make.serverPath": "/absolute/path/to/makefile-lsp/target/release/makefile-lsp"
   }
   ```

### Verify Installation

1. Open a Makefile
2. Try typing targets or variables and you should see completions
3. Check `:CocList extensions` to see if `coc-make` is listed and active

## Configuration

Available settings in coc-settings.json:

- `make.enable` (boolean, default: true) - Enable/disable the extension
- `make.serverPath` (string, default: "makefile-lsp") - Path to the makefile-lsp executable

## Development

To work on the extension:

```bash
# Watch for changes and rebuild
npm run watch

# After making changes, restart coc
:CocRestart
```

## Troubleshooting

- **LSP not starting:** Check that the `make.serverPath` points to the correct executable
- **No completions:** Verify the file is recognized as a Makefile
- **Extension not loading:** Check `:CocList extensions` and look for any error messages
