//! Default linker backend: spawn the system `ld` (or `cc`) and pipe
//! the `.o` through `-shared` to produce an `ET_DYN`.
//!
//! ## Why subprocess instead of in-process
//!
//! `lld-sys` / `mold-sys` are not on a stable release channel as of
//! v5-gamma planning, so wiring them in would either pin a fork or
//! commit to building lld from source in CI. Both are large costs
//! for a step that runs once per cold-start. A subprocess to the
//! system linker is what every other ahead-of-time toolchain (rustc,
//! clang, ghc) does and the IO cost is dwarfed by cranelift codegen.
//!
//! ## Tempfile rationale
//!
//! `ld` insists on a real path for `-o`, and most distros' linker
//! versions also refuse `/dev/stdin` for the input. We therefore
//! materialise both ends as `tempfile::NamedTempFile`; they live in
//! the OS temp dir (`tmpfs` on most Linux distros) so the IO never
//! hits a spinning disk. Both files are unlinked when the linker
//! returns regardless of success.
//!
//! ## Flag set
//!
//! - `-shared` — produce `ET_DYN` instead of `ET_EXEC`.
//! - `-z noexecstack` — match what gcc / clang inject; old binutils
//!   versions otherwise mark the output as needing an executable
//!   stack which trips `noexec`-mounted `tmpfs` partitions.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::elf_check::{parse_elf_type, ElfType};
use crate::error::LinkError;

/// Subprocess linker. Reusable: one [`SubprocLinker::new`] discovers
/// the binary once, subsequent [`SubprocLinker::link`] calls just
/// fork+exec it.
#[derive(Debug, Clone)]
pub struct SubprocLinker {
    /// Resolved path of the linker we will invoke. `/usr/bin/ld` is
    /// the default; environment variable `RELON_LD` overrides it.
    ld_path: PathBuf,
    /// Whether the resolved binary is `cc` / `gcc` / `clang` — these
    /// need `-nostdlib` so we do not accidentally pull libgcc into a
    /// pure-cranelift output.
    is_cc_frontend: bool,
    /// Extra flags appended after the defaults. Public-ish hook for
    /// future host integration (e.g. `-z now` or a custom `-rpath`).
    extra_flags: Vec<String>,
}

impl SubprocLinker {
    /// Resolve the linker binary. Order:
    ///
    /// 1. `RELON_LD` env var if set and the path exists.
    /// 2. `/usr/bin/ld` — every glibc / musl distro ships this.
    /// 3. `ld` on `$PATH`.
    /// 4. `cc` on `$PATH` (last resort, drives the platform `ld` via
    ///    `-shared`).
    ///
    /// Returns [`LinkError::LinkerNotFound`] if none of those work.
    pub fn new() -> Result<Self, LinkError> {
        if let Ok(custom) = std::env::var("RELON_LD") {
            let p = PathBuf::from(&custom);
            if p.is_file() {
                return Ok(Self {
                    is_cc_frontend: is_cc_like(&p),
                    ld_path: p,
                    extra_flags: Vec::new(),
                });
            }
        }
        let default = PathBuf::from("/usr/bin/ld");
        if default.is_file() {
            return Ok(Self {
                ld_path: default,
                is_cc_frontend: false,
                extra_flags: Vec::new(),
            });
        }
        if let Some(p) = which_on_path("ld") {
            return Ok(Self {
                is_cc_frontend: false,
                ld_path: p,
                extra_flags: Vec::new(),
            });
        }
        if let Some(p) = which_on_path("cc") {
            return Ok(Self {
                is_cc_frontend: true,
                ld_path: p,
                extra_flags: Vec::new(),
            });
        }
        Err(LinkError::LinkerNotFound)
    }

    /// Internal-only: construct a linker pointed at an arbitrary
    /// path. Lets tests exercise the `LinkerNotFound` / failure paths
    /// deterministically without mutating the process environment.
    #[doc(hidden)]
    pub fn from_path_for_tests(path: PathBuf) -> Self {
        Self {
            is_cc_frontend: is_cc_like(&path),
            ld_path: path,
            extra_flags: Vec::new(),
        }
    }

    /// Append arbitrary extra args after the default flag set. Useful
    /// for host integration tests that need to verify `--build-id`
    /// or similar policy flags propagate.
    pub fn with_extra_flag(mut self, flag: impl Into<String>) -> Self {
        self.extra_flags.push(flag.into());
        self
    }

    /// Resolved linker path. Mostly for diagnostics / logging.
    pub fn ld_path(&self) -> &Path {
        &self.ld_path
    }

