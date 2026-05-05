---
layout: home

hero:
  name: "Relon"
  text: "Build typed business-config DSLs on top of JSON"
  tagline: "A Rust-embeddable toolkit for typed business-config DSLs. Platform teams define schemas, decorators, and native functions; business teams compose them in JSON-shaped configs that compile to plain JSON. From JSON-like, to JSON, for JSON."
  image:
    src: /logo.svg
    alt: Relon
  actions:
    - theme: brand
      text: Get Started
      link: /en/guide/introduction
    - theme: alt
      text: View on GitHub
      link: https://github.com/kookyleo/relon

features:
  - title: Two-tier authoring
    details: "Platform teams ship schemas, decorators, native functions, and `.relon` libraries. Business teams write thin entry configs that compose them. The `@library` marker enforces the split at the language level — library files refuse to be evaluated as entries."
  - title: Typed business schemas
    details: "Sum-type tagged enums, recursive schemas, `@expect` for custom validation messages, and required / optional / literal-default / computed-default fields all coexist. Domain contracts no longer have to decay into loose dicts."
  - title: Sandboxed by default
    details: "`Capabilities` gate filesystem reads, evaluation step budgets, value-size watermarks, and native-function allowlists. `Context::sandboxed()` rejects everything until the host opts in — safe defaults for untrusted scripts."
  - title: A JSON closed loop
    details: "From JSON-like, to JSON, for JSON. Input syntax sits next to JSON; output is always plain JSON. The `Projector` trait lets you tune the output shape (e.g. sum-type encoding) but the destination is always JSON."
---

<figure style="margin: 3rem auto; max-width: 760px; text-align: center;">
  <img src="/relon/positioning.svg" alt="Relon two-tier authoring: Platform Team writes schemas/fns/decorators, Business Team imports them and authors entry configs that the relon-evaluator turns into plain JSON for downstream services." style="width: 100%; height: auto;" />
  <figcaption style="margin-top: 0.75rem; font-size: 0.9rem; color: #64748b; font-style: italic;">Two-tier authoring: platform team ships the vocabulary, business team composes it, the evaluator emits JSON.</figcaption>
</figure>
