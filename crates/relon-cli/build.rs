//! Build script for `relon-cli`. The only job here is to opt the
//! `relon-cli` binary into the dynamic-relocation packing the linker
//! offers on Linux (`-z pack-relative-relocs`).
//!
//! Why this matters: the `release-cli` binary ships an optimised
//! whole-program ELF (LTO + cranelift + every Relon backend + LSP).
//! The loader's per-startup work scales linearly with the
//! `.rela.dyn` section entry count; every `R_X86_64_RELATIVE` reloc
//! the build emits has to be processed at load time before `main`
//! runs. On a current toolchain (rustc 1.92, GNU ld 2.42, glibc
//! 2.39) the unpacked table is roughly 350 KB / 14 k entries; packing
//! the same data into the `DT_RELR` form drops the section to about
//! 8 KB of `.relr.dyn` and shortens the kernel-side reloc-apply pass
//! enough to show up on the W11 fresh-process cold-start row (the
//! `cmp_lua` panel's LuaJIT-parity gate). The flag is per-target and
//! per-binary so debug builds keep whatever the host default linker
//! does. Platforms without `DT_RELR` loader support (older glibc,
//! non-glibc loaders, non-Linux) skip it entirely.
//!
//! No dependency on `mold`: the GNU `ld` shipping with binutils 2.40+
//! already understands `-z pack-relative-relocs`. Hosts that route
//! through LLD or mold get the same packing through the same flag.

fn main() {
    // Limit the flag to Linux glibc ELF targets. macOS' dyld and
    // Windows' loader don't ship the `DT_RELR` path; LLVM's
    // `link.exe` driver and musl loaders also reject the
    // `-z pack-relative-relocs` syntax.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os == "linux" && target_env == "gnu" {
        // `rustc-link-arg-bin` scopes the flag to the `relon-cli`
        // binary specifically. Keeps the dep crates' integration
        // tests (which sometimes statically link relon-cli via cargo
        // and build them with the same target spec) free of any
        // surprise linker rejection on a host whose `ld` lags 2.40.
        println!("cargo:rustc-link-arg-bin=relon-cli=-Wl,-z,pack-relative-relocs");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
