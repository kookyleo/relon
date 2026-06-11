# Playground & wasm bindings

The [Playground](/en/playground) in the top nav is a Relon
environment that runs entirely in the browser: parsing, analysis,
evaluation and formatting are all performed by a WebAssembly (wasm)
module — **no server involved**.

It consists of two parts:

- **Bindings layer** `crates/relon-wasm-bindings`: compiles the Relon
  engine to wasm via wasm-bindgen and exposes evaluation, formatting
  and a set of editor-intelligence entry points to JavaScript. Any
  page that wants an in-browser Relon runtime can reuse this layer
  directly.
- **Frontend** `docs/.vitepress/theme/components/`: a
  CodeMirror-based multi-file editor page that consumes those entry
  points for live evaluation, diagnostic markers, completion, hover
  tooltips, and more.

This page documents the bindings layer's full JS surface and the
local build flow, with the code as the single source of truth.

## Common calling conventions

All entry points share these conventions:

- **`sources`**: the in-memory module map; both shapes are accepted —
  an object `{ "main.relon": "…", "lib.relon": "…" }`, or an array
  `[{ path: "main.relon", content: "…" }, …]` (the array form
  preserves ordering, friendlier for tab-based frontends).
- **`entry`**: the entry file name; must be one of the keys in
  `sources`.
- **`line` / `character`**: cursor position, 0-based; `character` is
  measured in UTF-16 code units (the CodeMirror / LSP position
  convention).
- **Return values**: Rust structs serialize as plain JS objects /
  arrays (not `Map`), so property access and `JSON.stringify` just
  work.
- **Error surface**: failures throw a structured `ErrorReport`
  object rather than an opaque string, so a frontend can render
  inline markers:

```ts
interface ErrorReport {
  kind: 'InvalidInput'    // malformed input (missing entry, bad sources shape, …)
      | 'ParseError'      // entry file failed to parse
      | 'AnalyzeError'    // analyzer errors in the workspace (entry or any import)
      | 'EvalError'       // runtime error at evaluation time
      | 'ProjectionError';// evaluation succeeded but the result has no JSON
                          // projection (closure, non-finite float, …)
  message: string;        // human-readable summary
  spans: {                // source ranges the error anchors to (byte offsets)
    file: string | null;  // owning module; null for workspace-level reports
    start: number;
    end: number;
    label: string | null;
  }[];
  help: string | null;    // miette-style help text when available
  code: string | null;    // diagnostic code, e.g. relon::analyze::unresolved_reference
}
```

## Evaluation & formatting

### `evaluate(sources, entry, args)`

Evaluates the entry file and returns the projected JSON result (a
plain JS object / array / scalar). One entry point covers both script
kinds — a program declaring `#main(...)` decodes `args` against its
signature and runs; a script without one evaluates its root
expression directly.

`args` must be a **JSON string** (e.g. `JSON.stringify({...})`),
`null`, `undefined`, or omitted. Why not a JS object: JS has only one
Number type, so `100.0` collapses to `100` once it passes through a
JS object, while the `#main(...)` signature relies on the Int vs
Float distinction. Parsing the string on the Rust side with
`serde_json` preserves it losslessly (`100` → Int, `100.0` → Float).

Decoding of `args` is driven by the `#main(...)` parameter types:
`Option<T>` (`null`, `"None"`, `{"Some": …}` shapes),
`Result<T, E>` (externally tagged `{"Ok": …}` / `{"Err": …}`),
`#enum` (unit variants accept the bare string name, payload variants
take `{"VariantName": payload}`), `#schema`, and `Tuple` / `List` /
`Dict` at arbitrary nesting depth.

```js
import init, { evaluate } from './wasm/relon/relon_wasm.js';
await init({ module_or_path: './wasm/relon/relon_wasm_bg.wasm' });

// No-args script: root-expression evaluation
evaluate({ 'main.relon': '{ price: 100 + 23 }' }, 'main.relon');
// → { price: 123 }

// #main entry + JSON-string args
evaluate(
  { 'main.relon': '#main(Int n) -> Int\nn * 2' },
  'main.relon',
  JSON.stringify({ n: 21 })
);
// → 42

// Multi-file #import (cross-module member types aren't statically
// derivable, hence #relaxed)
evaluate(
  [
    { path: 'main.relon', content: '#relaxed\n#import lib from "./lib.relon"\n{ g: lib.hello }' },
    { path: 'lib.relon', content: '{ hello: "hi" }' },
  ],
  'main.relon'
);
// → { g: "hi" }
```

On failure it throws an `ErrorReport`:

```js
try {
  evaluate({ 'main.relon': '{ not closed' }, 'main.relon');
} catch (err) {
  err.kind;    // "ParseError"
  err.message; // human-readable summary
  err.spans;   // [{ file, start, end, label }]
}
```

### `format(content)`

Pretty-prints a Relon source string through `relon-fmt`. Returns the
formatted source on success; throws an `ErrorReport`
(`kind: "ParseError"`) when the input doesn't parse.

### `version()`

