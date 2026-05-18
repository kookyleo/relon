//! Module cache for the cranelift-native AOT backend.
//!
//! v5-beta-1 keeps the cache deliberately simple: we serialize the
//! IR module itself plus the sandbox-config flags. `from_cache`
//! re-runs the cranelift codegen pipeline against the cached IR. The
//! benefit over `from_source` is that we skip the parse + analyze +
//! lower passes; benchmarks in `docs/internal/wasm-bench-report-...`
//! consistently show those passes dominate cold-start for simple
//! programs.
//!
//! v5-gamma will swap the cache format to use `cranelift-object`'s
//! relocatable `.o` shape so the JIT step is skipped too — that
//! requires the host to supply a relocation table though, and the
//! tradeoff isn't obviously worth the extra plumbing for the
//! HelloWorld scenarios.
//!
//! On-disk format (little-endian, `repr(C)` semantically):
//!
//! ```text
//! magic           : [u8; 4]  = "RLNC"
//! format_version  : u32      = 1
//! sandbox_flags   : u32      // bit-packed SandboxConfig
//! ir_bincode_len  : u32      // length of the serialised IR
//! ir_bincode      : [u8; N]  // bincode encoding of `relon_ir::ir::Module`
//! sha256          : [u8; 32] // digest of bytes 0..(8 + 4 + 4 + N)
//! ```

use sha2::{Digest, Sha256};

use crate::error::CraneliftError;
use crate::sandbox::SandboxConfig;

/// Magic prefix for an on-disk cache blob (Relon-native-cache).
pub const CACHE_MAGIC: [u8; 4] = *b"RLNC";

/// Cache format version. Bumped on any layout change so old caches
/// produced by an earlier toolchain refuse-to-load with a clean
/// diagnostic.
pub const CACHE_FORMAT_VERSION: u32 = 1;

/// Bit-pack a `SandboxConfig` into a `u32` for storage.
fn pack_sandbox(cfg: &SandboxConfig) -> u32 {
    (cfg.bounds_check as u32)
        | ((cfg.deadline_check as u32) << 1)
        | ((cfg.capability_check as u32) << 2)
        | ((cfg.div_check as u32) << 3)
}

/// Inverse of [`pack_sandbox`].
fn unpack_sandbox(bits: u32) -> SandboxConfig {
    SandboxConfig {
        bounds_check: (bits & 0b0001) != 0,
        deadline_check: (bits & 0b0010) != 0,
        capability_check: (bits & 0b0100) != 0,
        div_check: (bits & 0b1000) != 0,
    }
}

/// Container holding the bits a `from_cache` constructor needs to
/// reconstruct a `CraneliftAotEvaluator` without re-running parse /
/// analyze / lower.
#[derive(Debug)]
pub struct CacheEntry {
    /// IR module the cache was built from. The codegen pipeline runs
    /// against this on every reload.
    pub ir: relon_ir::ir::Module,
    /// Sandbox config snapshot. Restored verbatim so cached and
    /// freshly-built evaluators behave identically.
    pub sandbox: SandboxConfig,
}

/// Encode a [`CacheEntry`] into the on-disk byte form.
pub fn serialize(entry: &CacheEntry) -> Result<Vec<u8>, CraneliftError> {
    // We use `serde_json` here rather than `bincode` — the IR module
    // is small (< 10 KB for HelloWorld scenarios) and the workspace
    // already pulls `serde_json` in, so we avoid adding a new dep.
    // v5-gamma swaps to bincode once a benchmark proves the savings
    // matter.
    let ir_blob = serde_json::to_vec(&IrSerde::from_ir(&entry.ir))
        .map_err(|e| CraneliftError::Cache(format!("serialize ir: {e}")))?;
    let mut out = Vec::with_capacity(ir_blob.len() + 64);
    out.extend_from_slice(&CACHE_MAGIC);
    out.extend_from_slice(&CACHE_FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&pack_sandbox(&entry.sandbox).to_le_bytes());
    let len_u32 = u32::try_from(ir_blob.len())
        .map_err(|_| CraneliftError::Cache("ir blob too large for u32 length".into()))?;
    out.extend_from_slice(&len_u32.to_le_bytes());
    out.extend_from_slice(&ir_blob);
    let digest = Sha256::digest(&out);
    out.extend_from_slice(digest.as_slice());
    Ok(out)
}

