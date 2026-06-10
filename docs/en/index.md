---
layout: home

hero:
  name: "Relon"
  text: "Logic as data"
  tagline: "Write the business rule once and store it like JSON. Relon is an executable data format whose payload is the logic itself — validation rules, pricing formulas, workflows, risk policies — evaluated by a sandboxed embeddable runtime (Rust). Same source + same input → byte-identical output, by design."
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
  - title: Deterministic by design
    details: "Same source + same input → byte-identical output. Dict iteration is `BTreeMap`-ordered, floats are IEEE-754 `f64`, the environment is opaque to the script. Replay, hash, cache evaluation freely — running the same `.relon` twice cannot diverge."
  - title: Sandboxed by default — no escape hatch
    details: "Scripts hold zero ambient privileges. `Capabilities` explicitly grant filesystem reads, step budgets, value-size watermarks, and capability-bit gates on native fns. There is no \"trusted mode\" the script can fall back to without the host's consent."
  - title: Self-describing type contracts
    details: "`#schema`, sum-type tagged enums, recursive schemas, branded values, computed defaults — type information travels with the payload. Receivers validate without out-of-band documentation."
  - title: Context-aware references
    details: "`&root`, `&sibling`, `&prev`, `&next` let logic reference its surrounding data declaratively — move a fragment to a different position in the tree and references re-resolve against its new neighbors automatically."
---

<RelonGallery />
