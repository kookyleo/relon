//! `memfd_create` + `/proc/self/fd/<n>` dlopen path.
//!
//! ## Linux flow
//!
//! 1. `memfd_create("relon-native-<nonce>", MFD_CLOEXEC)` — anonymous
//!    file backed by tmpfs; never visible on disk.
//! 2. `write(2)` the cranelift-object bytes.
//! 3. `dlopen("/proc/self/fd/<n>", RTLD_NOW | RTLD_LOCAL)` — glibc /
//!    musl both honour the procfs path and resolve relocations from
//!    the in-kernel fd directly.
//! 4. `dlsym` each expected symbol; the caller hands those pointers
//!    to whatever calling convention the codegen agreed on.
//!
//! On `dlclose`, the dynamic linker drops its reference and the
//! kernel reclaims the memfd. We `close(2)` the fd ourselves once
//! `dlopen` has returned because the linker keeps its own dup'd
//! handle inside the loaded namespace.
//!
//! ## Non-Linux
//!
//! macOS' `NSCreateObjectFileImageFromMemory` is deprecated and
//! Windows wants a different format entirely; both surface as
//! [`LoaderError::UnsupportedPlatform`]. Gamma phase is Linux-only;
//! later phases will add the fallbacks.

use std::collections::HashMap;
use std::ffi::{c_void, CString};

use crate::error::LoaderError;

#[cfg(target_os = "linux")]
use std::os::unix::io::RawFd;
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicU64, Ordering};

/// Owns the OS-level resources backing a loaded object: the memfd
/// (kept alive for the lifetime of the dlopen handle so /proc/self/fd
/// stays valid against late-binding lazy symbols) and the dlopen
/// handle itself.
///
/// On drop, `dlclose` runs first, *then* the fd is closed — the
/// linker keeps its own dup but releasing it before dlclose triggers
/// undefined behaviour with some musl versions.
pub struct ObjectHandle {
    #[cfg(target_os = "linux")]
    fd: RawFd,
    #[cfg(target_os = "linux")]
    dl_handle: *mut c_void,
    /// Recorded so `Drop` does not panic if we are dropped through
    /// an early-return path that never populated the handle.
    #[cfg(target_os = "linux")]
    is_open: bool,
    #[cfg(not(target_os = "linux"))]
    _marker: std::marker::PhantomData<*const c_void>,
}

// SAFETY: `dl_handle` and `fd` are kernel-owned resources that are
// safe to share between threads — `dlsym` itself is thread-safe per
// POSIX. The handle is only mutated on drop, which the Rust ownership
// model already serialises.
unsafe impl Send for ObjectHandle {}
unsafe impl Sync for ObjectHandle {}

impl std::fmt::Debug for ObjectHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("ObjectHandle");
        #[cfg(target_os = "linux")]
        {
            d.field("fd", &self.fd);
            d.field("dl_handle", &(self.dl_handle as usize));
            d.field("is_open", &self.is_open);
        }
        d.finish()
    }
}

impl Drop for ObjectHandle {
    #[cfg(target_os = "linux")]
    fn drop(&mut self) {
        if !self.is_open {
            return;
        }
        // SAFETY: We hold the only reference to `dl_handle` and `fd`.
        // `dlclose` and `close` are safe to call on valid handles
        // owned by this struct.
        unsafe {
            if !self.dl_handle.is_null() {
                libc::dlclose(self.dl_handle);
            }
            if self.fd >= 0 {
                libc::close(self.fd);
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    fn drop(&mut self) {}
}

/// Resolved view of a loaded object: an [`ObjectHandle`] keeping
/// the linker mapping alive plus a name → fn-pointer table.
pub struct LoadedObject {
    fn_pointers: HashMap<String, *const u8>,
    _handle: ObjectHandle,
}

// SAFETY: same justification as `ObjectHandle` — the raw pointers
// are valid for the lifetime of the handle and `dlsym`'d functions
// are designed to be callable from any thread.
unsafe impl Send for LoadedObject {}
unsafe impl Sync for LoadedObject {}

impl std::fmt::Debug for LoadedObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedObject")
            .field("symbol_count", &self.fn_pointers.len())
            .finish()
    }
}

impl LoadedObject {
    /// Linux implementation — `memfd_create` + procfs dlopen.
    #[cfg(target_os = "linux")]
    pub fn from_bytes(
        object_bytes: &[u8],
        _target_triple: &str,
        expected_symbols: &[&str],
    ) -> Result<Self, LoaderError> {
        // Pick a per-call unique name so log inspection of /proc can
        // tell concurrent evaluators apart. The kernel allows up to
        // 249 bytes; we stay well under.
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let nonce = SEQ.fetch_add(1, Ordering::Relaxed);
        let name = CString::new(format!("relon-native-{}", nonce))
            .expect("nonce-derived name never contains NUL");

        // SAFETY: `memfd_create` is a plain syscall wrapper. We pass
        // a valid C string and a well-known flag constant; the
        // returned fd is owned by us.
        let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(LoaderError::Memfd(std::io::Error::last_os_error()));
        }

