# lsp-gleam

Gleam language-server bridge for Basalt (`gleam lsp`).

## Capabilities

- `DIAGNOSTICS` / `HOVER`.
- Spawns `gleam lsp` through the host LSP process imports
  (`basalt_spawn_lsp`, `basalt_lsp_write`, `basalt_lsp_read`,
  `basalt_lsp_stop`, `basalt_sleep_ms`).

## Behaviour

- The project root is found by walking up from the file to the nearest
  `gleam.toml`.
- `diagnose` sends `textDocument/didOpen` (or `didChange` for later edits),
  waits for `textDocument/publishDiagnostics`, and converts LSP
  line/UTF-16 positions into byte ranges for Basalt.
- `hover` sends `textDocument/hover` and flattens `MarkupContent`,
  `MarkedString`, or arrays of either into Markdown text.
- ANSI-free compiler diagnostics include the LSP code (for example
  `[unknown_variable] The name ...`), and severities map to Basalt's
  `Error` / `Warning` / `Info` / `Hint`.

The LSP process is started lazily per plugin instance and reused for
subsequent files.

## Build

```bash
cargo build --release --target wasm32-unknown-unknown
```

Artifact: `target/wasm32-unknown-unknown/release/lsp_gleam.wasm`.
