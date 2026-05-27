//! Build-script API for compiling Relon sources to relocatable object
//! files at build time.
//!
//! The crate is the bridge between a `.relon` source on disk and the
//! Rust binary that wants to call its `#main` as if it were a native
//! `extern "C"` function. The build.rs side calls into [`Compiler`];
//! the consuming source then `include_relon!`s the matching binding
//! file (`relon-rs-macro`) to import the typed Rust wrapper.
//!
//! ## Phase 1 envelope
//!
//! Only `#main(Int...) -> Int` shapes are accepted. Each `.relon`
//! source contributes:
//!
//! - One relocatable ELF object file under `out_dir`, exporting a
//!   single extern symbol `__relon_<hash>_main` with signature
//!   `extern "C" fn(i64, ...) -> i64`.
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

use relon_codegen_llvm::LlvmAotEvaluator;

/// Optimisation level requested from the LLVM backend. Phase 1 always
/// runs the same `-O3` middle-end pipeline regardless of this value
/// — the field is recorded for future use when the backend grows a
/// per-emit knob.
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
    opt_level: OptLevel,
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
        });
        self
    }

    /// Set the optimisation level. Phase 1 only accepts the default
    /// `-O3` pipeline; the setter exists so consumers can express
    /// intent today without breaking on the Phase 2 widening.
    pub fn opt_level(&mut self, _level: u32) -> &mut Self {
        // Phase 1 ignores the value (pipeline is hard-wired to -O3).
        // The signature accepts a generic level so callers can write
        // `.opt_level(3)` without worrying about the enum surface.
        self.opt_level = OptLevel::Aggressive;
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
            if !is_valid_rust_ident(&alias) {
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

            let info = LlvmAotEvaluator::emit_object(&src, &symbol, &object_path)
                .map_err(|e| BuildError::Llvm(e, canonical_path.clone()))?;

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

            objects.push(object_path);
            binding_modules.push(BindingModule {
                alias,
                symbol: info.entry_symbol,
                param_names: info.param_names,
                arity: info.entry_arity,
                source_path: canonical_path,
                aliased: entry.alias.is_some(),
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
    source_path: PathBuf,
    /// `true` when the caller used `source_as` (or the macro form
    /// `include_relon!("foo.relon" as bar)`); the binding generator
    /// emits a flat `pub fn <alias>(...)` rather than a `mod`.
    aliased: bool,
}

fn file_stem_or_err(path: &Path) -> Result<String, BuildError> {
    let stem = path.file_stem().and_then(OsStr::to_str).ok_or_else(|| {
        BuildError::InvalidPath(path.to_path_buf(), "missing file stem".to_string())
    })?;
    Ok(stem.to_string())
}

/// Lightweight check that a string is a valid Rust identifier
/// (`[A-Za-z_][A-Za-z0-9_]*`). Phase 1 doesn't reject Rust keywords —
/// the generated `mod foo { ... }` shape will surface the conflict at
/// compile time, which is loud enough for the trivial demo path.
fn is_valid_rust_ident(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
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

    // Param list for the Rust shim. Phase 1 is Int-only so every arg
    // is `i64`; we name each param after the corresponding `#main`
    // declaration so the consumer's call site stays readable.
    let rust_params: Vec<String> = m
        .param_names
        .iter()
        .map(|n| format!("{}: i64", sanitize_param(n)))
        .collect();
    let extern_params: Vec<String> = m
        .param_names
        .iter()
        .map(|n| format!("{}: i64", sanitize_param(n)))
        .collect();
    let extern_args: Vec<String> = m.param_names.iter().map(|n| sanitize_param(n)).collect();

    debug_assert_eq!(rust_params.len(), m.arity);

    if m.aliased {
        // Flat top-level function. The user supplied an explicit
        // alias via `.source_as` / `include_relon!("..." as bar)`.
        out.push_str(&format!(
            "extern \"C\" {{\n    fn {sym}({eparams}) -> i64;\n}}\n",
            sym = m.symbol,
            eparams = extern_params.join(", "),
        ));
        out.push_str(&format!(
            "/// Safe shim for the AOT-compiled Relon `#main` defined in this source.\n\
             pub fn {fn_name}(_state: &::relon_rs_shims::SandboxState, {rparams}) -> i64 {{\n\
                 // SAFETY: the AOT body is a leaf arithmetic function under the Phase 1\n\
                 // Int-only envelope; no arena / shim dependencies. `_state` is threaded\n\
                 // through for forward-compat with Phase 2 (pointer-indirect args).\n\
                 unsafe {{ {sym}({eargs}) }}\n\
             }}\n\n",
            fn_name = m.alias,
            rparams = rust_params.join(", "),
            sym = m.symbol,
            eargs = extern_args.join(", "),
        ));
    } else {
        // Module-scoped — `mod foo { fn main(...) }`. Matches the
        // default `include_relon!("src/foo.relon")` shape.
        out.push_str(&format!(
            "pub mod {alias} {{\n\
                 use ::relon_rs_shims::SandboxState;\n\
                 extern \"C\" {{\n\
                     fn {sym}({eparams}) -> i64;\n\
                 }}\n\
                 /// Safe shim for the AOT-compiled Relon `#main` defined in this source.\n\
                 pub fn main(_state: &SandboxState, {rparams}) -> i64 {{\n\
                     // SAFETY: see crate-level comment in `relon-rs-build`.\n\
                     unsafe {{ {sym}({eargs}) }}\n\
                 }}\n\
             }}\n\n",
            alias = m.alias,
            sym = m.symbol,
            eparams = extern_params.join(", "),
            rparams = rust_params.join(", "),
            eargs = extern_args.join(", "),
        ));
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
