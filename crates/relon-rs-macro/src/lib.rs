//! `include_relon!` proc-macro — stitches the per-source bindings
//! emitted by `relon-rs-build` into the consuming Rust source.
//!
//! ## Forms
//!
//! The macro itself only emits an `include!(...)` of the bindings file
//! (see below); the `// effective form:` snippets show the *final*
//! shape once that included file is stitched in by `rustc`, not what
//! this macro literally produces.
//!
//! ```ignore
//! relon_rs_macro::include_relon!("src/foo.relon");
//! // effective form: pub mod foo { pub fn main(&SandboxState, i64) -> i64 { ... } }
//!
//! relon_rs_macro::include_relon!("src/foo.relon" as compute);
//! // effective form: pub fn compute(&SandboxState, i64) -> i64 { ... }
//! ```
//!
//! The param / return Rust types follow the source's `#main` signature
//! — the build.rs generator maps each accepted leaf type (`Int`,
//! `Float`, `Bool`, `Null`, `String`, `List<Int>`) onto its Rust
//! surface (`i64`, `f64`, `bool`, `()`, `&str` / `String`, `&[i64]` /
//! `Vec<i64>`). This macro is signature-agnostic: it only stitches in
//! the generated bindings file, whatever shape it carries.
//!
//! The macro resolves the path to a file stem (or honours the
//! supplied alias) and emits a single `include!(...)` whose argument
//! is `concat!(env!("OUT_DIR"), "/relon_rs/<alias>.rs")`. The
//! matching file is produced by `relon-rs-build::Compiler::emit_all`.
//!
//! The macro does **not** read the source file at compile time; it
//! only knows the file's path string. The build.rs side owns the
//! parse / analyze / emit chain and writes the bindings file before
//! `rustc` reaches this macro.

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{Ident, LitStr, Token};

/// Parsed form of one `include_relon!` invocation.
struct IncludeRelonInput {
    /// String literal naming the `.relon` source path (relative to
    /// the consuming crate's source root — same path the build.rs
    /// hands to `Compiler::source`).
    path: LitStr,
    /// Optional `as <alias>` clause overriding the default file-stem
    /// derived alias. Matches `Compiler::source_as` on the build
    /// side.
    alias: Option<Ident>,
}

impl Parse for IncludeRelonInput {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let path: LitStr = input.parse()?;
        let alias = if input.peek(Token![as]) {
            input.parse::<Token![as]>()?;
            Some(input.parse::<Ident>()?)
        } else {
            None
        };
        Ok(Self { path, alias })
    }
}

/// Stitch the build.rs-generated bindings for a `.relon` source into
/// the consuming Rust source. See module-level docs for the syntax.
#[proc_macro]
pub fn include_relon(input: TokenStream) -> TokenStream {
    let parsed = syn::parse_macro_input!(input as IncludeRelonInput);

    // Derive the alias the build.rs would have picked. The two halves
    // must agree because they share the bindings filename — if the
    // build.rs used `source_as("src/foo.relon", "bar")` then the
    // macro side must say `include_relon!("src/foo.relon" as bar)`.
    let alias = match &parsed.alias {
        Some(id) => id.to_string(),
        None => match derive_alias_from_path(&parsed.path.value()) {
            Ok(s) => s,
            Err(e) => {
                return syn::Error::new_spanned(&parsed.path, e)
                    .to_compile_error()
                    .into();
            }
        },
    };
    if !relon_util::is_valid_rust_ident(&alias) {
        return syn::Error::new(
            Span::call_site(),
            format!("`{alias}` is not a valid Rust identifier"),
        )
        .to_compile_error()
        .into();
    }

    // `include!(concat!(env!("OUT_DIR"), "/relon_rs/<alias>.rs"))`.
    // The `env!` resolves at rustc time so the bindings file is
    // located inside the consuming crate's OUT_DIR — exactly where
    // `Compiler::emit_all` wrote it. We carry the path as a string
    // literal so the macro stays hygienic w.r.t. path joining on
    // Windows etc. (cargo's OUT_DIR is always a forward-slash path
    // on every supported platform).
    let rel_path = format!("/relon_rs/{alias}.rs");

    let expanded = quote! {
        ::std::include!(::std::concat!(::std::env!("OUT_DIR"), #rel_path));
    };
    expanded.into()
}

fn derive_alias_from_path(path: &str) -> Result<String, String> {
    // `path` is a forward-slash-friendly string handed in by the
    // user. Strip directory components, then strip the `.relon`
    // extension. We don't go through `std::path::Path` because cargo
    // workflows treat the macro argument as a literal string and
    // shouldn't depend on host path-separator semantics.
    let basename = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let stem = basename.strip_suffix(".relon").unwrap_or(basename);
    if stem.is_empty() {
        return Err(format!("could not derive identifier from `{path}`"));
    }
    Ok(stem.to_string())
}
