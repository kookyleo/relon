# What is Relon?

Relon is a configuration language and UI template engine designed for the modern web and industrial-grade applications. It merges the simplicity of JSON with the power of a fully-fledged expression language and strong type system.

## Key Features

- **Expression-oriented**: Everything evaluates to a value. There are no statements.
- **Strict JSON Consistency**: Relon is designed to be easily transformable into pure JSON, while retaining structural and nominal constraints internally.
- **Nominal Types & Branding**: Support for nominal identity validation through schemas.
- **Dynamic Pathing**: Build flexible UI definitions and configurations using dynamic token keys.

## Quick Example

```javascript
{
    @schema User: {
        String name: *,
        @expect("Age must be positive")
        Int age: (a) => a > 0
    },
    
    User alice: { name: "Alice", age: 25 }
}
```

Dive in and explore how Relon can harden your configurations!