/// Decode the on-disk byte form back into a [`CacheEntry`].
pub fn deserialize(bytes: &[u8]) -> Result<CacheEntry, CraneliftError> {
    if bytes.len() < 4 + 4 + 4 + 4 + 32 {
        return Err(CraneliftError::Cache("cache blob too short".into()));
    }
    if bytes[..4] != CACHE_MAGIC {
        return Err(CraneliftError::Cache("magic mismatch".into()));
    }
    let format_version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    if format_version != CACHE_FORMAT_VERSION {
        return Err(CraneliftError::Cache(format!(
            "format version mismatch: expected {CACHE_FORMAT_VERSION}, got {format_version}"
        )));
    }
    let sandbox_bits = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    let ir_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    let ir_end = 16 + ir_len;
    if bytes.len() < ir_end + 32 {
        return Err(CraneliftError::Cache("cache blob truncated".into()));
    }
    let ir_blob = &bytes[16..ir_end];
    let stored_digest = &bytes[ir_end..ir_end + 32];
    let computed_digest = Sha256::digest(&bytes[..ir_end]);
    if computed_digest.as_slice() != stored_digest {
        return Err(CraneliftError::Cache("sha256 digest mismatch".into()));
    }

    let ir_serde: IrSerde = serde_json::from_slice(ir_blob)
        .map_err(|e| CraneliftError::Cache(format!("deserialize ir: {e}")))?;

    Ok(CacheEntry {
        ir: ir_serde.into_ir()?,
        sandbox: unpack_sandbox(sandbox_bits),
    })
}

/// Serde-shaped mirror of the IR module's narrow v5-beta-1 envelope
/// (Int params, Int return, arithmetic body). Keeping this struct
/// in-crate lets us serialize without forcing `relon-ir` to grow a
/// `serde` dependency just for the cache plumbing.
#[derive(serde::Serialize, serde::Deserialize)]
struct IrSerde {
    funcs: Vec<FuncSerde>,
    entry_func_index: Option<usize>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct FuncSerde {
    name: String,
    params: Vec<IrTySerde>,
    ret: IrTySerde,
    body: Vec<OpSerde>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy)]
enum IrTySerde {
    I32,
    I64,
    F64,
    Bool,
    Null,
    String,
    ListInt,
    Closure,
}

impl IrTySerde {
    fn to_ir(self) -> relon_ir::ir::IrType {
        match self {
            IrTySerde::I32 => relon_ir::ir::IrType::I32,
            IrTySerde::I64 => relon_ir::ir::IrType::I64,
            IrTySerde::F64 => relon_ir::ir::IrType::F64,
            IrTySerde::Bool => relon_ir::ir::IrType::Bool,
            IrTySerde::Null => relon_ir::ir::IrType::Null,
            IrTySerde::String => relon_ir::ir::IrType::String,
            IrTySerde::ListInt => relon_ir::ir::IrType::ListInt,
            IrTySerde::Closure => relon_ir::ir::IrType::Closure,
        }
    }

    fn from_ir(ty: relon_ir::ir::IrType) -> Result<Self, CraneliftError> {
        Ok(match ty {
            relon_ir::ir::IrType::I32 => IrTySerde::I32,
            relon_ir::ir::IrType::I64 => IrTySerde::I64,
            relon_ir::ir::IrType::F64 => IrTySerde::F64,
            relon_ir::ir::IrType::Bool => IrTySerde::Bool,
            relon_ir::ir::IrType::Null => IrTySerde::Null,
            relon_ir::ir::IrType::String => IrTySerde::String,
            relon_ir::ir::IrType::ListInt => IrTySerde::ListInt,
            relon_ir::ir::IrType::Closure => IrTySerde::Closure,
            other => {
                return Err(CraneliftError::Cache(format!(
                    "ir type {:?} not supported by v5-beta-1 cache",
                    other
                )))
            }
        })
    }
}

