---
layout: page
title: Playground
---

# Playground

> Run Relon right in your browser. Edits trigger an evaluate on the fly;
> the right pane shows the projected JSON output. Errors are listed in
> the bottom panel and marked inline in the editor.

<Playground />

## Try the examples

The **Example** dropdown at the top switches between four prebaked
sources:

- **demo** — the home-page example. Exercises function definitions,
  `&sibling` references, decorators and `f"..."` interpolation. Runs in
  the browser sandbox as-is.
- **pricing** — invoice pricing with tiered discounts and tax. Its
  signature is `#main(Order order)`, so it needs CLI `--args` to run.
  The browser sandbox cannot supply arguments; run it locally with
  `cargo run -p relon-cli -- run examples/pricing.relon --args '{...}'`.
- **feature_flag** — runtime feature-flag evaluator. Requires both
  `--args` and a host-registered `native_hash` function, neither of
  which the browser sandbox provides.
- **workflow** — state-machine-driven order workflow with the signature
  `#main(String state, String event)`. Same story — needs the CLI to
  run with arguments.

When you pick a non-demo example, a banner above the error panel
explains what's missing and points at the CLI command that works.

## Sandbox notes

- No `fs` / `net` / `clock` / `env` / `rng` capabilities are granted by
  default — see the repo's
  [`SECURITY.md`](https://github.com/kookyleo/relon/blob/main/SECURITY.md).
- Multiple files are supported via `#import`. Click `+` on the editor
  toolbar to add a file, `×` on a tab to remove one. The entry file is
  marked with `★`; click another tab's `★` to switch entries.
- The status bar shows the loaded wasm module version and readiness.
- Click any error span to jump to the offending file and position.
