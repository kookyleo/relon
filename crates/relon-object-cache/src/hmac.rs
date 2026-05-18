//! Per-installation HMAC key store and verification primitives.
//!
//! ## Threat model
//!
//! Cache files live in a user-writable directory. A local attacker
//! who can write there could otherwise drop a hand-crafted `.o`
//! that gets dlopen'd into the host process — game over. The
//! per-installation HMAC key, stored mode-`0600` outside the cache
//! directory, makes that attack require **also** reading the key
//! file. That bar matches the rest of the user's home directory.
//!
//! ## Key location
//!
//! 1. `$XDG_DATA_HOME/relon/cache-key` if `XDG_DATA_HOME` is set.
//! 2. `$HOME/.local/share/relon/cache-key` otherwise.
//! 3. If neither variable is set, the current directory is used as
//!    a last resort — the host should set `HOME` to avoid that.
//!
//! The file is 32 random bytes, no header, mode `0600`. We refuse
//! to load it with anything but the right size or mode.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use ::hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::error::HmacError;

type HmacSha256 = Hmac<Sha256>;

/// Length in bytes of the per-installation key.
pub const KEY_LEN: usize = 32;

/// Compute the canonical path of the HMAC key file. Made public so
/// hosts and tests can `set_var("XDG_DATA_HOME", ...)` and predict
/// where the resulting file will land.
pub fn hmac_key_path() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        let mut p = PathBuf::from(xdg);
        p.push("relon");
        p.push("cache-key");
        return p;
    }
    if let Some(home) = std::env::var_os("HOME") {
        let mut p = PathBuf::from(home);
        p.push(".local");
        p.push("share");
        p.push("relon");
        p.push("cache-key");
        return p;
    }
    PathBuf::from("relon-cache-key")
}

/// Load the HMAC key, generating one if it does not exist. The
/// returned key is always exactly [`KEY_LEN`] bytes; corrupted or
/// world-readable keys surface as [`HmacError`] so the caller can
/// regenerate after a clean-up.
pub fn ensure_key() -> Result<[u8; KEY_LEN], HmacError> {
    let path = hmac_key_path();

    if path.exists() {
        return load_key(&path);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut key = [0u8; KEY_LEN];
    getrandom::getrandom(&mut key).map_err(|e| HmacError::Random(e.to_string()))?;

    // Write with mode 0600 from the start so there is no narrow
    // window where the file is readable by others.
    #[cfg(unix)]
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    #[cfg(not(unix))]
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;

    f.write_all(&key)?;
    f.flush()?;
    drop(f);
    Ok(key)
}

/// Read an existing key file and validate its size + mode.
fn load_key(path: &std::path::Path) -> Result<[u8; KEY_LEN], HmacError> {
    let mut f = File::open(path)?;
    #[cfg(unix)]
    {
        let mode = f.metadata()?.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(HmacError::InsecureMode(mode));
        }
    }
    let mut buf = Vec::with_capacity(KEY_LEN);
    f.read_to_end(&mut buf)?;
    if buf.len() != KEY_LEN {
        return Err(HmacError::BadSize(buf.len()));
    }
    let mut key = [0u8; KEY_LEN];
    key.copy_from_slice(&buf);
    Ok(key)
}

/// Compute the HMAC-SHA256 tag over `bytes` using `key`. The
/// returned slice is exactly 32 bytes.
pub fn compute_hmac(bytes: &[u8], key: &[u8; KEY_LEN]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key)
        .expect("HMAC-SHA256 accepts keys of any length, including 32 bytes");
    mac.update(bytes);
    let out = mac.finalize().into_bytes();
    let mut tag = [0u8; 32];
    tag.copy_from_slice(&out);
    tag
}

/// Constant-time HMAC verification. Returns `true` iff
/// `expected == HMAC(key, bytes)`.
pub fn verify_hmac(bytes: &[u8], key: &[u8; KEY_LEN], expected: &[u8; 32]) -> bool {
    let mut mac = match HmacSha256::new_from_slice(key) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(bytes);
    mac.verify_slice(expected).is_ok()
}
