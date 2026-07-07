//! Build-script API for compiling Relon sources to relocatable object
//! files at build time.
//!
//! The crate is the bridge between a `.relon` source on disk and the
//! Rust binary that wants to call its `#main` as if it were a native
//! `extern "C"` function. The build.rs side calls into [`Compiler`];
//! the consuming source then `include_relon!`s the matching binding
//! file (`relon-rs-macro`) to import the typed Rust wrapper.
//!
//! ## Supported signature surface
//!
//! The accepted leaf types for `#main` parameters and the return slot
//! are `Int`, `Float`, `Bool`, `String`, and `List<Int>`; internal unit slots map to Rust `()` (the
//! [`rust_type_for`] table is the authoritative list — a new
//! codegen-llvm leaf variant fails the exhaustive `match` until a row
//! is added). Each `.relon` source contributes:
//!
//! - One relocatable ELF object file under `out_dir`, exporting a
//!   single extern symbol `__relon_<hash>_main`. The signature depends
//!   on the entry shape: an Int-only `#main(Int...) -> Int` qualifies
//!   for the fast path (`extern "C" fn(i64, ...) -> i64`); every other
//!   accepted shape carries the canonical buffer-protocol signature
//!   (`extern "C" fn(*const ArenaState, i32, i32, i32, i32, i64) ->
//!   i32`) and marshals its typed args through
//!   `relon-rs-shims::call_buffer_entry`.
//! - One generated `.rs` binding file that names the safe Rust shim
//!   (module-scoped by default, or aliased via `.source_as` /
//!   `include_relon!("... " as alias)`).
//!
//! The build.rs caller emits the cargo directives that pull the
//! object into the linker invocation. Object files end up under
//! `OUT_DIR` so cargo's incremental story stays intact.
//!
//! ## Usage
//!
//! ```ignore
//! // crates/my-app/build.rs
//! fn main() {
//!     let out_dir = std::env::var_os("OUT_DIR").unwrap();
//!     relon_rs_build::Compiler::new()
//!         .source("src/foo.relon")
//!         .emit_all(&out_dir)
//!         .unwrap();
//! }
//! ```
//!
//! ```ignore
//! // crates/my-app/src/main.rs
//! relon_rs_macro::include_relon!("src/foo.relon");
//!
//! fn main() {
//!     let state = relon_rs_shims::SandboxState::default();
//!     println!("{}", foo::main(&state, 42));
//! }
//! ```

#![warn(missing_docs)]
#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use relon_codegen_llvm::{EmittedEntryShape, EmittedField, EmittedFieldType, LlvmAotEvaluator};

/// Per-native-fn capability requirement, re-exported from
/// `relon-analyzer` so a `build.rs` consumer can construct a
/// [`NativeHostFn`] gate without taking a direct dep on the analyzer
/// crate. Build a default and flip the bits the host fn needs:
/// `let mut g = NativeFnGate::default(); g.reads_clock = true;`.
pub use relon_analyzer::NativeFnGate;

/// Optimisation level requested from the LLVM backend. Phase 1 always
/// runs the same `-O3` middle-end pipeline regardless of this value;
/// the type exists so the [`Compiler::opt_level`] setter can express
/// intent today and gain effect when the backend grows a per-emit knob.
#[derive(Debug, Clone, Copy, Default)]
pub enum OptLevel {
    /// Default `-O3` pipeline. The only level Phase 1 supports.
    #[default]
    Aggressive,
}

/// One declared source entry (path + optional alias). The alias drives
/// both the LLVM exported symbol's stable prefix and the generated
/// Rust module / function name.
#[derive(Debug, Clone)]
struct SourceEntry {
    /// Absolute or build.rs-relative path to the `.relon` file.
    path: PathBuf,
    /// Rust-side identifier the binding generator uses. When `None`
    /// the file basename (e.g. `foo` for `src/foo.relon`) is used.
    alias: Option<String>,
    /// Host `#native` functions this source calls by name. When
    /// non-empty, [`Compiler::emit_all`] lowers the source closed-world
    /// (`Op::CallNative` → `call @<host_symbol>`), threading the
    /// matching [`relon_analyzer::AnalyzeOptions`] so the host
    /// declarations resolve and the host shim Rust source links + inlines
    /// into the `.o`. Empty for a pure source (open-world, the historical
    /// path).
    host_fns: Vec<NativeHostFn>,
}

