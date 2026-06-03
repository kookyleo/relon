//! S3.X wasm32 link step: turn an LLVM-emitted **relocatable** wasm
//! object (`\0asm` with a `linking` custom section, undefined symbols,
//! no exports / no memory) into an **instantiable** wasm module.
//!
//! `LlvmAotEvaluator::emit_object_for_target(.., CodegenTarget::Wasm32)`
//! writes a relocatable object — the LLVM WebAssembly backend emits the
//! same object-file shape `clang -c --target=wasm32` produces. wasmtime
//! cannot instantiate that directly; it needs the linker pass that
//! materialises the `memory`, the `globals` (stack pointer), and the
//! function `export`s. We shell out to `wasm-ld` for that, mirroring how
//! a `clang --target=wasm32` toolchain finishes the build.
//!
//! `wasm-ld` is the LLVM linker shipped with the `lld` package; we probe
//! the common binary names (`wasm-ld`, `wasm-ld-NN`). The relocatable
//! wasm object format is stable across recent LLVM majors, so a system
//! `wasm-ld-17` happily links an LLVM-18-emitted object.

use std::path::Path;
use std::process::Command;

use crate::error::LlvmError;

/// Candidate `wasm-ld` binary names, most-specific first. The LLVM-18
/// build the emitter uses doesn't ship `wasm-ld` in `/usr/lib/llvm-18`
/// on every distro, but the wasm object format is forward/back-compatible
/// across these majors for the link step.
const WASM_LD_CANDIDATES: &[&str] = &[
    "wasm-ld",
    "wasm-ld-18",
    "wasm-ld-17",
    "wasm-ld-19",
    "wasm-ld-16",
];

/// Locate a usable `wasm-ld` on `PATH`. Returns the binary name (for
/// `Command::new`) or `None` when no candidate responds to `--version`.
pub fn find_wasm_ld() -> Option<String> {
    for name in WASM_LD_CANDIDATES {
        let ok = Command::new(name)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            return Some((*name).to_string());
        }
    }
    None
}

/// Link a relocatable wasm object (`obj_path`) into an instantiable
/// wasm module written to `out_path`, exporting `entry_symbol` and the
/// linear `memory`.
///
/// Flags:
/// - `--no-entry`: there is no `_start` / `main`; the module is a
///   library whose entry is the exported relon symbol.
/// - `--export=<entry_symbol>` + `--export=__heap_base`: surface the
///   relon entry (and the heap base, useful for the buffer-arena
///   handshake) to the host.
/// - `--allow-undefined`: tolerate unresolved imports (e.g. a future
///   WASI host fn) — they become wasm `import`s the host satisfies.
/// - `--export-memory` (implicit default) yields the `memory` export
///   wasmtime reads for the arena handshake.
pub fn link_wasm_object(
    obj_path: &Path,
    out_path: &Path,
    entry_symbol: &str,
) -> Result<(), LlvmError> {
    let ld = find_wasm_ld().ok_or_else(|| {
        LlvmError::Codegen(
            "wasm-ld not found on PATH (install `lld` / `wasm-ld`); required to link the \
             relocatable wasm32 object into an instantiable module"
                .into(),
        )
    })?;
    let output = Command::new(&ld)
        .arg("--no-entry")
        .arg("--allow-undefined")
        .arg(format!("--export={entry_symbol}"))
        // `__heap_base` is a synthetic global the linker emits marking
        // the first byte past the static data; the buffer-arena
        // handshake lays its `ArenaState` + arena there. Harmless export
        // for the fast path (no consumer reads it).
        .arg("--export=__heap_base")
        .arg(obj_path)
        .arg("-o")
        .arg(out_path)
        .output()
        .map_err(|e| LlvmError::Codegen(format!("spawn {ld}: {e}")))?;
    if !output.status.success() {
        return Err(LlvmError::Codegen(format!(
            "{ld} failed ({}):\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}
