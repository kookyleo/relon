// Pre-baked example sources offered as a quick-switch dropdown in the
// playground. Contents are inlined (rather than fetched at runtime) so
// the playground stays self-contained — no extra network round trips,
// no build-step magic for `?raw` imports, and the bundle works whether
// VitePress is served from `/`, `/relon/`, or a CDN sub-path.
//
// Each preset declares its own `entry` plus a `runnableInSandbox` flag.
// When `false`, the playground will still call `evaluate()` (we don't
// want a special-case branch that hides errors) — but a contextual
// banner explains why an `EvalError` is the expected outcome. Most
// `#main(...)` examples are runnable here because they carry
// `defaultArgs`; presets that need host-only wiring should remain
// explicit rather than failing as the first impression.

export interface PresetFile {
    path: string;
    content: string;
}

export interface Preset {
    id: string;
    label: string;
    files: PresetFile[];
    entry: string;
    runnableInSandbox: boolean;
    /** Shown above the error panel when this preset is active. */
    note?: string;
    /** Pre-fills the playground's "Args" input. Pretty-printed JSON. */
    defaultArgs?: string;
}

const DEMO_MAIN = `// Try editing me - evaluate runs automatically.
//
// Strict (default) mode: closure/function params need explicit types,
// and a path-referenced dict (here &root.project.name in the f-string)
// needs a schema so the analyzer can derive each field's type.
#schema Details {
    Int base_price: *,
    Float total: *,
    String display: *
}
#schema Project {
    String name: *,
    Details details: *
}
{
    String currency(Float val, String symbol): val + " " + symbol,
    Float multiply(Int a, Float b): a * b,
    Project project: {
        name: "Relon Playground",
        details: {
            base_price: 1500,
            total: multiply(&sibling.base_price, 1.2),
            @currency("GBP")
            display: &sibling.total
        }
    },
    meta: {
        tags_count: len(["rust", "config", "dsl"]),
        summary: f"Active project: \${&root.project.name}"
    }
}
`;

const PRICING_MAIN = `/*
  Invoice pricing with tiered discounts and tax.
  See examples/pricing.relon in the repo for the full annotated source.

  Strict (default) mode: every closure/function parameter carries an
  explicit type and each schema #expect validator is written with typed
  params and a Bool return type.
*/
#schema LineItem {
    String sku: * ,
    #expect "qty must be > 0"
    Int qty: (Int n) -> Bool => n > 0,
    #expect "unit_price must be >= 0"
    Float unit_price: (Float p) -> Bool => p >= 0
}
#schema Order {
    List<LineItem> items: * ,
    #expect "tier must be one of: standard / gold"
    String tier: (String t) -> Bool => t == "standard" || t == "gold"
}
#main(Order order)
{
    #internal
    String currency(Float symbol, String val): symbol + " " + val,
    #internal
    Float volume_rate(Float sub): sub >= 1000 ? 0.10: sub >= 500 ? 0.05: 0.0,
    #internal
    Float loyalty_rate(String tier): tier == "gold" ? 0.03: 0.0,
    #internal
    tax_rate: 0.08,
    #internal
    Float sum_floats(List<Float> xs): _list_reduce(xs, 0.0, (a, x) => a + x),
    subtotal: sum_floats([it.qty * it.unit_price for it in order.items]),
    discount_rate: volume_rate(&sibling.subtotal) + loyalty_rate(order.tier),
    discount_amount: &sibling.subtotal * &sibling.discount_rate,
    taxable: &sibling.subtotal - &sibling.discount_amount,
    tax_amount: &sibling.taxable * tax_rate,
    total: &sibling.taxable + &sibling.tax_amount,
    @currency("USD")
    total_display: &sibling.total
}
`;

const FEATURE_FLAG_MAIN = `/*
  Feature flags as a deterministic decision table.

  The host computes a stable rollout bucket before evaluation and pushes
  it with the user. Relon stays pure: no clock, no RNG, no native hash.
*/
#schema User {
    String id: * ,
    #expect "region must be us / eu / apac"
    String region: (String region) -> Bool => region == "us" || region == "eu" || region == "apac",
    #expect "plan must be free / pro / enterprise"
    String plan: (String plan) -> Bool => plan == "free" || plan == "pro" || plan == "enterprise",
    #expect "rollout_bucket must be 0..99"
    Int rollout_bucket: (Int n) -> Bool => n >= 0 && n < 100
}

#main(User user)
{
    flags: {
        legacy_checkout: false,
        dark_mode: true,
        gdpr_banner: user.region == "eu",
        advanced_editor: user.plan == "pro" || user.plan == "enterprise",
        new_search: user.rollout_bucket < 25
    },
    audit: {
        subject: user.id,
        bucket: user.rollout_bucket,
        ruleset: "2026-06-11"
    }
}
`;