Returns the bindings crate's version string (from
`CARGO_PKG_VERSION`, tracking the workspace version) — useful for UI
footers and cache busting.

## Editor-intelligence entry points

These share the same analyzer implementation as `relon-lsp`, but are
driven entirely by the in-memory `sources` map, so the browser gets
identical semantics without a filesystem.

| Entry point | Returns | Description |
| --- | --- | --- |
| `complete(sources, entry, line, character)` | `CompletionResult[]` | Completion candidates. Falls back to a recovering parse when the entry doesn't parse cleanly, so completion keeps working mid-edit; never returns `null` |
| `hover(sources, entry, line, character)` | `HoverResult \| null` | Hover tooltip; `markdown` is the tooltip body, with the byte range of the source it describes |
| `goto_definition(sources, entry, line, character)` | `GotoDefinitionResult \| null` | Go to definition, across modules in `sources`; on an `#import` path it jumps to the top of the target file |
| `find_references(sources, entry, line, character, include_declaration)` | `ReferenceLocation[] \| null` | All in-file occurrences of the symbol; `include_declaration` controls whether the declaration is included |
| `signature_help(sources, entry, line, character)` | `SignatureHelpResult \| null` | Call-argument help: rendered callee signature + index of the parameter the cursor sits in |
| `document_symbols(sources, entry)` | `DocumentSymbolWire[]` | File outline. Each item carries a `parent` index into the same array, so the caller can rebuild the tree |
| `inlay_hints(sources, entry)` | `InlayHintWire[]` | Inline hints (currently parameter-name ghost text); returns an empty array when the entry doesn't parse |
| `code_actions(sources, entry, line, character)` | `CodeActionWire[]` | Quick fixes for diagnostics anchored at the cursor; empty array when none apply |
| `prepare_rename(sources, entry, line, character)` | `PrepareRenameResult` | Probes whether the cursor is on a renamable symbol; `valid: false` carries the reason in `error` |
| `rename_symbol(sources, entry, line, character, new_name)` | `TextEditWire[]` | Computes every text replacement for the rename; throws an `ErrorReport` on failure |

Field conventions for the returned shapes:

- `CompletionResult`: `{ label, kind, detail, apply_snippet }`.
  `kind` is one of `method` / `field` / `param` / `schema` /
  `stdlib` / `module` / `import` / `reference` / `directive` /
  `pragma` / `decorator` / `keyword`; `apply_snippet` is an LSP-style
  `${N:placeholder}` template (callables expand like
  `@currency(${1:symbol})`), `null` means insert the bare `label`.
- `TextEditWire` (shared by `rename_symbol` and
  `code_actions.edits`): `{ start_line, start_character, end_line,
  end_character, start_offset, end_offset, new_text }` — both
  LSP-style line/character and byte offsets, pick whichever fits.
- `GotoDefinitionResult`: `{ path, start: { line, character },
  end: { line, character } }`, where `path` is the target file in
  `sources`.
- `HoverResult` / `SignatureHelpResult`: positions are byte offsets
  (`range_start_offset` / `range_end_offset`).

## Building locally

Prerequisites: a Rust toolchain with the `wasm32-unknown-unknown`
target, [wasm-pack](https://rustwasm.github.io/wasm-pack/), and
Node.js.

```bash
rustup target add wasm32-unknown-unknown

cd docs
npm install
npm run build:wasm   # wasm-pack builds crates/relon-wasm-bindings;
                     # output lands in docs/public/wasm/relon/ (gitignored)
npm run test:wasm    # Node smoke test (test-node.mjs)
npm run docs:dev     # local VitePress; the Playground nav entry is live
```

Two things to keep in mind:

- The wasm32 target enables `+simd128` in `.cargo/config.toml`,
  paired with `--enable-simd` in the crate's wasm-opt flags
  (`Cargo.toml`). **Both must stay in sync** — changing one without
  the other yields either scalar (slow) wasm or a wasm-opt validator
  rejection.
- The crate is `publish = false`: it is currently built only as a
  site asset, not published to crates.io / npm.

The live site is rebuilt by `.github/workflows/deploy-docs.yml` on
every push to main: it builds the wasm bundle and deploys it together
with VitePress to GitHub Pages.

## Sandbox boundary

The browser uses the exact same posture as host embedding: the
bindings construct the evaluation context with
`Context::sandboxed()`, capabilities are **all denied by default**,
and no `--trust`-style toggle is ever surfaced to untrusted browser
users. This is a capability posture; the general boundary model is in
[Threat model](./threat-model).

- No filesystem module resolver: `#import` can only hit files in the
  `sources` map or `std/*` virtual modules; referencing a path not in
  the map fails at analysis time with `AnalyzeError` — it never falls
  through to disk.
- Any call that touches a real capability (filesystem, host-native
  functions, …) fails cleanly with `CapabilityDenied`, surfaced as an
  `EvalError`.

See [Threat model](./threat-model) for the boundary statement and
[Sandbox & capabilities](./sandbox.md) for the full capability model.
