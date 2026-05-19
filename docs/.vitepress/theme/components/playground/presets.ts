// Pre-baked example sources offered as a quick-switch dropdown in the
// playground. Contents are inlined (rather than fetched at runtime) so
// the playground stays self-contained — no extra network round trips,
// no build-step magic for `?raw` imports, and the bundle works whether
// VitePress is served from `/`, `/relon/`, or a CDN sub-path.
//
// Each preset declares its own `entry` plus a `runnableInSandbox` flag.
// When `false`, the playground will still call `evaluate()` (we don't
// want a special-case branch that hides errors) — but a dismissable
// banner explains why an `EvalError` is the expected outcome. The four
// `#main(...)` examples need either CLI `--args` or host-registered
// native functions; running them in the in-browser sandbox surfaces a
// genuine, demo-correct failure, and the banner points users at the
// CLI command that does work.

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
{
    currency(val, symbol): val + " " + symbol,
    multiply(a, b): a * b,
    project: {
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

  Uses #relaxed to keep the existing untyped closure params in the
  helper functions; analyzer strict mode would otherwise demand
  explicit closure-parameter types.
*/
#relaxed
#schema LineItem {
    String sku: * ,
    #expect "qty must be > 0"
    Int qty: (n) => n > 0,
    #expect "unit_price must be >= 0"
    Float unit_price: (p) => p >= 0
}
#schema Order {
    List<LineItem> items: * ,
    #expect "tier must be one of: standard / gold"
    String tier: (t) => t == "standard" || t == "gold"
}
#main(Order order)
{
    #private
    currency(symbol, val): symbol + " " + val,
    #private
    volume_rate(sub): sub >= 1000 ? 0.10: sub >= 500 ? 0.05: 0.0,
    #private
    loyalty_rate(tier): tier == "gold" ? 0.03: 0.0,
    #private
    tax_rate: 0.08,
    #private
    sum_floats(xs): _list_reduce(xs, 0.0, (a, x) => a + x),
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
  Runtime feature-flag evaluator.

  Percentage rollouts need a host-registered \`native_hash(s) -> Int\`.
  See examples/feature_flag.relon for the full annotated source.

  #relaxed lets the demo keep its untyped closure params in
  schema validators / helper fns; analyzer strict mode would
  otherwise require explicit parameter types.
*/
#relaxed
#schema User {
    String id: * ,
    String region: (r) => r == "us" || r == "eu" || r == "apac",
    String plan: (p) => p == "free" || p == "pro" || p == "enterprise"
}
#main(User user) -> Dict<String, Dict<String, Bool>>
{
    #private
    hash_mod_100(s): native_hash(s) % 100,
    #private
    rules: {
        legacy_checkout: (u) => false,
        dark_mode: (u) => true,
        gdpr_banner: (u) => u.region == "eu",
        advanced_editor: (u) => u.plan == "pro" || u.plan == "enterprise",
        new_search: (u) => hash_mod_100(u.id) < 25
    },
    flags: {
        legacy_checkout: rules.legacy_checkout(user),
        dark_mode: rules.dark_mode(user),
        gdpr_banner: rules.gdpr_banner(user),
        advanced_editor: rules.advanced_editor(user),
        new_search: rules.new_search(user)
    }
}
`;

const WORKFLOW_MAIN = `/*
  Order workflow as a data-driven state machine.

  Try via the CLI:
    cargo run -q -p relon-cli -- run examples/workflow.relon \\
        --args '{"state": "placed", "event": "pay"}'

  #relaxed lets the schema validators use untyped closure
  params; strict mode by default would error otherwise.
*/
#relaxed
#schema Transition {
    String from: (s) => s == "placed" || s == "paid" || s == "shipped",
    String on: * ,
    String to: (s) => s == "paid" || s == "shipped" || s == "delivered" || s == "cancelled",
    List<String> emit: *
}
#main(String state, String event)
{
    #private
    transitions: [
        #brand Transition { from: "placed", on: "pay",     to: "paid",      emit: ["charge_card", "log_payment"] },
        #brand Transition { from: "paid",   on: "ship",    to: "shipped",   emit: ["notify_shipper", "email_user"] },
        #brand Transition { from: "shipped",on: "deliver", to: "delivered", emit: ["email_user"] },
        #brand Transition { from: "placed", on: "cancel",  to: "cancelled", emit: [] },
        #brand Transition { from: "paid",   on: "cancel",  to: "cancelled", emit: ["refund_card"] }
    ],
    #private
    match_one(t): t.from == state && t.on == event,
    #private
    matched: _list_filter(&sibling.transitions, &sibling.match_one),
    next_state: len(matched) > 0 ? matched[0].to: state,
    emit: len(matched) > 0 ? matched[0].emit: ["unhandled_event"]
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
// #relaxed propagates from the entry to every reachable #import target,
// so lib.relon's untyped closure params (with_tax / format_price / discount)
// are accepted without explicit type annotations.
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

const MODULES_LIB = `// Pricing helpers shared by main.relon.
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
        runnableInSandbox: false,
        note: '`#main(User user)` expects a `user` argument *and* a host-registered `native_hash` fn. The Args input takes care of the first; the browser sandbox can\'t register host fns, so evaluate still fails on `new_search` — see the host-integration guide for wiring.',
        defaultArgs: `{
  "user": { "id": "u123", "region": "us", "plan": "pro" }
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