/// Declaration of one host-provided `#native` function a `.relon`
/// source calls by name. The consuming crate's `build.rs` registers
/// it via [`Compiler::native_fn`] / [`Compiler::source_with_native_fns`].
///
/// The closed-world emit path uses this to (1) resolve the call name in
/// the analyzer (`host_fn_names` / `host_fn_signatures`), (2) bake the
/// `Op::CheckCap` capability gate from `gate`, and (3) compile +
/// inline `rust_impl` (a `#[no_mangle] extern "C"` Rust body) into the
/// emitted object so the linked binary is self-contained.
#[derive(Debug, Clone)]
pub struct NativeHostFn {
    /// Function name as called from the `.relon` source. Must match the
    /// `#[no_mangle]` symbol exported by [`Self::rust_impl`].
    pub name: String,
    /// Scalar parameter leaf types, in declaration order. Phase G1
    /// wires the scalar lane the closed-world dynamic envelope accepts:
    /// `"Int"`, `"Bool"`. Each maps onto an `i64` C ABI slot.
    pub param_types: Vec<String>,
    /// Return leaf type — for example `"Int"`, `"Bool"`, or an internal unit slot.
    pub return_type: String,
    /// Capability bits this function requires. A non-empty gate makes
    /// the source gated: the binding surfaces `Result<T,
    /// BufferEntryError>` so a runtime denial returns a typed
    /// `CapabilityDenied` rather than panicking.
    pub gate: relon_analyzer::NativeFnGate,
    /// `#[no_mangle] pub extern "C" fn <name>(...) -> ...` Rust source
    /// the closed-world co-compile links + inlines into the `.o`.
    pub rust_impl: String,
}

/// Fluent compile-orchestrator the build.rs caller drives.
///
/// Holds the queue of `.relon` sources and the per-emit knobs. Once
/// the caller has registered every source, [`Self::emit_all`] runs
/// the full parse + analyze + lower + LLVM-emit-to-object pipeline
/// and writes the matching Rust binding file.
#[derive(Debug, Default)]
pub struct Compiler {
    sources: Vec<SourceEntry>,
}

/// Result of a successful [`Compiler::emit_all`] call. Lists every
/// object file produced and the per-source Rust binding files.
#[derive(Debug, Clone)]
pub struct EmitOutput {
    /// One absolute path per registered `.relon` source, in
    /// registration order.
    pub objects: Vec<PathBuf>,
    /// One Rust binding file per registered source — path layout is
    /// `OUT_DIR/relon_rs/<alias>.rs`. The `relon-rs-macro::include_relon!`
    /// macro stitches the matching file into the consuming source.
    pub bindings: Vec<PathBuf>,
}

/// Errors surfaced by the build pipeline. Wraps the LLVM crate's
/// errors plus a thin IO / path-validation surface.
#[derive(Debug)]
pub enum BuildError {
    /// `.relon` source path didn't resolve / file unreadable.
    Io(std::io::Error, PathBuf),
    /// LLVM emit pipeline rejected the source (parser / analyzer /
    /// codegen failure). The wrapped enum carries the per-phase
    /// detail.
    Llvm(relon_codegen_llvm::LlvmError, PathBuf),
    /// A registered source path lacks a recognisable file stem the
    /// binding generator can use as a Rust identifier (`.relon`
    /// directly in root, no basename, etc.).
    InvalidPath(PathBuf, String),
    /// Two registered sources collided on the same module identifier
    /// (e.g. `src/foo.relon` + `src/sub/foo.relon` without aliasing).
    DuplicateAlias(String),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildError::Io(e, p) => write!(f, "io error on `{}`: {e}", p.display()),
            BuildError::Llvm(e, p) => write!(f, "llvm emit failed for `{}`: {e}", p.display()),
            BuildError::InvalidPath(p, why) => {
                write!(f, "invalid source path `{}`: {why}", p.display())
            }
            BuildError::DuplicateAlias(name) => {
                write!(f, "duplicate module alias `{name}`")
            }
        }
    }
}

impl std::error::Error for BuildError {}

impl Compiler {
    /// Construct an empty compiler. Add sources via [`Self::source`]
    /// or [`Self::source_as`] before calling [`Self::emit_all`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a `.relon` source. The Rust module name in the
    /// generated bindings is derived from the file stem
    /// (`src/foo.relon` becomes `mod foo`).
    pub fn source<P: AsRef<Path>>(&mut self, path: P) -> &mut Self {
        self.sources.push(SourceEntry {
            path: path.as_ref().to_path_buf(),
            alias: None,
            host_fns: Vec::new(),
        });
        self
    }

    /// Register a `.relon` source that calls one or more host `#native`
    /// functions. The source lowers closed-world: each call resolves
    /// against the supplied [`NativeHostFn`] declarations, its
    /// capability gate is baked in, and the host Rust bodies link +
    /// inline into the emitted `.o` (the linked binary is
    /// self-contained — no separate host symbol to resolve at the
    /// consumer link step).
    ///
    /// A source with a gated host fn surfaces its binding as
    /// `Result<T, ::relon_rs_shims::BufferEntryError>`: an authorised
    /// call returns `Ok(value)`, an unauthorised one (the
    /// `SandboxState` didn't grant the required capability) returns
    /// `Err(BufferEntryError::CapabilityDenied)` rather than trapping.
    pub fn source_with_native_fns<P: AsRef<Path>>(
        &mut self,
        path: P,
        host_fns: Vec<NativeHostFn>,
    ) -> &mut Self {
        self.sources.push(SourceEntry {
            path: path.as_ref().to_path_buf(),
            alias: None,
            host_fns,
        });
        self
    }

    /// Register a `.relon` source with an explicit Rust identifier.
    /// The generated bindings expose the entry as a top-level
    /// function `pub fn <alias>(...)` rather than under a module —
    /// matching the `include_relon!("foo.relon" as alias)` macro
    /// shape.
    pub fn source_as<P: AsRef<Path>>(&mut self, path: P, alias: impl Into<String>) -> &mut Self {
        self.sources.push(SourceEntry {
            path: path.as_ref().to_path_buf(),
            alias: Some(alias.into()),
            host_fns: Vec::new(),
        });
        self
    }