    /// Drive the linker. Validates input is `ET_REL`, materialises
    /// it as a tempfile, runs `ld -shared`, validates output is
    /// `ET_DYN`, returns the linked bytes.
    pub fn link(&self, et_rel_bytes: &[u8], target_triple: &str) -> Result<Vec<u8>, LinkError> {
        // Fail fast before we pay for fork+exec.
        match parse_elf_type(et_rel_bytes)? {
            ElfType::Rel => {}
            other => return Err(LinkError::NotEtRel(other)),
        }
        // v5-gamma is x86_64-linux only; reject everything else early
        // so the operator sees a clear message instead of a confusing
        // linker stderr blob.
        if !is_supported_triple(target_triple) {
            return Err(LinkError::UnsupportedTriple(target_triple.to_owned()));
        }

        // Both tempfiles need stable paths; we close the handles
        // (via `into_temp_path`) so `ld` can mmap them without us
        // holding a competing fd open.
        let mut in_file = tempfile::Builder::new()
            .prefix("relon-link-in-")
            .suffix(".o")
            .tempfile()?;
        std::io::Write::write_all(&mut in_file, et_rel_bytes)?;
        std::io::Write::flush(&mut in_file)?;
        let in_path = in_file.into_temp_path();

        let out_file = tempfile::Builder::new()
            .prefix("relon-link-out-")
            .suffix(".so")
            .tempfile()?;
        // We only need the path; the file will be truncated +
        // rewritten by the linker. Drop the fd before invoking ld so
        // its `O_TRUNC` open does not race ours.
        let out_path = out_file.into_temp_path();

        let mut cmd = Command::new(&self.ld_path);
        if self.is_cc_frontend {
            // cc-driver mode: -nostdlib so we don't accidentally
            // link libc / libgcc into the output. `-fPIC` is implied
            // by `-shared` on every cc we've tested.
            cmd.args(["-shared", "-nostdlib", "-Wl,-z,noexecstack"]);
        } else {
            cmd.args(["-shared", "-z", "noexecstack"]);
        }
        for f in &self.extra_flags {
            cmd.arg(f);
        }
        cmd.arg("-o").arg(&out_path).arg(&in_path);

        let output = cmd.output().map_err(|e| match e.kind() {
            // Catches the race where the binary we resolved at
            // `new()` time was deleted before we could exec it.
            std::io::ErrorKind::NotFound => LinkError::LinkerNotFound,
            _ => LinkError::Io(e),
        })?;
        if !output.status.success() {
            // `from_utf8_lossy` borrows the child output (no allocation
            // unless the bytes contain invalid UTF-8); the owning
            // conversion is no longer needed since we only read it here.
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let mut msg = format!("{} exited {}", self.ld_path.display(), output.status);
            if !stderr.is_empty() {
                msg.push_str("\nstderr:\n");
                msg.push_str(stderr.trim_end());
            }
            if !stdout.is_empty() {
                msg.push_str("\nstdout:\n");
                msg.push_str(stdout.trim_end());
            }
            return Err(LinkError::LinkerFailed(msg));
        }

        let bytes = std::fs::read(&out_path)?;
        // Tempfiles unlink on drop; explicit drop here makes the
        // ordering obvious for a future reader.
        drop(in_path);
        drop(out_path);

        // Parse the header once: `is_et_dyn` would re-parse internally,
        // so match on the type directly and reuse the result for the
        // error diagnostic. A parse failure keeps the previous
        // `NotEtDyn(Other)` mapping rather than surfacing `InvalidElf`.
        match parse_elf_type(&bytes) {
            Ok(ElfType::Dyn) => {}
            Ok(other) => return Err(LinkError::NotEtDyn(other)),
            Err(_) => return Err(LinkError::NotEtDyn(ElfType::Other)),
        }
        Ok(bytes)
    }
}

/// Heuristic: is `path` a C compiler driver (`cc` / `gcc` / `clang`)
/// rather than a raw linker? We match on the file stem so symlinks
/// like `/usr/bin/cc -> gcc-13` still classify correctly.
fn is_cc_like(path: &Path) -> bool {
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    matches!(stem, "cc" | "gcc" | "clang") || stem.starts_with("gcc-") || stem.starts_with("clang-")
}

/// Hand-rolled `which`. Avoids pulling the `which` crate for ~10 lines.
fn which_on_path(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(binary);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Triple gating policy: only `x86_64-*-linux-*` is exercised in CI.
/// Everything else surfaces as [`LinkError::UnsupportedTriple`] so
/// downstream sees an actionable error rather than a cryptic `ld`
/// failure.
fn is_supported_triple(triple: &str) -> bool {
    let lower = triple.to_ascii_lowercase();
    lower.starts_with("x86_64-") && lower.contains("-linux")
}
