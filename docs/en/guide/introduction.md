# What is Relon?

Relon is an **executable data format**: its payload is business logic.
Write a validation rule, a pricing formula, a workflow, a risk policy
once as a Relon document, store it like JSON, and let the embedded
Rust runtime evaluate it **deterministically** — the same source plus
the same input always produces a byte-identical result.

> **One-liner**: Logic as data — write the rule once, store it like
> JSON, evaluate it deterministically with explicit host authority.

## What this commits us to (the hard constraints)

* **Same source + same input → same output.** Dict iteration is
  `BTreeMap`-ordered; floats are IEEE-754 `f64`; the script reads no
  environment variables, system clock, or RNG.
* **No implicit trust.** Scripts have zero ambient
  privileges. Filesystem, network, native functions are all gated by
  `Capabilities` and granted explicitly by the host. Host-owned code
  may opt into full trust, but that grant must be explicit and
  auditable; scripts cannot bypass host consent on their own. For the
  security boundary, see [Threat model](./threat-model).
* **The std library is part of the language.** `std/list`,
  `std/string`, `std/math`, … ship with the runtime — scripts can
  `#import` them without the embedder wiring anything up. Authors
  depend only on the stable names the language provides.

<figure style="margin: 2rem auto; max-width: 720px; text-align: center;">
  <img src="/positioning.svg" alt="Relon two-tier authoring diagram" style="width: 100%; height: auto;" />
  <figcaption style="margin-top: 0.75rem; font-size: 0.9rem; color: #64748b; font-style: italic;">Two-tier authoring: platform team ships the vocabulary, business team composes it.</figcaption>
</figure>

## What Relon is

Treat Relon as a small toolkit purpose-built for business configuration:

- **JSON-like syntax**: it reads like JSON with expressions, directives, decorators, and references. People who already know JSON pick it up in minutes.
- **Typed schemas**: `#schema` defines contracts, with sum-type tagged enums, recursive schemas, custom validation messages, and computed defaults.
- **Host extensions**: register native functions and decorator plugins from Rust; ship shared schemas / helpers in `.relon`; tie the two sides together with `#import`.
- **Capability posture by default**: `Capabilities` control filesystem reads, evaluation budgets, and value sizes; native fns are admitted by the capability bits they declare on `NativeFnGate` — there is no by-name allowlist.

## Who writes what — the two-tier model

Relon assumes two kinds of authors:

| Role | Deliverables | Concerns |
| --- | --- | --- |
| **Platform / framework team** | Rust extensions (native fns, decorator plugins) + `.relon` libraries (no `#main`) | Expose a stable business vocabulary; encode domain rules into schemas and decorators |
| **Business / product team** | `.relon` entry files declaring `#main(...)` | `#import` platform libraries; write JSON-shaped configs; have errors caught early by types and validation |

Whether a file declares `#main(...)` decides how it's used: a `#main` file is an **entry program** (the host must push args via `run_main`); a file without `#main` can be evaluated directly as data, and it can also be `#import`ed by other files. That's the typical shape of a platform library.

## A complete tour in 30 lines

The example below uses `#schema`, sum-type tagged enums, computed defaults, and host integration.

**`platform/notify.relon`** (platform-team library, no `#main`):

```relon
#enum Channel {
    Email { address: String, subject: String },
    SMS { phone: String },
    Push
}

{
    // A general "notification with body" contract + computed default
    #schema Notification {
        Channel via: *,
        String title: *,
        #default (self) => "[" + self.title + "]"
        String summary: *
    }
}
```

**`app/main.relon`** (business-team entry):

```relon
#import * from "../platform/notify.relon"
{
    Notification welcome: {
        via: Channel.Email { address: "user@x.com", subject: "Hi" },
        title: "Welcome"
    }
}
```

Three lines of Rust on the host:

```rust
// The entry uses #import and must read a local file — it's the host's
// own script, so use the *_trusted entry point.
let json = relon::json_from_file_trusted("app/main.relon")?;
println!("{}", serde_json::to_string_pretty(&json)?);
```

> The default entry `json_from_file` uses the sandboxed posture:
> scripts hold zero ambient privileges, so the `#import` here would be
> denied filesystem access. That's why this example uses
> `json_from_file_trusted` (host-owned scripts only); when running
> untrusted scripts, keep explicit capabilities narrow and use the
> appropriate VM/process boundary — see [Threat model](./threat-model)
> and [Sandbox & capabilities](./sandbox).

Output (note that the `Email` layer is the externally-tagged sum-type form):

```json
{
  "welcome": {
    "via": { "Email": { "address": "user@x.com", "subject": "Hi" } },
    "title": "Welcome",
    "summary": "[Welcome]"
  }
}
```

## What Relon is NOT

To prevent misreadings, here's what's deliberately out of scope:

- ❌ **Multi-format output**: no YAML/TOML/XML — [Pkl](https://pkl-lang.org/) handles that.
- ❌ **General-purpose scripting**: no IO, no statement-style loops, no side effects — don't reach for Relon as a Lua/Starlark replacement.
- ❌ **Constraint-only validation**: Relon both describes and evaluates; if you only need constraints, [CUE](https://cuelang.org/) fits better.
- ❌ **Total / pure-functional purism**: evaluation can fail and closures aren't required to be total — Relon isn't [Dhall](https://dhall-lang.org/).
- ❌ **Cross-language native type / decorator registration**: the v1 cross-language roadmap is a C ABI "JSON in / JSON out" entry plus native-fn callbacks via JSON-wire — not schema registration from Python/Node.
- ❌ **Multi-environment branching primitives**: no `dev/staging/prod` keywords — use plain `match` / `if`.

## Where to go next

- Syntax basics: [Syntax basics](./syntax)
- Writing contracts: [Types & schema contracts](./types)
- Modules and entry programs: [Modules & scope](./modules)
- Embedding into a Rust host: [Host integration](./host-integration)
- Running untrusted scripts: [Sandbox & capabilities](./sandbox)
- Standard library tour: [Standard library](./stdlib)
- Project on GitHub: <https://github.com/kookyleo/relon>