    /// Set the optimisation level. **The value is currently ignored**:
    /// Phase 1 always runs the same `-O3` pipeline regardless of what
    /// is passed (there is no `-O0`..`-O2` today). The setter exists so
    /// consumers can express intent without breaking on the Phase 2
    /// widening that will make the level take effect.
    pub fn opt_level(&mut self, _level: u32) -> &mut Self {
        // Phase 1 ignores the value (pipeline is hard-wired to -O3).
        // The signature accepts a generic level so callers can write
        // `.opt_level(3)` without worrying about the enum surface.
        self
    }

    /// Run the full compile pipeline. Writes one `.o` per registered
    /// source plus a single Rust binding file under `out_dir`, and
    /// emits the cargo directives the linker needs to pull every
    /// produced object into the final binary.
    pub fn emit_all<P: AsRef<Path>>(&self, out_dir: P) -> Result<EmitOutput, BuildError> {
        let out_dir = out_dir.as_ref();
        std::fs::create_dir_all(out_dir).map_err(|e| BuildError::Io(e, out_dir.to_path_buf()))?;

        let mut objects: Vec<PathBuf> = Vec::with_capacity(self.sources.len());
        let mut binding_modules: Vec<BindingModule> = Vec::with_capacity(self.sources.len());
        let mut seen_aliases: std::collections::HashSet<String> = std::collections::HashSet::new();

        for entry in &self.sources {
            let canonical_path = entry.path.clone();
            // The Rust-side identifier — alias if supplied, file stem otherwise.
            let alias = match &entry.alias {
                Some(a) => a.clone(),
                None => file_stem_or_err(&canonical_path)?,
            };
            if !relon_util::is_valid_rust_ident(&alias) {
                return Err(BuildError::InvalidPath(
                    canonical_path,
                    format!("derived identifier `{alias}` is not a valid Rust identifier"),
                ));
            }
            if !seen_aliases.insert(alias.clone()) {
                return Err(BuildError::DuplicateAlias(alias));
            }

            let src = std::fs::read_to_string(&canonical_path)
                .map_err(|e| BuildError::Io(e, canonical_path.clone()))?;

            // Mangled C ABI symbol — `__relon_<8-char-hash>_main`. The
            // hash mixes alias + source content so two sources that
            // happen to share the same `#main` shape land on disjoint
            // symbols (avoiding the linker's "duplicate symbol"
            // diagnostic).
            let symbol = mangled_symbol(&alias, &src);
            let object_name = format!("{alias}.o");
            let object_path = out_dir.join(&object_name);

            // Pure sources keep the historical open-world `emit_object`
            // path (byte-identical). Sources that call host `#native`
            // fns lower closed-world: the host declarations resolve via
            // `AnalyzeOptions`, the gate bakes in, and the host Rust
            // bodies link + inline into the `.o`.
            let required_caps_mask = required_caps_mask(&entry.host_fns);
            let info = if entry.host_fns.is_empty() {
                LlvmAotEvaluator::emit_object(&src, &symbol, &object_path)
                    .map_err(|e| BuildError::Llvm(e, canonical_path.clone()))?
            } else {
                let options = build_native_options(&entry.host_fns);
                let shim = render_host_shim(&entry.host_fns);
                LlvmAotEvaluator::emit_object_with_options(
                    &src,
                    &symbol,
                    &object_path,
                    &options,
                    relon_codegen_llvm::WorldMode::ClosedWorld,
                    Some(&shim),
                )
                .map_err(|e| BuildError::Llvm(e, canonical_path.clone()))?
            };

            // Rebuild only when the source itself changed; the build
            // crate's own changes are tracked by cargo at the dep
            // level.
            println!(
                "cargo:rerun-if-changed={}",
                canonical_path.to_string_lossy()
            );

            // Linker stamp: pull the .o into the final binary. We use
            // `rustc-link-arg` (rather than splitting the .o into a
            // separate staticlib + `rustc-link-lib=static=`) because
            // `link-arg` keeps the per-object provenance — `nm` on
            // the final binary will still show the mangled symbol's
            // origin file. Linker also folds the .o through whole-
            // program LTO if the consuming crate enabled it.
            println!("cargo:rustc-link-arg={}", object_path.to_string_lossy());

            // Phase 2: when the emitted body references the host
            // `relon_llvm_str_contains_arena` shim, force the linker
            // to keep the matching symbol from `relon-rs-shims`. The
            // shim lives in an `.rlib` (Rust dep), and without an
            // explicit `--undefined` flag the linker may drop the
            // symbol before the AOT-`.o` reference resolves —
            // depending on the consuming binary's
            // `--gc-sections` / LTO posture. `-Wl,-u,<sym>` mirrors
            // what `extern "C" { fn <sym>(...) }` would do from a
            // Rust source — only force-keep when the symbol is
            // actually referenced so a no-string-contains build
            // doesn't pay the dead-shim cost.
            if info.references_str_contains_shim {
                println!("cargo:rustc-link-arg=-Wl,-u,relon_llvm_str_contains_arena");
            }

            objects.push(object_path);
            binding_modules.push(BindingModule {
                alias,
                symbol: info.entry_symbol,
                param_names: info.param_names,
                arity: info.entry_arity,
                shape: info.shape,
                main_fields: info.main_fields,
                return_fields: info.return_fields,
                main_root_size: info.main_root_size,
                return_root_size: info.return_root_size,
                return_has_tail: info.return_has_tail,
                const_data: info.const_data,
                source_path: canonical_path,
                aliased: entry.alias.is_some(),
                required_caps_mask,
            });
        }

        // Per-source binding files under `OUT_DIR/relon_rs/<alias>.rs`.
        // The macro side maps `include_relon!("src/foo.relon")` to
        // `include!(concat!(env!("OUT_DIR"), "/relon_rs/foo.rs"))` so
        // multiple sources can coexist without macro re-include
        // collisions.
        let bindings_dir = out_dir.join("relon_rs");
        std::fs::create_dir_all(&bindings_dir)
            .map_err(|e| BuildError::Io(e, bindings_dir.clone()))?;
        let mut bindings_paths = Vec::with_capacity(binding_modules.len());
        for m in &binding_modules {
            let path = bindings_dir.join(format!("{}.rs", m.alias));
            let src = render_one_module(m);
            std::fs::write(&path, src).map_err(|e| BuildError::Io(e, path.clone()))?;
            bindings_paths.push(path);
        }

        // Also emit a single aggregated file (`relon_rs_bindings.rs`)
        // listing every binding module, useful for downstream tooling
        // that wants a one-shot include. Optional — the macro side
        // does not depend on it.
        let agg_path = out_dir.join("relon_rs_bindings.rs");
        let agg_src = render_bindings(&binding_modules);
        std::fs::write(&agg_path, agg_src).map_err(|e| BuildError::Io(e, agg_path.clone()))?;

        Ok(EmitOutput {
            objects,
            bindings: bindings_paths,
        })
    }
}

