//! Leaf utility helpers shared across Relon crates.
//!
//! This crate is deliberately a **leaf**: it depends on no other
//! `relon-*` crate. It collects tiny, byte-identical helpers that were
//! previously copy-pasted across the codegen / ABI / build crates so a
//! single definition is shared (and unit-tested) in one place.

/// Round `value` up to the next multiple of `align`. `align` is
/// expected to be a power of two.
pub fn align_up(value: u32, align: u32) -> u32 {
    let rem = value % align;
    if rem == 0 {
        value
    } else {
        value + (align - rem)
    }
}

/// Lightweight check that a string is a valid Rust identifier
/// (`[A-Za-z_][A-Za-z0-9_]*`). This does not reject Rust keywords —
/// callers that splice the string into generated `mod foo { ... }` /
/// `fn foo` shapes will surface a keyword conflict at compile time,
/// which is loud enough for the trivial demo paths that use it.
pub fn is_valid_rust_ident(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Filesystem sandbox root shared by every executor (tree-walk,
/// cranelift native, llvm native).
///
/// P-fs Stage 1's `read_file(path)` primitive resolves its `path`
/// argument relative to a single process-global root and refuses any
/// path that escapes it. The default root is the process current
/// working directory; hosts override it with [`set_fs_sandbox_root`].
///
/// This is the native-side analogue of the wasm preopened directory:
/// the wasm backend lowers `read_file` to the standard preview1 fd
/// protocol against a preopened dir, and the host preopens the same
/// directory it installs here, so all four executors resolve a relative
/// path against the same root. Keeping the root in one leaf crate gives
/// every backend a single source of truth without a dependency cycle.
mod fs_sandbox {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};

    // Thread-scoped, not process-global: `read_file`'s native helper runs
    // synchronously on the evaluation thread (the same thread that called
    // `set_fs_sandbox_root`), so a thread-local root gives each concurrent
    // evaluation an independent sandbox root instead of one process-wide
    // root that concurrent evaluations would clobber. (The wasm backend
    // does not consult this — it resolves against the WASI host's preopened
    // dir.) Caveat: set the root on the same thread that evaluates; setting
    // it on one thread and evaluating on another sees the default root.
    thread_local! {
        static FS_SANDBOX_ROOT: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    }

    /// Set the filesystem sandbox root used by `read_file` **on the current
    /// thread**. All relative paths resolve against this directory and any
    /// path escaping it is refused. Pass the same directory the wasm host
    /// preopens so the four executors agree. Thread-scoped: set it on the
    /// thread that runs the evaluation.
    pub fn set_fs_sandbox_root(root: impl Into<PathBuf>) {
        FS_SANDBOX_ROOT.with(|r| *r.borrow_mut() = Some(root.into()));
    }

    /// Current sandbox root: the thread's configured root, or the process
    /// current working directory when unset.
    fn current_root() -> PathBuf {
        if let Some(root) = FS_SANDBOX_ROOT.with(|r| r.borrow().clone()) {
            return root;
        }
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    }

    /// Resolve `path` against the sandbox root, refusing absolute paths
    /// and any path that escapes the root (`../`, symlinks). Returns the
    /// absolute, root-contained path to read.
    ///
    /// The escape check canonicalizes the root and the joined target so
    /// symlink + `..` escapes are caught the same way (mirrors
    /// `FilesystemModuleResolver`). When the file does not exist yet the
    /// lexical-only containment check still applies, so a non-existent
    /// `../escape.txt` is refused before the read is attempted.
    pub fn resolve_fs_sandbox_path(path: &str) -> Result<PathBuf, String> {
        let root = current_root();
        let candidate = Path::new(path);
        if candidate.is_absolute() {
            return Err(format!(
                "read_file: absolute path {path:?} is outside the filesystem sandbox root"
            ));
        }
        let joined = root.join(candidate);

        // Prefer canonicalization (catches symlink + `..` escapes). When
        // the target does not exist, fall back to a lexical check that
        // still rejects any component that climbs above the root.
        let canonical_root = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
        match std::fs::canonicalize(&joined) {
            Ok(canon) => {
                if !canon.starts_with(&canonical_root) {
                    return Err(format!(
                        "read_file: path {path:?} escapes the filesystem sandbox root"
                    ));
                }
                Ok(canon)
            }
            Err(_) => {
                if lexically_escapes(candidate) {
                    return Err(format!(
                        "read_file: path {path:?} escapes the filesystem sandbox root"
                    ));
                }
                Ok(joined)
            }
        }
    }

    /// Lexical `..`-escape check: returns true when walking the path's
    /// normal/parent components ever drops below the root.
    fn lexically_escapes(path: &Path) -> bool {
        use std::path::Component;
        let mut depth: i32 = 0;
        for comp in path.components() {
            match comp {
                Component::ParentDir => {
                    depth -= 1;
                    if depth < 0 {
                        return true;
                    }
                }
                Component::Normal(_) => depth += 1,
                Component::CurDir => {}
                // RootDir / Prefix can't appear in a relative path we
                // already rejected as absolute; treat as escape.
                _ => return true,
            }
        }
        false
    }
}

pub use fs_sandbox::{resolve_fs_sandbox_path, set_fs_sandbox_root};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align_up_already_aligned_is_identity() {
        assert_eq!(align_up(0, 8), 0);
        assert_eq!(align_up(8, 8), 8);
        assert_eq!(align_up(16, 8), 16);
        assert_eq!(align_up(64, 8), 64);
    }

    #[test]
    fn align_up_rounds_up_to_next_multiple() {
        assert_eq!(align_up(1, 8), 8);
        assert_eq!(align_up(7, 8), 8);
        assert_eq!(align_up(9, 8), 16);
        assert_eq!(align_up(15, 8), 16);
        assert_eq!(align_up(17, 8), 24);
    }

    #[test]
    fn align_up_align_one_is_identity() {
        assert_eq!(align_up(0, 1), 0);
        assert_eq!(align_up(1, 1), 1);
        assert_eq!(align_up(42, 1), 42);
        assert_eq!(align_up(u32::MAX, 1), u32::MAX);
    }

    #[test]
    fn align_up_non_power_of_two_align() {
        // The function does not require a power-of-two `align`; it
        // simply rounds to the next multiple.
        assert_eq!(align_up(10, 4), 12);
        assert_eq!(align_up(12, 4), 12);
        assert_eq!(align_up(5, 3), 6);
    }

    #[test]
    fn is_valid_rust_ident_accepts_normal_idents() {
        assert!(is_valid_rust_ident("foo"));
        assert!(is_valid_rust_ident("Foo"));
        assert!(is_valid_rust_ident("_foo"));
        assert!(is_valid_rust_ident("_"));
        assert!(is_valid_rust_ident("foo_bar_42"));
        assert!(is_valid_rust_ident("a1"));
        assert!(is_valid_rust_ident("X"));
        // Keyword-ish strings still pass the lexical check — keyword
        // rejection is intentionally left to the Rust compiler.
        assert!(is_valid_rust_ident("fn"));
        assert!(is_valid_rust_ident("match"));
    }

    #[test]
    fn is_valid_rust_ident_rejects_invalid() {
        assert!(!is_valid_rust_ident(""));
        assert!(!is_valid_rust_ident("1foo"));
        assert!(!is_valid_rust_ident("9"));
        assert!(!is_valid_rust_ident("foo-bar"));
        assert!(!is_valid_rust_ident("foo bar"));
        assert!(!is_valid_rust_ident("foo.bar"));
        assert!(!is_valid_rust_ident("foo!"));
        // Non-ASCII is rejected (ASCII-only by design). Built from
        // `char` escapes so the source file stays pure ASCII.
        let accented: String = ['c', 'a', 'f', '\u{e9}'].iter().collect(); // "cafe" + e-acute
        assert!(!is_valid_rust_ident(&accented));
        let cyrillic: String = ['\u{43f}', '\u{440}'].iter().collect(); // Cyrillic "pr"
        assert!(!is_valid_rust_ident(&cyrillic));
        let cjk: String = ['\u{540d}', '\u{524d}'].iter().collect(); // CJK ideographs
        assert!(!is_valid_rust_ident(&cjk));
        let n_tilde: String = ['\u{f1}'].iter().collect(); // n-tilde
        assert!(!is_valid_rust_ident(&n_tilde));
    }
}