        // Helper to close `fd` on every early return path so we do
        // not leak descriptors when the loader bails before we have
        // a real `ObjectHandle` to drop.
        struct FdGuard {
            fd: RawFd,
            armed: bool,
        }
        impl Drop for FdGuard {
            fn drop(&mut self) {
                if self.armed && self.fd >= 0 {
                    // SAFETY: fd was returned by memfd_create above
                    // and is still owned by us at this point.
                    unsafe {
                        libc::close(self.fd);
                    }
                }
            }
        }
        let mut guard = FdGuard { fd, armed: true };

        // Write the object bytes in a loop so partial writes (rare
        // on memfd but legal) do not silently truncate.
        let mut written = 0usize;
        while written < object_bytes.len() {
            // SAFETY: slice pointer + length come from a live slice;
            // `fd` is the freshly-allocated memfd.
            let n = unsafe {
                libc::write(
                    fd,
                    object_bytes.as_ptr().add(written) as *const c_void,
                    object_bytes.len() - written,
                )
            };
            if n < 0 {
                return Err(LoaderError::Write(std::io::Error::last_os_error()));
            }
            if n == 0 {
                return Err(LoaderError::Write(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "memfd write returned 0",
                )));
            }
            written += n as usize;
        }

        let proc_path =
            CString::new(format!("/proc/self/fd/{}", fd)).expect("decimal fd path contains no NUL");
        // SAFETY: procfs path is null-terminated and the fd is still
        // open in the current process. Flags are well-defined libc
        // constants.
        let dl_handle =
            unsafe { libc::dlopen(proc_path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if dl_handle.is_null() {
            // SAFETY: dlerror() returns a static thread-local string
            // pointer that is valid until the next dl* call.
            let msg = unsafe {
                let err = libc::dlerror();
                if err.is_null() {
                    "dlopen returned null with no dlerror".to_owned()
                } else {
                    std::ffi::CStr::from_ptr(err).to_string_lossy().into_owned()
                }
            };
            return Err(LoaderError::Dlopen(msg));
        }

        // Resolve every requested symbol up front. Return immediately
        // upon the first missing symbol so the diagnostic names the
        // first failure rather than continuing to bind later ones.
        let mut fn_pointers = HashMap::with_capacity(expected_symbols.len());
        for &sym in expected_symbols {
            let cname = CString::new(sym)
                .map_err(|_| LoaderError::SymbolNotFound(format!("{sym} (contains NUL)")))?;
            // Clear any prior dlerror so we can distinguish "symbol
            // resolved to NULL" from "lookup failed". POSIX requires
            // both cases to return null, so dlerror is the only
            // signal. See `dlsym(3)`.
            // SAFETY: dlerror() is thread-safe in glibc / musl.
            unsafe { libc::dlerror() };
            // SAFETY: `dl_handle` is non-null; `cname` is a valid C
            // string; `dlsym` is defined for any valid handle.
            let ptr = unsafe { libc::dlsym(dl_handle, cname.as_ptr()) };
            if ptr.is_null() {
                // Distinguish missing-symbol vs intentional-NULL by
                // peeking dlerror.
                // SAFETY: dlerror() returns a thread-local pointer.
                let had_err = unsafe { !libc::dlerror().is_null() };
                if had_err {
                    // SAFETY: dlclose handle we just opened.
                    unsafe {
                        libc::dlclose(dl_handle);
                    }
                    return Err(LoaderError::SymbolNotFound(sym.to_owned()));
                }
            }
            fn_pointers.insert(sym.to_owned(), ptr as *const u8);
        }

        guard.armed = false; // hand fd ownership to ObjectHandle.

        Ok(LoadedObject {
            fn_pointers,
            _handle: ObjectHandle {
                fd,
                dl_handle,
                is_open: true,
            },
        })
    }

    /// Non-Linux stub — return a clean error so the host can pick
    /// the JIT fallback without touching the rest of the loader.
    #[cfg(not(target_os = "linux"))]
    pub fn from_bytes(
        _object_bytes: &[u8],
        _target_triple: &str,
        _expected_symbols: &[&str],
    ) -> Result<Self, LoaderError> {
        Err(LoaderError::UnsupportedPlatform)
    }

    /// Look up one of the symbols requested at load time. Returns
    /// `None` if the caller asks for a name that was not in the
    /// original `expected_symbols` slice.
    pub fn resolve(&self, name: &str) -> Option<*const u8> {
        self.fn_pointers.get(name).copied()
    }

    /// Iterate over `(name, ptr)` pairs in insertion order. Useful
    /// for debugging / dumping the resolved symbol table.
    pub fn iter_symbols(&self) -> impl Iterator<Item = (&str, *const u8)> {
        self.fn_pointers.iter().map(|(k, v)| (k.as_str(), *v))
    }
}