/// Information collected about one source for binding-file emission.
#[derive(Debug)]
struct BindingModule {
    alias: String,
    symbol: String,
    param_names: Vec<String>,
    arity: usize,
    /// Which extern signature the emitted symbol carries — drives the
    /// binding's outer dispatch shape (typed `extern "C"` invocation
    /// for `FastInt`, marshalled `call_buffer_entry` for `Buffer`).
    shape: EmittedEntryShape,
    /// Declared `#main` parameters with byte-offsets + type tags, in
    /// declaration order. Empty for `FastInt` (the binding reads its
    /// args from positional registers).
    main_fields: Vec<EmittedField>,
    /// Return record fields. Always one entry under the Phase 2
    /// envelope (`#main` returns wrap in a single-field `Ret { value }`).
    /// Empty for `FastInt`.
    return_fields: Vec<EmittedField>,
    /// Fixed-area size of the input record. Zero for `FastInt`.
    main_root_size: u32,
    /// Fixed-area size of the return record. Zero for `FastInt`.
    return_root_size: u32,
    /// Whether the return schema includes pointer-indirect leaves.
    /// Drives the binding's tail-cap sizing.
    return_has_tail: bool,
    /// Const-pool blob the JIT body references through arena-relative
    /// i32 offsets. The binding ships this as a `const &[u8]` and
    /// hands it to `call_buffer_entry` on every dispatch. Empty for
    /// `FastInt`.
    const_data: Vec<u8>,
    source_path: PathBuf,
    /// `true` when the caller used `source_as` (or the macro form
    /// `include_relon!("foo.relon" as bar)`); the binding generator
    /// emits a flat `pub fn <alias>(...)` rather than a `mod`.
    aliased: bool,
    /// OR of `(1 << bit)` over every capability bit the source's host
    /// `#native` fns require. Non-zero means the source is gated: the
    /// buffer binding returns `Result<T, BufferEntryError>` so a runtime
    /// denial surfaces as a typed `CapabilityDenied`. Zero for a pure
    /// (open-world) or ungated source — the binding returns `T` directly
    /// (the historical shape, `.expect(...)` on the marshaller).
    required_caps_mask: i64,
}

fn file_stem_or_err(path: &Path) -> Result<String, BuildError> {
    let stem = path.file_stem().and_then(OsStr::to_str).ok_or_else(|| {
        BuildError::InvalidPath(path.to_path_buf(), "missing file stem".to_string())
    })?;
    Ok(stem.to_string())
}

/// Compute a stable 8-hex-char prefix mixing the alias + the source
/// text. Two sources with identical bodies but different aliases
/// hash distinctly; identical bodies + identical aliases collapse to
/// the same symbol (which only matters if the caller registers the
/// same source twice — caught by the `DuplicateAlias` check anyway).
fn mangled_symbol(alias: &str, src: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(alias.as_bytes());
    hasher.update(b"\0");
    hasher.update(src.as_bytes());
    let digest = hasher.finalize();
    let prefix = hex::encode(&digest[..4]);
    format!("__relon_{prefix}_main")
}

