//! Facade entry-point example.
//!
//! Demonstrates the shortest open-the-box path: feed Relon source to
//! `relon::from_str` and the facade returns a deserialised host value
//! via serde. No backend selection, no capability tweaking, no
//! `Context` construction — useful when the host just wants to treat
//! a Relon file as "JSON with computed fields".
//!
//! Run with:
//!
//! ```sh
//! cargo run -p relon --example use_evaluator
//! ```

use serde::Deserialize;

/// Host-side projection of the Relon document. `relon::from_str`
/// evaluates the source and then `serde_json::from_value` walks the
/// resulting JSON shape into this struct.
#[derive(Debug, Deserialize)]
struct Config {
    project: Project,
    meta: Meta,
}

#[derive(Debug, Deserialize)]
struct Project {
    name: String,
    base_price: i64,
    total: f64,
}

#[derive(Debug, Deserialize)]
struct Meta {
    tags_count: i64,
    summary: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Inline Relon source. Mixes user-defined logic
    // (`multiply(...)`), sibling references (`&sibling.base_price`),
    // an f-string interpolation (`f"..."`), and a stdlib call
    // (`len(...)`). The facade evaluates everything in a default-
    // sandboxed context so the source can't reach the filesystem or
    // touch host capabilities.
    let source = r#"
#relaxed
{
    multiply(a, b): a * b,

    project: {
        name: "Relon Modern",
        base_price: 1500,
        total: multiply(&sibling.base_price, 1.2)
    },

    meta: {
        tags_count: len(["rust", "config", "dsl"]),
        summary: f"Active project: ${&root.project.name}"
    }
}
"#;

    // One-call evaluation + projection. The generic parameter steers
    // serde_json's deserialisation; any `DeserializeOwned` type works.
    let config: Config = relon::from_str(source)?;

    println!("project   = {}", config.project.name);
    println!("base      = {}", config.project.base_price);
    println!("total     = {}", config.project.total);
    println!("tags_cnt  = {}", config.meta.tags_count);
    println!("summary   = {}", config.meta.summary);

    // The same source can also be projected straight to
    // `serde_json::Value` (skip the typed `Config` step) or to a
    // `relon::Value` (the runtime data shape, useful when the host
    // wants to inspect Relon-specific kinds like closures / thunks
    // that don't have a JSON equivalent).
    let json: serde_json::Value = relon::json_from_str(source)?;
    println!("\nas JSON:\n{}", serde_json::to_string_pretty(&json)?);

    Ok(())
}
