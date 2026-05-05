# What is Relon?

Relon is a **Rust-embeddable toolkit for building typed business-config DSLs**. It's not a general-purpose scripting language, and it's not trying to replace JSON. Its goal is to give you real type contracts, composition, and host extension points on top of JSON — and to emit plain JSON that downstream services can consume directly.

> **One-liner**: Build typed business-config DSLs on top of JSON.
>
> **From JSON-like, to JSON, for JSON.**

<figure style="margin: 2rem auto; max-width: 720px; text-align: center;">
  <img src="/relon/positioning.svg" alt="Relon two-tier authoring diagram" style="width: 100%; height: auto;" />
  <figcaption style="margin-top: 0.75rem; font-size: 0.9rem; color: #64748b; font-style: italic;">Two-tier authoring: platform team ships the vocabulary, business team composes it.</figcaption>
</figure>

## What Relon is

Treat Relon as a small toolkit purpose-built for business configuration:

- **JSON-like syntax**: it reads like JSON with expressions, decorators, and references. People who already know JSON pick it up in minutes.
- **Typed schemas**: `@schema` defines contracts, with sum-type tagged enums, recursive schemas, custom validation messages, and computed defaults.
- **Host extensions**: register native functions and decorator plugins from Rust; ship shared schemas / helpers in `.relon`; tie the two sides together with `@import`.
- **Sandboxed by default**: `Capabilities` control filesystem reads, evaluation budgets, value sizes, and native-fn allowlists.

## Who writes what — the two-tier model

Relon assumes two kinds of authors:

| Role | Deliverables | Concerns |
| --- | --- | --- |
| **Platform / framework team** | Rust extensions (native fns, decorator plugins) + `.relon` libraries marked with `@library` | Expose a stable business vocabulary; encode domain rules into schemas and decorators |
| **Business / product team** | `.relon` entry files (no `@library` marker) | `@import` platform libraries; write JSON-shaped configs; have errors caught early by types and validation |

When a platform-team file is marked `@library`, the runtime refuses to evaluate it as a host entry — it can only be consumed via `@import`. Business-team entry files stay double-purpose: directly evaluable AND importable. See [Library vs Entry](./library-vs-entry.md) for details.

## A complete tour in 30 lines

The example below uses `@library`, sum-type tagged enums, computed defaults, and host integration.

**`platform/notify.relon`** (platform-team library):

```relon
@library
{
    // Notification channel: sum-type tagged enum
    @schema Channel: Enum<
        Email { String address: *, String subject: * },
        SMS   { String phone: * },
        Push,
    >,

    // A general "notification with body" contract + computed default
    @schema Notification: {
        Channel via: *,
        String title: *,
        @default((self) => "[" + self.title + "]")
        String summary: *
    }
}
```

**`app/main.relon`** (business-team entry):

```relon
@import("../platform/notify.relon", spread=true)
{
    Notification welcome: {
        via: Channel.Email { address: "user@x.com", subject: "Hi" },
        title: "Welcome"
    }
}
```

Three lines of Rust on the host:

```rust
let json = relon::json_from_file("app/main.relon")?;
println!("{}", serde_json::to_string_pretty(&json)?);
```

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

> Note: the comprehensive guide is currently Chinese-first. The English landing pages are kept in sync; deeper guides may still be Chinese-only — switch to **简体中文** in the top right for the full guide.

- Syntax basics: [Syntax](../guide/introduction.md)
- Project on GitHub: <https://github.com/kookyleo/relon>