/// OR of `(1 << bit)` over every capability bit required by any of
/// `host_fns`' gates. Drives the binding's `Result` vs plain-`T`
/// return shape and is the bitmask the runtime gate tests against the
/// host's granted `SandboxState` mask.
fn required_caps_mask(host_fns: &[NativeHostFn]) -> i64 {
    let mut mask = 0i64;
    for f in host_fns {
        for bit in f.gate.required_bit_indices() {
            mask |= 1i64 << bit;
        }
    }
    mask
}

/// Build the [`relon_analyzer::AnalyzeOptions`] the closed-world emit
/// path threads so each host `#native` call resolves (name + signature),
/// its capability gate bakes into the IR, and the static
/// reachability check passes.
///
/// The granted `caps` here flips **every** required bit on: the static
/// check must not reject the source at build time, because the runtime
/// grant/deny decision rides the `caps` argument the host threads per
/// call. (The same `.o` serves both the authorised and unauthorised
/// runtime paths.)
fn build_native_options(host_fns: &[NativeHostFn]) -> relon_analyzer::AnalyzeOptions {
    use std::collections::{HashMap, HashSet};

    let mut names: HashSet<String> = HashSet::new();
    let mut signatures: HashMap<String, relon_analyzer::FnSignature> = HashMap::new();
    let mut gates: HashMap<String, relon_analyzer::NativeFnGate> = HashMap::new();

    for f in host_fns {
        names.insert(f.name.clone());
        let params = f
            .param_types
            .iter()
            .enumerate()
            .map(|(i, ty)| relon_analyzer::FnParam {
                name: format!("_{i}"),
                ty: relon_analyzer::type_node_simple(ty),
                optional: false,
            })
            .collect();
        signatures.insert(
            f.name.clone(),
            relon_analyzer::FnSignature {
                name: f.name.clone(),
                generics: Vec::new(),
                params,
                return_type: relon_analyzer::type_node_simple(&f.return_type),
                variadic_tail: None,
            },
        );
        gates.insert(f.name.clone(), f.gate.clone());
    }

    // Grant every required bit so the static check passes; runtime
    // grant/deny is the host's `caps` mask, not this build-time grant.
    let mut caps = relon_analyzer::Capabilities::default();
    for f in host_fns {
        caps.reads_fs |= f.gate.reads_fs;
        caps.writes_fs |= f.gate.writes_fs;
        caps.network |= f.gate.network;
        caps.reads_clock |= f.gate.reads_clock;
        caps.reads_env |= f.gate.reads_env;
        caps.uses_rng |= f.gate.uses_rng;
    }

    // Keep `strict_mode` at its `true` default and turn the single-file
    // capability-reachability check on, so the closed-world `#native`
    // `.o` seam aligns with the pure-source `emit_object` seam and the
    // in-process `from_source` path. Every required cap bit is granted
    // above, so the static check passes at build time; the runtime
    // grant/deny decision still rides the host's `caps` mask per call.
    // The frontend runs the shared `compile` with no diagnostic
    // suppression, so closure-as-value shapes must annotate their
    // parameters and return under strict (TypeScript-style), matching the
    // cranelift backend's accept/reject decision.
    relon_analyzer::AnalyzeOptions {
        host_fn_names: names,
        host_fn_signatures: signatures,
        host_fn_gates: gates,
        caps,
        standalone_capability_check: true,
        ..Default::default()
    }
}