const WORKFLOW_MAIN = `/*
  Order workflow as a data-driven state machine.

  Try via the CLI:
    cargo run -q -p relon-cli -- run examples/workflow.relon \\
        --args '{"state": "placed", "event": "pay"}'

  Strict mode: the schema validators and the #internal helper carry
  explicit parameter and return types, and matched is typed so the
  ternaries' field reads resolve statically.
*/
#schema Transition {
    String from: (String s) -> Bool => s == "placed" || s == "paid" || s == "shipped",
    String on: * ,
    String to: (String s) -> Bool => s == "paid" || s == "shipped" || s == "delivered" || s == "cancelled",
    List<String> emit: *
}
#main(String state, String event)
{
    #internal
    transitions: [
        #brand Transition { from: "placed", on: "pay",     to: "paid",      emit: ["charge_card", "log_payment"] },
        #brand Transition { from: "paid",   on: "ship",    to: "shipped",   emit: ["notify_shipper", "email_user"] },
        #brand Transition { from: "shipped",on: "deliver", to: "delivered", emit: ["email_user"] },
        #brand Transition { from: "placed", on: "cancel",  to: "cancelled", emit: [] },
        #brand Transition { from: "paid",   on: "cancel",  to: "cancelled", emit: ["refund_card"] }
    ],
    #internal
    match_one(Transition t) -> Bool: t.from == state && t.on == event,
    #internal
    List<Transition> matched: _list_filter(&sibling.transitions, &sibling.match_one),
    next_state: len(matched) > 0 ? matched.0.to: state,
    emit: len(matched) > 0 ? matched.0.emit: ["unhandled_event"]
}
`;

// Multi-file preset — exercises the cross-file `#import` path so the
// playground's tab bar + workspace analyzer are visibly involved. The
// entry pulls in a small "currency" lib via three import shapes so
// users can see all three forms (alias, destructure, spread) in one
// place and compare them.
const MODULES_MAIN = `// Three #import shapes — try Mod-clicking any imported name to
// jump across files.
//
// #relaxed is per-module (file-local): each module declares its own
// mode. main.relon's #relaxed governs only main.relon, so it does NOT
// reach lib.relon — the entry's directive is never stamped onto its
// imports. That is why lib.relon carries its OWN #relaxed to keep its
// untyped helpers (with_tax / format_price / discount) legal. This is
// the playground's one deliberate #relaxed demo; every other preset
// uses strict (the default).
#relaxed
#import lib from "./lib.relon"
#import { format_price } from "./lib.relon"
#import * from "./lib.relon"
{
    namespaced: lib.with_tax(100.0, 0.08),

    destructured: format_price(199.99, "USD"),

    spread: discount(50.0, 0.15)
}
`;

const MODULES_LIB = `// Pricing helpers shared by main.relon. This library declares its
// OWN #relaxed so its untyped closure params stay legal — the entry's
// #relaxed does not reach here under per-module strictness.
#relaxed
{
    with_tax(amount, rate): amount * (1.0 + rate),

    format_price(value, symbol): symbol + " " + value,

    discount(amount, rate): amount * (1.0 - rate)
}
`;

export const PRESETS: Preset[] = [
    {
        id: 'demo',
        label: 'demo',
        files: [{ path: 'main.relon', content: DEMO_MAIN }],
        entry: 'main.relon',
        runnableInSandbox: true,
    },
    {
        id: 'pricing',
        label: 'pricing',
        files: [{ path: 'main.relon', content: PRICING_MAIN }],
        entry: 'main.relon',
        runnableInSandbox: false,
        note: '`#main(Order order)` expects an `order` argument. Fill the Args box (right side of the title bar) and click Run — or use the CLI with the same JSON.',
        defaultArgs: `{
  "order": {
    "tier": "gold",
    "items": [
      { "sku": "BOOK-01", "qty": 3, "unit_price": 100.0 },
      { "sku": "PEN-09",  "qty": 4, "unit_price": 50.0  },
      { "sku": "DESK-22", "qty": 1, "unit_price": 300.0 }
    ]
  }
}`,
    },
    {
        id: 'feature_flag',
        label: 'feature_flag',
        files: [{ path: 'main.relon', content: FEATURE_FLAG_MAIN }],
        entry: 'main.relon',
        runnableInSandbox: true,
        defaultArgs: `{
  "user": { "id": "alice-42", "region": "eu", "plan": "pro", "rollout_bucket": 17 }
}`,
    },
    {
        id: 'workflow',
        label: 'workflow',
        files: [{ path: 'main.relon', content: WORKFLOW_MAIN }],
        entry: 'main.relon',
        runnableInSandbox: false,
        note: '`#main(String state, String event)` expects `state` and `event` arguments. Fill the Args box (right side of the title bar) and click Run.',
        defaultArgs: `{
  "state": "placed",
  "event": "pay"
}`,
    },
    {
        id: 'modules',
        label: 'modules',
        files: [
            { path: 'main.relon', content: MODULES_MAIN },
            { path: 'lib.relon', content: MODULES_LIB },
        ],
        entry: 'main.relon',
        runnableInSandbox: true,
        note: 'Cross-file workspace. Mod-click `lib`, `format_price`, or `discount` to jump into lib.relon — the tab bar switches and the destination key is selected.',
    },
];

export const DEFAULT_PRESET_ID = 'demo';
