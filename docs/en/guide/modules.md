# Modules & Scope

When configurations grow, you naturally want to split them across
files. Relon provides a module system based on the `#import`
directive. Because Relon is a declarative language with no global
variables, modules are the right boundary for organizing reusable
logic.

## Importing

At the top level of a dict or file, use `#import` to pull in other
`.relon` files. The unified syntax:

```text
#import <bindspec> from "<path>"
```

`<bindspec>` has three forms:

| Form | Spelling | Meaning |
| --- | --- | --- |
| Namespace | `lib` | Bind the entire module to the name `lib` |
| Spread | `*` | Merge every exported field of the module into the current scope |
| Destructuring | `{ a, b as c }` | Take only `a` and `b`, renaming `b` to `c` |

### Namespace import

The most common and safest form. The engine evaluates the target
file and exposes it as a "module object" bound to the name you give:

```relon
// main.relon
#import theme from "./lib.relon",
{
    // Call helper functions or use color variables from the theme module
    button_color: theme.colors.primary,

    // Or reference a schema from it
    theme.ButtonConfig my_button: { label: "Click" }
}
```

### Spread import

If you have a bunch of common schemas or pure helpers, going through
a namespace every time gets tedious. Use `*` to "destructure" every
top-level variable of the target file into the current scope:

```relon
#import * from "./helpers.relon",
{
    // If helpers.relon exports a shout function, you can call it directly
    msg: shout("hello")
}
```

### Destructuring import

When you want only a few names, possibly renamed:

```relon
#import { upper, lower as lo } from "std/string",
{
    a: upper("hello"),
    b: lo("WORLD")
}
```

#### Import protection

If a spread import causes a name collision, the **import overwrites**.
To protect a namespace, mark fields you don't want to be spread-
imported with the `#internal` directive: private fields aren't written
to the module's export map, so spread imports skip them naturally, and
namespace-form access also can't reach them
(`lib.private_field` → `VariableNotFound`).

```relon
// helpers.relon
{
    // Will be spread-imported
    shout(v): v + "!!!",

    // Private helper: not exported by any #import form
    #internal
    add(a, b): a + b
}
```

> Historical note: early versions used a `_` prefix as an implicit
> convention and an `@private` decorator. Both are **fully retired**;
> use the `#internal` directive. See
> [Syntax basics](./syntax#field-visibility-—-internal).

## Entry programs vs libraries

Relon has no file-level "library/entry" marker. Whether a file
declares `#main(...)` decides **how it's used**:

- The file **has** `#main(...)`: it's an entry program. The host must
  push arguments via `Evaluator::run_main(scope, args)` for it to
  evaluate. `#import`-ing it as a library is also allowed (the args
  aren't used; only its exports).
- The file **lacks** `#main(...)`: it's a "contractless" pure-data /
  library file. It can be used as a module via `#import` or evaluated
  directly by the host via `eval_root` to get plain JSON.

A complete example:

```relon
// app/main.relon — entry program
#import * from "../platform/notify.relon",
#main(Notification notice)
{
    delivered: notice.title + " (via " + notice.via + ")"
}
```

```relon
// platform/notify.relon — shared library (no #main)
{
    #schema Channel Enum<
        Email { String address: *, String subject: * },
        SMS   { String phone: * },
        Push
    >,
    #schema Notification {
        Channel via: *,
        String title: *
    }
}
```

Host side:

```rust
let mut args = HashMap::new();
args.insert(
    "notice".to_string(),
    /* host-pushed Value::Dict */ notice_value,
);
let result = evaluator.run_main(&scope, args)?;
```

If the host tries to `eval_root` an entry program directly, it gets
`NoMainSignature` — the error is caught at the boundary and never
enters evaluation. Conversely, calling `run_main` on a library file
without `#main` also raises `NoMainSignature`.

## Relative references

When you don't need cross-file modules, you can use "kinship
references" inside a single deeply nested object to access surrounding
data.

Relon supports these locators:
- `&root`: always points at the top-most dict of the current file's
  AST.
- `&sibling`: same-level scope.
- `&uncle`: the parent's sibling (one level up).

In list processing, cursor-based relative references are available:
- `&prev`: the previous list element (null on the first element).
- `&next`: the next list element (supports lookahead).
- `&index`: the current element's index (Int).
- `&this`: the top-level context container of the list or traversal.

```relon
{
    steps: [
        { title: "Step 1", done: true,  next_ready: &next.done },
        { title: "Step 2", done: false, index: &index }
    ]
}
```

Combining `&prev` and `&next`, Relon handles state-machine configs
and complex list workflows gracefully — all thanks to its lazy-by-
default evaluation model (thunks).
