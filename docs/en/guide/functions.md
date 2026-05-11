# Functions & Closures

In Relon, functions are first-class citizens. They can be stored in
dicts, passed as arguments to other functions, or called elsewhere
through relative references.

To match different use cases, Relon offers two function syntaxes.

## Double-track syntax

### 1. Method shorthand

If you're writing something like a standard library or a component's
business logic, you'll usually put functions at the top level of a
dict. Method shorthand keeps the structure clean:

```relon
{
    // Without type annotations
    sum(a, b): a + b,

    // With type annotations (recommended)
    Int multiply(Int a, Int b): a * b
}
```

*Tip: method shorthand defines a key/value pair in a dict. In long
files it reads better than `"sum": (a, b) => a + b`.*

### 2. Arrow functions

When passing a small inline lambda to a higher-order function like
`map` or `filter`, an arrow function is the natural choice:

```relon
#import list from "std/list"
{
    numbers: [1, 2, 3, 4, 5],
    // A higher-order function from the standard library
    doubled: list.map(&sibling.numbers, (x) => x * 2),

    // Arrow function with type annotations
    evens: list.filter(&sibling.numbers, (Int x) -> Bool => x % 2 == 0)
}
```

## Pipe operator

When working with data flows, nested calls like `a(b(c(x)))` are hard
to read. Relon's `|` pipe operator passes the result of the previous
expression as the **first argument** of the next call:

```relon
#import string from "std/string"
{
    words: "apple,banana,cherry",

    // Standard nested call (list.len is a list-module member; len is
    // a language-level builtin, same below)
    count_normal: len(string.split(&sibling.words, ",")),

    // Same expression with pipe — flows left to right
    count_piped: &sibling.words | string.split(",") | len()
}
```

## `where` binding blocks

Sometimes you don't want to extract a separate function — you just
want to avoid recomputing a value inside one long expression. Use
the `where` clause:

`where` binds temporary local variables to a single expression.

```relon
{
    volume: (width * height * depth) where {
        width: 10,
        height: 20,
        depth: 5
    }
}
```

## Recursion

Since dict keys can reference each other within their scope (or you
can use `&sibling` directly), you can write recursive closures:

```relon
{
    // Define a factorial function
    factorial(n): n <= 1 ? 1 : n * factorial(n - 1),

    // Call it
    result: factorial(5) // 120
}
```
