# Chezzi editor tooling

Two layers, both single-sourced from the compiler so language changes flow through automatically:

| Layer | What it does | Source of truth |
| --- | --- | --- |
| `chezzi-lsp` (LSP server) | Diagnostics + semantic-token highlighting in any LSP editor (neovim, VSCode, …) | The compiler front-end (lexer / parser / checker). A new keyword in the lexer highlights with **no** extra config. |
| `vscode/syntaxes/chezzi.tmLanguage.json` (TextMate grammar) | Static syntax highlighting in VSCode (works with no server) | Generated from the lexer's `KEYWORDS` / `PUNCTUATION` tables — **never hand-edit it.** |

The LSP is the primary, auto-extending path; the TextMate grammar is a static fallback for VSCode.

---

## Neovim (primary target)

`chezzi-lsp` is a stdio language server providing **diagnostics** (type/parse errors as you type) and
**semantic tokens** (highlighting straight from the lexer).

### 1. Build the server

```sh
cargo build --features lsp --bin chezzi-lsp        # → target/debug/chezzi-lsp
# or release:
cargo build --release --features lsp --bin chezzi-lsp
```

The `lsp` feature keeps the async deps (tower-lsp + tokio) out of the default `cargo build`, so the
server is built on demand only.

### 2. Register the filetype + server (lspconfig, Neovim 0.10+)

```lua
-- ~/.config/nvim/after/ftdetect or your init.lua
vim.filetype.add({ extension = { chz = "chezzi" } })

local lspconfig = require("lspconfig")
local configs = require("lspconfig.configs")

-- chezzi-lsp is not in nvim-lspconfig's registry, so define it once:
if not configs.chezzi then
  configs.chezzi = {
    default_config = {
      cmd = { "/absolute/path/to/target/debug/chezzi-lsp" }, -- adjust to your checkout
      filetypes = { "chezzi" },
      -- root = the project dir containing chezzi.toml (so imports resolve), else the file's dir.
      root_dir = lspconfig.util.root_pattern("chezzi.toml") or vim.fn.getcwd,
      single_file_support = true,
    },
  }
end

lspconfig.chezzi.setup({
  on_attach = function(client, bufnr)
    -- Enable LSP semantic-token highlighting (Neovim applies it automatically when the server
    -- advertises the capability, which chezzi-lsp does). Nothing extra is required, but you can
    -- force-refresh with:
    vim.lsp.semantic_tokens.start(bufnr, client.id)
  end,
})
```

Semantic tokens are on by default in Neovim 0.9+ whenever the server advertises
`semanticTokensProvider` — which `chezzi-lsp` does (legend:
`keyword, operator, string, number, comment, variable`). If your colorscheme doesn't map them, link
the `@lsp.type.*` groups, e.g.:

```lua
vim.api.nvim_set_hl(0, "@lsp.type.keyword.chezzi",  { link = "Keyword" })
vim.api.nvim_set_hl(0, "@lsp.type.string.chezzi",   { link = "String" })
vim.api.nvim_set_hl(0, "@lsp.type.number.chezzi",   { link = "Number" })
vim.api.nvim_set_hl(0, "@lsp.type.comment.chezzi",  { link = "Comment" })
vim.api.nvim_set_hl(0, "@lsp.type.operator.chezzi", { link = "Operator" })
vim.api.nvim_set_hl(0, "@lsp.type.variable.chezzi", { link = "Identifier" })
```

Diagnostics appear automatically via `textDocument/publishDiagnostics` on open/change/save.

> Note: the server type-checks the **live buffer** for the file you're editing; imported modules are
> read from disk, so an unsaved edit to an imported module isn't reflected until you save it.

---

## VSCode (secondary target)

### Option A — TextMate grammar only (no server)

The `editors/vscode` directory is a minimal extension that contributes the `.chz` language and the
generated TextMate grammar.

```sh
# from the repo root, open the extension in an Extension Development Host:
code editors/vscode
# then press F5 ("Run Extension") — a second VSCode window opens with .chz highlighting active.
```

To install it permanently, package with [`vsce`](https://github.com/microsoft/vscode-vsce)
(`npx vsce package`) and `code --install-extension chezzi-0.1.0.vsix`.

### Option B — add the LSP

Pair the extension with `chezzi-lsp` (build as above) using a generic LSP client extension, or extend
this package with a small `vscode-languageclient` activation that launches `chezzi-lsp` over stdio.
The grammar and the server compose: TextMate paints instantly, the server adds diagnostics + semantic
refinement.

---

## Regenerating the TextMate grammar

`chezzi.tmLanguage.json` is generated from the lexer; the generator and a CI drift-guard live in
`tests/editor_tmlanguage.rs`. After adding a keyword/operator to the lexer:

```sh
UPDATE_EDITOR_ASSETS=1 cargo test --test editor_tmlanguage
```

A plain `cargo test` **fails** if the committed grammar is stale, so regeneration is a script — never a
hand-edit.