/// Concatenate every host fn's `#[no_mangle] extern "C"` Rust body into
/// the single shim source the closed-world co-compile links + inlines.
fn render_host_shim(host_fns: &[NativeHostFn]) -> String {
    let mut out = String::new();
    out.push_str("// Auto-generated host shim for relon-rs closed-world `#native` link.\n");
    for f in host_fns {
        out.push_str(&f.rust_impl);
        if !f.rust_impl.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

fn render_bindings(modules: &[BindingModule]) -> String {
    let mut out = String::new();
    out.push_str("// Auto-generated by `relon-rs-build`. Do not edit.\n");
    out.push_str("// One module / function per registered `.relon` source.\n\n");
    for m in modules {
        render_one(&mut out, m);
    }
    out
}

/// Render a single source's binding file. Same shape as
/// [`render_bindings`] but for one module — the macro side picks
/// these files individually.
fn render_one_module(m: &BindingModule) -> String {
    let mut out = String::new();
    out.push_str("// Auto-generated by `relon-rs-build`. Do not edit.\n");
    render_one(&mut out, m);
    out
}

fn render_one(out: &mut String, m: &BindingModule) {
    // Source-path comment for human auditing — when a downstream
    // binary lists `__relon_*_main` in its symbol table the bindings
    // file gives a 1-step trace back to the originating `.relon`.
    out.push_str(&format!("// From: {}\n", m.source_path.to_string_lossy()));
    match m.shape {
        EmittedEntryShape::FastInt => render_one_fast_int(out, m),
        EmittedEntryShape::Buffer => render_one_buffer(out, m),
    }
}

/// Render the `FastInt` shape: an `extern "C" fn(i64...) -> i64`
/// declaration and a thin Rust wrapper. Same code path the Phase 1
/// trivial demo used.
fn render_one_fast_int(out: &mut String, m: &BindingModule) {
    // Param list for the Rust shim. Fast path is Int-only so every
    // arg is `i64`; we name each param after the corresponding
    // `#main` declaration so the consumer's call site stays readable.
    let rust_params: Vec<String> = m
        .param_names
        .iter()
        .map(|n| format!("{}: i64", sanitize_param(n)))
        .collect();
    // The extern declaration and the Rust shim share the same `i64`
    // param list, so render it once and reuse for both.
    let extern_args: Vec<String> = m.param_names.iter().map(|n| sanitize_param(n)).collect();

    debug_assert_eq!(rust_params.len(), m.arity);

    if m.aliased {
        out.push_str(&format!(
            "extern \"C\" {{\n    fn {sym}({eparams}) -> i64;\n}}\n",
            sym = m.symbol,
            eparams = rust_params.join(", "),
        ));
        out.push_str(&format!(
            "/// Safe shim for the AOT-compiled Relon `#main` defined in this source.\n\
             pub fn {fn_name}(_state: &::relon_rs_shims::SandboxState, {rparams}) -> i64 {{\n\
                 // SAFETY: the AOT body qualified for the fast-path entry — pure i64\n\
                 // arithmetic, no arena / shim dependencies. `_state` is threaded\n\
                 // through verbatim for forward-compat with the buffer-protocol path.\n\
                 unsafe {{ {sym}({eargs}) }}\n\
             }}\n\n",
            fn_name = m.alias,
            rparams = rust_params.join(", "),
            sym = m.symbol,
            eargs = extern_args.join(", "),
        ));
    } else {
        out.push_str(&format!(
            "pub mod {alias} {{\n\
                 use ::relon_rs_shims::SandboxState;\n\
                 extern \"C\" {{\n\
                     fn {sym}({eparams}) -> i64;\n\
                 }}\n\
                 /// Safe shim for the AOT-compiled Relon `#main` defined in this source.\n\
                 pub fn main(_state: &SandboxState, {rparams}) -> i64 {{\n\
                     unsafe {{ {sym}({eargs}) }}\n\
                 }}\n\
             }}\n\n",
            alias = m.alias,
            sym = m.symbol,
            eparams = rust_params.join(", "),
            rparams = rust_params.join(", "),
            eargs = extern_args.join(", "),
        ));
    }
}

/// Render the `Buffer` shape: the binding declares an extern with the
/// canonical buffer-protocol signature, embeds the const-pool blob +
/// per-field metadata as `const` data, and routes typed Rust args
/// through `relon_rs_shims::call_buffer_entry`.
fn render_one_buffer(out: &mut String, m: &BindingModule) {
    let sym = &m.symbol;
    let alias = &m.alias;

    // Stash references_str_contains_shim through a forced-keep
    // attribute on the binding side too — having the binding _name_
    // the shim function adds a Rust-level reference the linker can
    // see, defending against future build configs where the
    // cargo:rustc-link-arg approach gets stripped by section GC. We
    // emit a `use` to drag the symbol in; the underlying `#[no_mangle]`
    // exported function is named the same and lives in `relon-rs-shims`.

    // Render the per-arg signature + dispatch glue. Each `#main` param
    // declares the Rust-side type the user calls with, and the body
    // packs it into an `ArgValue` for the marshaller.
    let mut rust_params: Vec<String> = Vec::with_capacity(m.main_fields.len());
    let mut arg_value_exprs: Vec<String> = Vec::with_capacity(m.main_fields.len());
    for f in &m.main_fields {
        let pname = sanitize_param(&f.name);
        let map = rust_type_for(f.ty);
        rust_params.push(format!("{pname}: {}", map.arg_rust_ty));
        arg_value_exprs.push((map.arg_value_expr)(&pname));
    }

    // Return type: under Phase 2 the buffer wrapper always boxes the
    // return into a single-field `Ret { value: T }`. We surface T
    // directly to the caller. A `None` return (no return field) maps to
    // the same shape as an internal unit slot.
    let return_map = m
        .return_fields
        .first()
        .map(|f| rust_type_for(f.ty))
        .unwrap_or_else(|| rust_type_for(EmittedFieldType::Unit));
    let inner_ret_ty = return_map.ret_rust_ty;
    let ret_match_arm = return_map.ret_match_arm;

    // Gated sources (a host `#native` fn requires a capability) surface
    // their binding as `Result<T, BufferEntryError>` so a runtime
    // denial returns a typed `CapabilityDenied` instead of panicking.
    // Ungated / pure sources keep the historical plain-`T` shape
    // (the marshaller never errors on those, so `.expect(...)` is
    // unreachable in practice and keeps the call site ergonomic).
    let gated = m.required_caps_mask != 0;
    let rust_ret_ty = if gated {
        format!("::std::result::Result<{inner_ret_ty}, ::relon_rs_shims::BufferEntryError>")
    } else {
        inner_ret_ty.to_string()
    };

    // The marshaller-result handling differs by gate: gated bindings
    // `?`-propagate the error and wrap the decoded value in `Ok`;
    // ungated bindings `.expect(...)` (preserving the historical
    // never-errors contract).
    let (call_tail, decode) = if gated {
        (
            "?;".to_string(),
            format!(
                "match ret.pop().expect(\"return record empty\") {{\n\
                 {ret_arm},\n\
                 other => panic!(\"binding type mismatch on return: {{other:?}}\"),\n\
             }}\n",
                ret_arm = ok_wrap_match_arm(ret_match_arm),
            ),
        )
    } else {
        (
            ".expect(\"relon AOT body trapped\");".to_string(),
            format!(
                "match ret.pop().expect(\"return record empty\") {{\n\
                 {ret_arm},\n\
                 other => panic!(\"binding type mismatch on return: {{other:?}}\"),\n\
             }}\n",
                ret_arm = ret_match_arm,
            ),
        )
    };

    let main_fields_lit = render_field_slice(&m.main_fields);
    let return_fields_lit = render_field_slice(&m.return_fields);
    let const_data_lit = render_byte_slice(&m.const_data);

    // Common body — shared between the `aliased` (flat fn) and
    // `mod`-scoped emission paths.
    let body = format!(
        "    use ::relon_rs_shims::{{ArgValue, RetValue, EmittedField, EmittedFieldType, call_buffer_entry, BufferEntryFn}};\n\
         \n\
         extern \"C\" {{\n\
             fn {sym}(\n\
                 state: *const ::std::ffi::c_void,\n\
                 in_ptr: i32,\n\
                 in_len: i32,\n\
                 out_ptr: i32,\n\
                 out_cap: i32,\n\
                 caps: i64,\n\
             ) -> i32;\n\
         }}\n\
         \n\
         static MAIN_FIELDS: &[EmittedField] = &{main_fields};\n\
         static RETURN_FIELDS: &[EmittedField] = &{return_fields};\n\
         static CONST_DATA: &[u8] = &{const_data};\n\
         const MAIN_ROOT_SIZE: u32 = {main_root_size};\n\
         const RETURN_ROOT_SIZE: u32 = {return_root_size};\n\
         const RETURN_HAS_TAIL: bool = {return_has_tail};\n\
         \n\
         // SAFETY: the AOT-emitted body carries the canonical buffer-\n\
         // protocol signature; the cast erases the `ArenaState` pointer\n\
         // type (kept opaque on the binding side so the consuming crate\n\
         // doesn't take a direct dep on `relon-rs-shims`'s internal\n\
         // sandbox-state representation).\n\
         let entry_fn: BufferEntryFn = unsafe {{\n\
             ::std::mem::transmute({sym} as *const ())\n\
         }};\n\
         let args = [{arg_values}];\n\
         let mut ret = call_buffer_entry(\n\
             entry_fn,\n\
             CONST_DATA,\n\
             MAIN_FIELDS,\n\
             MAIN_ROOT_SIZE,\n\
             RETURN_FIELDS,\n\
             RETURN_ROOT_SIZE,\n\
             RETURN_HAS_TAIL,\n\
             _state,\n\
             &args,\n\
         ){call_tail}\n\
         {decode}",
        sym = sym,
        main_fields = main_fields_lit,
        return_fields = return_fields_lit,
        const_data = const_data_lit,
        main_root_size = m.main_root_size,
        return_root_size = m.return_root_size,
        return_has_tail = m.return_has_tail,
        arg_values = arg_value_exprs.join(", "),
        call_tail = call_tail,
        decode = decode,
    );

    if m.aliased {
        out.push_str(&format!(
            "/// Safe shim for the AOT-compiled Relon `#main` defined in this source.\n\
             pub fn {fn_name}(_state: &::relon_rs_shims::SandboxState, {rparams}) -> {ret_ty} {{\n\
             {body}\
             }}\n\n",
            fn_name = alias,
            rparams = rust_params.join(", "),
            ret_ty = rust_ret_ty,
            body = body,
        ));
    } else {
        out.push_str(&format!(
            "pub mod {alias} {{\n\
                 use ::relon_rs_shims::SandboxState;\n\
                 /// Safe shim for the AOT-compiled Relon `#main` defined in this source.\n\
                 pub fn main(_state: &SandboxState, {rparams}) -> {ret_ty} {{\n\
                 {body}\
                 }}\n\
             }}\n\n",
            alias = alias,
            rparams = rust_params.join(", "),
            ret_ty = rust_ret_ty,
            body = body,
        ));
    }
}

/// Render a `&[EmittedField]` constant initializer mirroring the
/// `EmittedField` definition in `relon-rs-shims`.
fn render_field_slice(fields: &[EmittedField]) -> String {
    let mut out = String::from("[\n");
    for f in fields {
        out.push_str(&format!(
            "        EmittedField {{ name: {name:?}, offset: {offset}, ty: {ty} }},\n",
            name = f.name,
            offset = f.offset,
            ty = rust_type_for(f.ty).tag_path,
        ));
    }
    out.push_str("    ]");
    out
}

/// Per-variant Rust-side projection of one [`EmittedFieldType`] tag —
/// the build-generator end of the three-crate marshalling triple.
///
/// Gathers everything the binding generator needs for a single leaf
/// type in one place: the `EmittedFieldType::*` literal path, the Rust
/// parameter / return surface types, and the `ArgValue` / `RetValue`
/// glue. See `relon_codegen_llvm::EmittedFieldType`'s docs for the
/// master triple contract.
struct RustTypeMap {
    /// `EmittedFieldType::*` literal path stamped into the generated
    /// `static MAIN_FIELDS` / `RETURN_FIELDS` slices.
    tag_path: &'static str,
    /// Rust type for a `#main` parameter of this leaf type.
    arg_rust_ty: &'static str,
    /// Builds the `ArgValue::*` constructor expression for a given
    /// (already-sanitised) parameter name.
    arg_value_expr: fn(&str) -> String,
    /// Rust type the `#main` return slot surfaces to the caller.
    ret_rust_ty: &'static str,
    /// `match` arm decoding the `RetValue::*` payload back to
    /// `ret_rust_ty`.
    ret_match_arm: &'static str,
}

/// Table mapping one [`EmittedFieldType`] tag to its Rust-side
/// projection. To widen the AOT signature surface (Float / List lanes),
/// add the matching arm here — the exhaustive `match` makes a new
/// codegen-llvm variant a compile error until this table is extended.
fn rust_type_for(ty: EmittedFieldType) -> RustTypeMap {
    match ty {
        EmittedFieldType::Int => RustTypeMap {
            tag_path: "EmittedFieldType::Int",
            arg_rust_ty: "i64",
            arg_value_expr: |p| format!("ArgValue::Int({p})"),
            ret_rust_ty: "i64",
            ret_match_arm: "RetValue::Int(v) => v",
        },
        EmittedFieldType::Float => RustTypeMap {
            tag_path: "EmittedFieldType::Float",
            arg_rust_ty: "f64",
            arg_value_expr: |p| format!("ArgValue::Float({p})"),
            ret_rust_ty: "f64",
            ret_match_arm: "RetValue::Float(v) => v",
        },
        EmittedFieldType::Bool => RustTypeMap {
            tag_path: "EmittedFieldType::Bool",
            arg_rust_ty: "bool",
            arg_value_expr: |p| format!("ArgValue::Bool({p})"),
            ret_rust_ty: "bool",
            ret_match_arm: "RetValue::Bool(v) => v",
        },
        EmittedFieldType::Unit => RustTypeMap {
            tag_path: "EmittedFieldType::Unit",
            arg_rust_ty: "()",
            arg_value_expr: |_p| "ArgValue::Unit".to_string(),
            ret_rust_ty: "()",
            ret_match_arm: "RetValue::Unit => ()",
        },
        EmittedFieldType::String => RustTypeMap {
            tag_path: "EmittedFieldType::String",
            arg_rust_ty: "&str",
            arg_value_expr: |p| format!("ArgValue::String({p})"),
            ret_rust_ty: "String",
            ret_match_arm: "RetValue::String(v) => v",
        },
        EmittedFieldType::ListInt => RustTypeMap {
            tag_path: "EmittedFieldType::ListInt",
            arg_rust_ty: "&[i64]",
            arg_value_expr: |p| format!("ArgValue::ListInt({p})"),
            ret_rust_ty: "Vec<i64>",
            ret_match_arm: "RetValue::ListInt(v) => v",
        },
        // ----- add new leaf type row above this line -----
    }
}

/// Render a `&[u8]` literal for the const-pool blob. Uses a hex-escape
/// form (`b"\\xAB..."`) so the resulting source stays compact for
/// large blobs while round-tripping through `rustc`'s string-literal
/// parser without UTF-8 validation issues.
fn render_byte_slice(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 4 + 16);
    out.push('[');
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&format!("0x{b:02x}"));
    }
    out.push(']');
    out
}

