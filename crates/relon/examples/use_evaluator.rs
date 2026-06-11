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
    public_api: Endpoint,
    bind: String,
    probe: String,
}

#[derive(Debug, Deserialize)]
struct Endpoint {
    name: String,
    port: i64,
    protocol: String,
    health_path: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Inline Relon source. The schema validates the config at load time,
    // then computed fields produce the exact host-facing values. The
    // facade evaluates everything in a default-sandboxed context so the
    // source can't reach the filesystem or touch host capabilities.
    let source = r#"
#schema Endpoint {
    String name: *,
    #expect "port must be in the non-privileged TCP range"
    Int port: (Int p) -> Bool => p >= 1024 && p <= 65535,
    #expect "protocol must be http or https"
    String protocol: (String p) -> Bool => p == "http" || p == "https",
    #expect "health_path must be absolute"
    String health_path: (String path) -> Bool => path.starts_with("/")
}

{
    Endpoint public_api: {
        name: "api",
        port: 8443,
        protocol: "https",
        health_path: "/healthz"
    },
    bind: f"${&sibling.public_api.protocol}://0.0.0.0:${&sibling.public_api.port}",
    probe: &sibling.public_api.health_path
}
"#;

    // One-call evaluation + projection. The generic parameter steers
    // serde_json's deserialisation; any `DeserializeOwned` type works.
    let config: Config = relon::from_str(source)?;

    println!("service = {}", config.public_api.name);
    println!("bind    = {}", config.bind);
    println!("probe   = {}", config.probe);
    println!("port    = {}", config.public_api.port);
    println!("scheme  = {}", config.public_api.protocol);
    println!("health  = {}", config.public_api.health_path);

    // The same source can also be projected straight to
    // `serde_json::Value` (skip the typed `Config` step) or to a
    // `relon::Value` (the runtime data shape, useful when the host
    // wants to inspect Relon-specific kinds like closures / thunks
    // that don't have a JSON equivalent).
    let json: serde_json::Value = relon::json_from_str(source)?;
    println!("\nas JSON:\n{}", serde_json::to_string_pretty(&json)?);

    Ok(())
}
