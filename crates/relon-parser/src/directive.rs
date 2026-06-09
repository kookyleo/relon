//! Directive name constants and shape-by-name lookup.
//!
//! `#name` directives are the structural / declarative attributes of
//! the language (imports, schemas, defaults, error messages, brand,
//! internal-binding markers, the `#main(...)` entry signature). Host-registered only;
//! no user-definable `#`. Each name dispatches to one of five fixed
//! [`DirectiveShape`] forms — the CST parser uses
//! [`directive_shape`] to choose its production path, and the typed
//! lowering uses the same lookup to interpret the directive's
//! parsed body.
//!
//! This module used to host a full winnow-based combinator parser as
//! well; P6 retired the runtime parser in favour of the rowan CST
//! walker in `lower.rs`. Only the lookup table + name constants
//! survive — they're re-exported by `relon-analyzer` and
//! `relon-evaluator` so every layer dispatches off the same string
//! literals.

use crate::DirectiveShape;

/// Canonical directive names. Centralizing the strings here lets
/// downstream crates (`relon-analyzer`, `relon-evaluator`) refer to the
/// same identifiers without maintaining their own private mirrors.
pub const INTERNAL: &str = "internal";
pub const DEFAULT: &str = "default";
pub const EXPECT: &str = "expect";
pub const MSG: &str = "msg";
pub const ERROR: &str = "error";
pub const BRAND: &str = "brand";
pub const SCHEMA: &str = "schema";
pub const ENUM: &str = "enum";
pub const IMPORT: &str = "import";
pub const MAIN: &str = "main";
/// `#relaxed` — bare file-level directive that opts the module out
/// of strict inference. Strict is the analyzer's default: every
/// value must have a statically inferable type, and sites the
/// analyzer can't classify (uninferrable spread sources, dynamic
/// keys without a hint, untyped closure parameters, native fns with
/// no signature, …) surface as errors. A `#relaxed` directive at
/// the file level lets those positions stay silent (the runtime
/// still type-checks them on the way out). The opt-out propagates
/// across `#import` from the *entry* module: a relaxed entry
/// analyses every reachable import in relaxed mode too, so a strict
/// library doesn't tighten a relaxed entry by accident.
pub const RELAXED: &str = "relaxed";
/// `#unstrict` — exact synonym for [`RELAXED`]. Both names are
/// accepted so authors can pick whichever reads more naturally next
/// to other directives.
pub const UNSTRICT: &str = "unstrict";
/// Phase A of the trait-bound / schema-method system: a method-level
/// pragma `#derive <Constraint>` declares the following method is the
/// witness for the named built-in constraint (e.g. `Equatable`,
/// `Comparable`). Body shape is a single bare identifier (the
/// constraint name). Registered globally so the parser accepts it; the
/// analyzer enforces that it only appears immediately above a method
/// inside a `with { ... }` block.
pub const DERIVE: &str = "derive";
/// Schema-level (or, in rare cases, method-level) pragma
/// `#no_auto_derive <Constraint>` opts the schema out of structural
/// auto-derivation for the named constraint (e.g. opt out of the
/// default `JsonProjectable` derivation for an internal-only schema).
pub const NO_AUTO_DERIVE: &str = "no_auto_derive";
/// Method-level pragma `#native` declares the method's body lives in
/// host Rust (registered through the schema-method host API). The
/// parser leaves the method's body empty when this pragma is present;
/// the analyzer cross-checks against the host registry.
pub const NATIVE: &str = "native";
/// Schema-rooted Phase A.1: `#extend X with { ... }` adds methods to
/// an already-declared schema X (built-in or user). Same parser shape
/// as `#schema` (NameBody), distinguished from `#schema` by the
/// directive name. Visibility is tied to the file's `#import` chain
/// (decision 9). Cannot re-declare X — only extend its method table.
///
/// Note: method-level `#internal` is the existing [`INTERNAL`] directive
/// reused — `#internal` already serves as a field-level marker that
/// keeps a dict-body binding visible to siblings but hidden from the
/// outer projection. In a `with { ... }` block, the same `#internal`
/// directive marks a method as schema-internal (only callable from
/// other method bodies on the same schema).
pub const EXTEND: &str = "extend";

/// Directive name → expected shape. Dispatch happens by name; unknown
/// `#name` produces a parse error.
pub const DIRECTIVE_SHAPES: &[(&str, DirectiveShape)] = &[
    (INTERNAL, DirectiveShape::Bare),
    (DEFAULT, DirectiveShape::Value),
    (EXPECT, DirectiveShape::Value),
    (MSG, DirectiveShape::Value),
    (ERROR, DirectiveShape::Value),
    (BRAND, DirectiveShape::Value),
    (SCHEMA, DirectiveShape::NameBody),
    (ENUM, DirectiveShape::Enum),
    (IMPORT, DirectiveShape::Import),
    (MAIN, DirectiveShape::Main),
    (RELAXED, DirectiveShape::Bare),
    (UNSTRICT, DirectiveShape::Bare),
    // Trait-bound / schema-method pragmas (Phase A): parsed globally,
    // semantic placement enforced by the analyzer.
    (DERIVE, DirectiveShape::Value),
    (NO_AUTO_DERIVE, DirectiveShape::Value),
    (NATIVE, DirectiveShape::Bare),
    (EXTEND, DirectiveShape::NameBody),
];

/// Look up a directive's expected shape by name. Returns `None` for
/// unknown directives.
pub fn directive_shape(name: &str) -> Option<DirectiveShape> {
    DIRECTIVE_SHAPES
        .iter()
        .find_map(|(n, s)| (*n == name).then_some(*s))
}

/// True when an `#import` path looks like a URL the remote resolver
/// chain knows how to handle (`http://` / `https://`). Centralized
/// here so every layer that classifies import paths — the analyzer's
/// `--require-hash` scoping, the evaluator's `RemoteHttpResolver`
/// gating, and the facade's sandboxed-posture short-circuit (including
/// the wasm32 build, which never links the resolver) — agrees on the
/// exact same prefix set.
pub fn is_remote_url(path: &str) -> bool {
    path.starts_with("https://") || path.starts_with("http://")
}