/// Op envelope mirroring the subset v5-beta-1 ever populates into IR.
#[derive(serde::Serialize, serde::Deserialize)]
enum OpSerde {
    ConstI64(i64),
    ConstI32(i32),
    ConstBool(bool),
    LocalGet(u32),
    Add(IrTySerde),
    Sub(IrTySerde),
    Mul(IrTySerde),
    Div(IrTySerde),
    Mod(IrTySerde),
    Eq(IrTySerde),
    Ne(IrTySerde),
    Lt(IrTySerde),
    Le(IrTySerde),
    Gt(IrTySerde),
    Ge(IrTySerde),
    Return,
    CheckCap { cap_bit: u32 },
}

impl IrSerde {
    fn from_ir(ir: &relon_ir::ir::Module) -> Self {
        Self {
            funcs: ir
                .funcs
                .iter()
                .map(|f| FuncSerde {
                    name: f.name.clone(),
                    params: f
                        .params
                        .iter()
                        .map(|t| IrTySerde::from_ir(*t).unwrap_or(IrTySerde::I64))
                        .collect(),
                    ret: IrTySerde::from_ir(f.ret).unwrap_or(IrTySerde::I64),
                    body: f
                        .body
                        .iter()
                        .filter_map(|tagged| op_to_serde(&tagged.op))
                        .collect(),
                })
                .collect(),
            entry_func_index: ir.entry_func_index,
        }
    }

    fn into_ir(self) -> Result<relon_ir::ir::Module, CraneliftError> {
        use relon_ir::ir;
        Ok(ir::Module {
            imports: vec![],
            funcs: self
                .funcs
                .into_iter()
                .map(|f| ir::Func {
                    name: f.name,
                    params: f.params.into_iter().map(|t| t.to_ir()).collect(),
                    ret: f.ret.to_ir(),
                    body: f
                        .body
                        .into_iter()
                        .map(|o| ir::TaggedOp {
                            op: serde_to_op(o),
                            range: relon_parser::TokenRange::default(),
                        })
                        .collect(),
                    range: relon_parser::TokenRange::default(),
                })
                .collect(),
            entry_func_index: self.entry_func_index,
            closure_table: vec![],
        })
    }
}

fn op_to_serde(op: &relon_ir::ir::Op) -> Option<OpSerde> {
    use relon_ir::ir::Op as I;
    Some(match op {
        I::ConstI64(v) => OpSerde::ConstI64(*v),
        I::ConstI32(v) => OpSerde::ConstI32(*v),
        I::ConstBool(b) => OpSerde::ConstBool(*b),
        I::LocalGet(idx) => OpSerde::LocalGet(*idx),
        I::Add(ty) => OpSerde::Add(IrTySerde::from_ir(*ty).ok()?),
        I::Sub(ty) => OpSerde::Sub(IrTySerde::from_ir(*ty).ok()?),
        I::Mul(ty) => OpSerde::Mul(IrTySerde::from_ir(*ty).ok()?),
        I::Div(ty) => OpSerde::Div(IrTySerde::from_ir(*ty).ok()?),
        I::Mod(ty) => OpSerde::Mod(IrTySerde::from_ir(*ty).ok()?),
        I::Eq(ty) => OpSerde::Eq(IrTySerde::from_ir(*ty).ok()?),
        I::Ne(ty) => OpSerde::Ne(IrTySerde::from_ir(*ty).ok()?),
        I::Lt(ty) => OpSerde::Lt(IrTySerde::from_ir(*ty).ok()?),
        I::Le(ty) => OpSerde::Le(IrTySerde::from_ir(*ty).ok()?),
        I::Gt(ty) => OpSerde::Gt(IrTySerde::from_ir(*ty).ok()?),
        I::Ge(ty) => OpSerde::Ge(IrTySerde::from_ir(*ty).ok()?),
        I::Return => OpSerde::Return,
        I::CheckCap { cap_bit } => OpSerde::CheckCap { cap_bit: *cap_bit },
        _ => return None,
    })
}

