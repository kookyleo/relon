---
layout: home

hero:
  name: "Relon"
  text: "Logic as portable data"
  tagline: "Write the business rule once. Store it like JSON, ship it like JSON, evaluate it identically on any conformant runtime — Go, TypeScript, Swift, browser-WASM, Rust. The reference runtime in this repo is Rust; the spec is runtime-agnostic and deterministic by construction."
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
  - title: Deterministic across runtimes
    details: "Same source + same input → byte-identical output. Dict iteration is `BTreeMap`-ordered, floats are IEEE-754 `f64`, environment is opaque to the script. Logic stored in your database evaluates the same in every runtime that consumes it."
  - title: Sandboxed by default — no escape hatch
    details: "Scripts hold zero ambient privileges. `Capabilities` explicitly grant filesystem reads, step budgets, value-size watermarks, and per-function allowlists. There is no \"trusted mode\" the script can fall back to without the host's consent."
  - title: Self-describing type contracts
    details: "`#schema`, sum-type tagged enums, recursive schemas, branded values, computed defaults — type information travels with the payload. Receivers validate without out-of-band documentation."
  - title: Context-aware references
    details: "`&root`, `&sibling`, `&prev`, `&next` let logic reference its surrounding data declaratively, no hard-coded paths. Declarative references stay deterministic across runtimes."
---

<RelonGallery />