/// Wrap a `RetValue` match arm's value expression in `Ok(...)` for the
/// gated (`Result`-returning) binding. Splits the arm on the first
/// `=>`: the left half is the pattern, the right half the decoded
/// value, and the result is `<pattern> => ::std::result::Result::Ok(<value>)`.
/// The arms come from [`rust_type_for`]'s `ret_match_arm` table — each
/// is a single `Pattern => value` clause with no nested `=>`.
fn ok_wrap_match_arm(arm: &str) -> String {
    match arm.split_once("=>") {
        Some((pat, val)) => format!(
            "{} => ::std::result::Result::Ok({})",
            pat.trim(),
            val.trim()
        ),
        // Defensive: the table always contains `=>`, but if it ever
        // doesn't, fall back to wrapping the whole arm so the generated
        // source still compiles into a visible error rather than silently
        // mis-decoding.
        None => format!("{} => ::std::result::Result::Ok(())", arm.trim()),
    }
}

/// Sanitise a `#main` parameter name into a safe Rust identifier.
/// Phase 1 source files use simple ASCII identifiers (the parser
/// already enforces this) so the function is mostly a defensive
/// strip-out; we replace anything that wouldn't pass `is_valid_rust_ident`
/// with `_` to keep the generated code lex-valid.
fn sanitize_param(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for (i, c) in name.chars().enumerate() {
        let ok = if i == 0 {
            c.is_ascii_alphabetic() || c == '_'
        } else {
            c.is_ascii_alphanumeric() || c == '_'
        };
        out.push(if ok { c } else { '_' });
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}