fn serde_to_op(op: OpSerde) -> relon_ir::ir::Op {
    use relon_ir::ir::Op as I;
    match op {
        OpSerde::ConstI64(v) => I::ConstI64(v),
        OpSerde::ConstI32(v) => I::ConstI32(v),
        OpSerde::ConstBool(b) => I::ConstBool(b),
        OpSerde::LocalGet(idx) => I::LocalGet(idx),
        OpSerde::Add(ty) => I::Add(ty.to_ir()),
        OpSerde::Sub(ty) => I::Sub(ty.to_ir()),
        OpSerde::Mul(ty) => I::Mul(ty.to_ir()),
        OpSerde::Div(ty) => I::Div(ty.to_ir()),
        OpSerde::Mod(ty) => I::Mod(ty.to_ir()),
        OpSerde::Eq(ty) => I::Eq(ty.to_ir()),
        OpSerde::Ne(ty) => I::Ne(ty.to_ir()),
        OpSerde::Lt(ty) => I::Lt(ty.to_ir()),
        OpSerde::Le(ty) => I::Le(ty.to_ir()),
        OpSerde::Gt(ty) => I::Gt(ty.to_ir()),
        OpSerde::Ge(ty) => I::Ge(ty.to_ir()),
        OpSerde::Return => I::Return,
        OpSerde::CheckCap { cap_bit } => I::CheckCap { cap_bit },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relon_ir::ir::{Func, IrType, Module, Op, TaggedOp};
    use relon_parser::TokenRange;

    fn synth_ir_add() -> Module {
        Module {
            imports: vec![],
            funcs: vec![Func {
                name: "run_main".to_string(),
                params: vec![IrType::I64, IrType::I64],
                ret: IrType::I64,
                body: vec![
                    TaggedOp {
                        op: Op::LocalGet(0),
                        range: TokenRange::default(),
                    },
                    TaggedOp {
                        op: Op::LocalGet(1),
                        range: TokenRange::default(),
                    },
                    TaggedOp {
                        op: Op::Add(IrType::I64),
                        range: TokenRange::default(),
                    },
                    TaggedOp {
                        op: Op::Return,
                        range: TokenRange::default(),
                    },
                ],
                range: TokenRange::default(),
            }],
            entry_func_index: Some(0),
            closure_table: vec![],
        }
    }

    #[test]
    fn sandbox_config_round_trips_through_bit_pack() {
        let cfg = SandboxConfig::default();
        let bits = pack_sandbox(&cfg);
        let unpacked = unpack_sandbox(bits);
        assert_eq!(unpacked.bounds_check, cfg.bounds_check);
        assert_eq!(unpacked.deadline_check, cfg.deadline_check);
        assert_eq!(unpacked.capability_check, cfg.capability_check);
        assert_eq!(unpacked.div_check, cfg.div_check);

        let cfg = SandboxConfig::unchecked();
        let bits = pack_sandbox(&cfg);
        let unpacked = unpack_sandbox(bits);
        assert!(!unpacked.bounds_check);
        assert!(!unpacked.deadline_check);
        assert!(!unpacked.capability_check);
        assert!(!unpacked.div_check);
    }

    #[test]
    fn cache_round_trip_preserves_ir_and_sandbox() {
        let entry = CacheEntry {
            ir: synth_ir_add(),
            sandbox: SandboxConfig::default(),
        };
        let bytes = serialize(&entry).expect("serialize");
        let decoded = deserialize(&bytes).expect("deserialize");
        assert_eq!(decoded.ir.funcs.len(), 1);
        assert_eq!(decoded.ir.funcs[0].params.len(), 2);
        assert_eq!(decoded.ir.funcs[0].body.len(), 4);
        assert!(decoded.sandbox.bounds_check);
    }

    #[test]
    fn cache_rejects_magic_mismatch() {
        let mut bytes = serialize(&CacheEntry {
            ir: synth_ir_add(),
            sandbox: SandboxConfig::default(),
        })
        .unwrap();
        bytes[0] = b'X';
        let err = deserialize(&bytes).expect_err("magic mismatch detected");
        assert!(format!("{err}").contains("magic"));
    }

    #[test]
    fn cache_rejects_digest_mismatch() {
        let mut bytes = serialize(&CacheEntry {
            ir: synth_ir_add(),
            sandbox: SandboxConfig::default(),
        })
        .unwrap();
        // Corrupt the last byte (within the sha256 digest area).
        let last = bytes.len() - 1;
        bytes[last] = bytes[last].wrapping_add(1);
        let err = deserialize(&bytes).expect_err("digest mismatch detected");
        assert!(format!("{err}").contains("sha256"));
    }
}
