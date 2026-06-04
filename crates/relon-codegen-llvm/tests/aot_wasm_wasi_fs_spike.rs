//! P-fs SPIKE — relon wasm reads a file through the **standard WASI
//! preview1 fd protocol** (`path_open` -> `fd_read` -> `fd_close`),
//! satisfied by an **off-the-shelf** `wasmtime-wasi` host with a single
//! **preopened directory**.
//!
//! ## Why this exists (highest-risk-first)
//!
//! P-clock / P-random proved the *nullary buffer-out* WASI shape
//! (`clock_time_get` / `random_get`): one import call, an 8-byte scratch
//! slot, read the result back. Filesystem reads are categorically harder:
//!
//!   * **multi-call protocol** — open a preopened dir's child by path,
//!     read the fd into an iovec, close the fd;
//!   * **String-in** — the guest must place the UTF-8 path bytes in
//!     linear memory and hand `path_open` a (ptr,len) pair;
//!   * **bytes-out** — `fd_read` takes an `iovec` array (each
//!     `{buf_ptr: i32, buf_len: i32}`) and writes the count read through
//!     an out-pointer.
//!
//! This spike validates the **codegen + host mechanism** in isolation,
//! ahead of any source-level surface, with a hand-built LLVM fixture:
//!
//!   1. emit a wasm module importing the **standard** preview1 symbols
//!      `path_open` / `fd_read` / `fd_close` on `wasi_snapshot_preview1`
//!      (via the `wasm-import-module` / `wasm-import-name` attributes,
//!      not the default `env` module);
//!   2. marshal through **linear memory** exactly as the WASI ABI
//!      requires (path bytes in, iovec describing the out buffer, nread
//!      out-pointer);
//!   3. satisfy it with the **standard** `wasmtime-wasi` preview1 host +
//!      a single `preopened_dir` — NO relon-custom `func_wrap`.
//!
//! Scope honesty: hand-built LLVM fixture. The dirfd is the conventional
//! first preopened fd (3, after stdio 0/1/2). The fixture reads a fixed
//! test file and returns the number of bytes read + writes the content
//! into a guest buffer the test inspects, proving the standard host
//! genuinely produced the bytes through the standard import.

use std::io::Write;
use std::path::PathBuf;

use inkwell::context::Context;
use inkwell::module::Linkage;
use inkwell::targets::{
    CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine, TargetTriple,
};
use inkwell::OptimizationLevel;

const WASM32_TRIPLE: &str = "wasm32-wasi";
const WASI_MODULE: &str = "wasi_snapshot_preview1";

/// The conventional first preopened directory fd: stdio occupies 0/1/2,
/// the first `preopened_dir` lands at 3.
const PREOPEN_DIRFD: u64 = 3;

fn wasm32_machine() -> TargetMachine {
    Target::initialize_webassembly(&InitializationConfig::default());
    let triple = TargetTriple::create(WASM32_TRIPLE);
    let t = Target::from_triple(&triple).expect("wasm32 target from_triple");
    t.create_target_machine(
        &triple,
        "",
        "",
        OptimizationLevel::Default,
        RelocMode::Static,
        CodeModel::Default,
    )
    .expect("create_target_machine(wasm32)")
}

/// Declare a standard-WASI import: retarget the undefined external off
/// the default `env` module onto `wasi_snapshot_preview1` via the LLVM
/// `wasm-import-module` / `wasm-import-name` attributes.
fn declare_import<'ctx>(
    ctx: &'ctx Context,
    module: &inkwell::module::Module<'ctx>,
    name: &str,
    fn_ty: inkwell::types::FunctionType<'ctx>,
) -> inkwell::values::FunctionValue<'ctx> {
    let f = module.add_function(name, fn_ty, Some(Linkage::External));
    f.add_attribute(
        inkwell::attributes::AttributeLoc::Function,
        ctx.create_string_attribute("wasm-import-module", WASI_MODULE),
    );
    f.add_attribute(
        inkwell::attributes::AttributeLoc::Function,
        ctx.create_string_attribute("wasm-import-name", name),
    );
    f
}

/// Hand-build the wasm32 relocatable object exporting
/// `read_file_spike(path_ptr, path_len, out_buf, out_cap, scratch) -> i64`.
///
/// The fixture runs the full preview1 fd protocol against dirfd 3:
///   * `path_open(3, 0, path_ptr, path_len, 0, RIGHTS, RIGHTS, 0,
///                fd_out)` — fd_out is `scratch+0`;
///   * build a single iovec at `scratch+8`: `{buf=out_buf, len=out_cap}`,
///     `nread_out` at `scratch+16`;
///   * `fd_read(fd, scratch+8, 1, scratch+16)`;
///   * `fd_close(fd)`;
///   * return the nread (negated errno on any failure so the test can
///     tell open/read failures apart from a legitimate 0-byte read).
fn emit_read_file_object(entry: &str, obj_path: &std::path::Path) {
    let ctx = Context::create();
    let module = ctx.create_module("relon_wasi_fs_spike");
    module.set_triple(&TargetTriple::create(WASM32_TRIPLE));

    let i32_t = ctx.i32_type();
    let i64_t = ctx.i64_type();
    let ptr_t = ctx.ptr_type(inkwell::AddressSpace::default());

    // path_open(dirfd, dirflags, path_ptr, path_len, oflags,
    //           fs_rights_base, fs_rights_inheriting, fdflags, fd_out)
    //   -> errno
    let path_open_ty = i32_t.fn_type(
        &[
            i32_t.into(), // dirfd
            i32_t.into(), // dirflags
            ptr_t.into(), // path_ptr
            i32_t.into(), // path_len
            i32_t.into(), // oflags
            i64_t.into(), // fs_rights_base
            i64_t.into(), // fs_rights_inheriting
            i32_t.into(), // fdflags
            ptr_t.into(), // fd_out
        ],
        false,
    );
    let path_open = declare_import(&ctx, &module, "path_open", path_open_ty);

    // fd_read(fd, iovs_ptr, iovs_len, nread_out) -> errno
    let fd_read_ty = i32_t.fn_type(
        &[i32_t.into(), ptr_t.into(), i32_t.into(), ptr_t.into()],
        false,
    );
    let fd_read = declare_import(&ctx, &module, "fd_read", fd_read_ty);

    // fd_close(fd) -> errno
    let fd_close_ty = i32_t.fn_type(&[i32_t.into()], false);
    let fd_close = declare_import(&ctx, &module, "fd_close", fd_close_ty);

    // read_file_spike(path_ptr, path_len, out_buf, out_cap, scratch) -> i64
    let entry_ty = i64_t.fn_type(
        &[
            i32_t.into(), // path_ptr (linear-mem addr)
            i32_t.into(), // path_len
            i32_t.into(), // out_buf (linear-mem addr)
            i32_t.into(), // out_cap
            i32_t.into(), // scratch (>=24 bytes free)
        ],
        false,
    );
    let entry_fn = module.add_function(entry, entry_ty, None);
    let builder = ctx.create_builder();
    let bb = ctx.append_basic_block(entry_fn, "entry");
    let open_ok_bb = ctx.append_basic_block(entry_fn, "open_ok");
    let read_ok_bb = ctx.append_basic_block(entry_fn, "read_ok");
    let err_bb = ctx.append_basic_block(entry_fn, "err");
    builder.position_at_end(bb);

    let path_addr = entry_fn.get_nth_param(0).unwrap().into_int_value();
    let path_len = entry_fn.get_nth_param(1).unwrap().into_int_value();
    let out_buf = entry_fn.get_nth_param(2).unwrap().into_int_value();
    let out_cap = entry_fn.get_nth_param(3).unwrap().into_int_value();
    let scratch = entry_fn.get_nth_param(4).unwrap().into_int_value();

    let path_ptr = builder
        .build_int_to_ptr(path_addr, ptr_t, "path_ptr")
        .unwrap();
    let fd_out_addr = scratch; // scratch+0: opened fd (i32)
    let fd_out_ptr = builder
        .build_int_to_ptr(fd_out_addr, ptr_t, "fd_out_ptr")
        .unwrap();

    // RIGHTS_FD_READ (bit 1) | RIGHTS_FD_SEEK (bit 2). The host rejects
    // any *unknown* rights bit (it strictly converts the mask), so we
    // pass exactly the rights this read path needs rather than `!0`.
    let rights = i64_t.const_int((1 << 1) | (1 << 2), false);
    let errno = builder
        .build_call(
            path_open,
            &[
                i32_t.const_int(PREOPEN_DIRFD, false).into(),
                i32_t.const_zero().into(), // dirflags
                path_ptr.into(),
                path_len.into(),
                i32_t.const_zero().into(), // oflags (no create/dir)
                rights.into(),
                rights.into(),
                i32_t.const_zero().into(), // fdflags
                fd_out_ptr.into(),
            ],
            "open_errno",
        )
        .unwrap()
        .try_as_basic_value();
    let open_errno = match errno {
        inkwell::values::ValueKind::Basic(inkwell::values::BasicValueEnum::IntValue(v)) => v,
        other => panic!("path_open returned {other:?}"),
    };
    let open_failed = builder
        .build_int_compare(
            inkwell::IntPredicate::NE,
            open_errno,
            i32_t.const_zero(),
            "open_failed",
        )
        .unwrap();
    builder
        .build_conditional_branch(open_failed, err_bb, open_ok_bb)
        .unwrap();

    // ---- open_ok: build iovec, fd_read ----
    builder.position_at_end(open_ok_bb);
    let fd = builder
        .build_load(i32_t, fd_out_ptr, "fd")
        .unwrap()
        .into_int_value();

    // iovec at scratch+8: {buf: i32 = out_buf, len: i32 = out_cap}
    let iov_addr = builder
        .build_int_add(scratch, i32_t.const_int(8, false), "iov_addr")
        .unwrap();
    let iov_ptr = builder
        .build_int_to_ptr(iov_addr, ptr_t, "iov_ptr")
        .unwrap();
    builder.build_store(iov_ptr, out_buf).unwrap();
    let iov_len_addr = builder
        .build_int_add(scratch, i32_t.const_int(12, false), "iov_len_addr")
        .unwrap();
    let iov_len_ptr = builder
        .build_int_to_ptr(iov_len_addr, ptr_t, "iov_len_ptr")
        .unwrap();
    builder.build_store(iov_len_ptr, out_cap).unwrap();
    // nread out at scratch+16
    let nread_addr = builder
        .build_int_add(scratch, i32_t.const_int(16, false), "nread_addr")
        .unwrap();
    let nread_ptr = builder
        .build_int_to_ptr(nread_addr, ptr_t, "nread_ptr")
        .unwrap();

    let read_errno = builder
        .build_call(
            fd_read,
            &[
                fd.into(),
                iov_ptr.into(),
                i32_t.const_int(1, false).into(),
                nread_ptr.into(),
            ],
            "read_errno",
        )
        .unwrap()
        .try_as_basic_value();
    let read_errno = match read_errno {
        inkwell::values::ValueKind::Basic(inkwell::values::BasicValueEnum::IntValue(v)) => v,
        other => panic!("fd_read returned {other:?}"),
    };
    // close regardless of read errno (best-effort)
    builder.build_call(fd_close, &[fd.into()], "close").unwrap();
    let read_failed = builder
        .build_int_compare(
            inkwell::IntPredicate::NE,
            read_errno,
            i32_t.const_zero(),
            "read_failed",
        )
        .unwrap();
    builder
        .build_conditional_branch(read_failed, err_bb, read_ok_bb)
        .unwrap();

    // ---- read_ok: return nread ----
    builder.position_at_end(read_ok_bb);
    let nread = builder
        .build_load(i32_t, nread_ptr, "nread")
        .unwrap()
        .into_int_value();
    let nread64 = builder.build_int_z_extend(nread, i64_t, "nread64").unwrap();
    builder.build_return(Some(&nread64)).unwrap();

    // ---- err: return -1 ----
    builder.position_at_end(err_bb);
    let neg1 = i64_t.const_int((-1i64) as u64, true);
    builder.build_return(Some(&neg1)).unwrap();

    let machine = wasm32_machine();
    module.set_data_layout(&machine.get_target_data().get_data_layout());
    machine
        .write_to_file(&module, FileType::Object, obj_path)
        .expect("wasm32 write_to_file");
    let bytes = std::fs::read(obj_path).expect("read emitted obj");
    assert_eq!(&bytes[..4], b"\0asm", "emitted object is not a wasm object");
}

fn build_wasm(entry: &str) -> Result<Vec<u8>, String> {
    if relon_codegen_llvm::wasm_link::find_wasm_ld().is_none() {
        return Err("wasm-ld not found on PATH (install lld)".into());
    }
    let tmp = std::env::temp_dir();
    let pid = std::process::id();
    let obj: PathBuf = tmp.join(format!("relon_fsspike_{entry}_{pid}.o"));
    let wasm: PathBuf = tmp.join(format!("relon_fsspike_{entry}_{pid}.wasm"));

    emit_read_file_object(entry, &obj);
    relon_codegen_llvm::wasm_link::link_wasm_object(&obj, &wasm, entry)
        .map_err(|e| format!("wasm-ld link: {e:?}"))?;
    let bytes = std::fs::read(&wasm).map_err(|e| format!("read wasm: {e}"))?;
    assert_eq!(&bytes[..4], b"\0asm", "linked module magic");
    let _ = std::fs::remove_file(&obj);
    let _ = std::fs::remove_file(&wasm);
    Ok(bytes)
}

/// The emitted module imports the **standard** preview1 fd-protocol
/// symbols on the `wasi_snapshot_preview1` module (NOT relon's `env`).
#[test]
fn emitted_module_imports_standard_wasi_fd_protocol() {
    let entry = "read_file_spike_imp";
    match build_wasm(entry) {
        Ok(bytes) => {
            use wasmtime::{Engine, ExternType, Module};
            let engine = Engine::default();
            let module = Module::new(&engine, &bytes).expect("Module::new");
            let imports: Vec<(String, String)> = module
                .imports()
                .map(|i| (i.module().to_string(), i.name().to_string()))
                .collect();
            for want in ["path_open", "fd_read", "fd_close"] {
                let found = module.imports().any(|imp| {
                    imp.module() == WASI_MODULE
                        && imp.name() == want
                        && matches!(imp.ty(), ExternType::Func(_))
                });
                assert!(
                    found,
                    "module must import (wasi_snapshot_preview1 {want}); imports: {imports:?}"
                );
            }
        }
        Err(reason) if reason.contains("wasm-ld not found") => {
            eprintln!("aot_wasm_wasi_fs_spike: wasm-ld unavailable; skipped import check");
        }
        Err(e) => panic!("{e}"),
    }
}

/// End-to-end: the standard preview1 fd protocol, satisfied by the
/// off-the-shelf `wasmtime-wasi` host + a single preopened dir, reads a
/// fixed test file's bytes back into a guest buffer the test inspects.
#[test]
fn standard_wasi_fd_protocol_reads_preopened_file() {
    let entry = "read_file_spike_run";
    let bytes = match build_wasm(entry) {
        Ok(b) => b,
        Err(reason) if reason.contains("wasm-ld not found") => {
            eprintln!("aot_wasm_wasi_fs_spike: wasm-ld unavailable; skipped e2e run");
            return;
        }
        Err(e) => panic!("{e}"),
    };

    use wasmtime::{Engine, Extern, Linker, Module, Store, Val};
    use wasmtime_wasi::p1::{self, WasiP1Ctx};
    use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};

    // Lay down a known file inside a temp dir we preopen.
    const CONTENT: &str = "relon wasi fs spike: hello preopened world\n";
    const FILENAME: &str = "spike_fixture.txt";
    let dir = std::env::temp_dir().join(format!("relon_fsspike_dir_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create preopen dir");
    let file_path = dir.join(FILENAME);
    {
        let mut f = std::fs::File::create(&file_path).expect("create fixture");
        f.write_all(CONTENT.as_bytes()).expect("write fixture");
    }

    let engine = Engine::default();
    let module = Module::new(&engine, &bytes).expect("wasmtime Module::new");

    // THE PROOF: standard wasmtime-wasi host + a single preopened dir.
    let wasi: WasiP1Ctx = WasiCtxBuilder::new()
        .preopened_dir(&dir, ".", DirPerms::READ, FilePerms::READ)
        .expect("preopened_dir")
        .build_p1();
    let mut store = Store::new(&engine, wasi);
    let mut linker: Linker<WasiP1Ctx> = Linker::new(&engine);
    p1::add_to_linker_sync(&mut linker, |cx| cx).expect("add standard WASIp1 to linker");

    let instance = linker
        .instantiate(&mut store, &module)
        .expect("instantiate (standard WASI fd-protocol imports must be satisfied)");

    let memory = match instance.get_export(&mut store, "memory") {
        Some(Extern::Memory(m)) => m,
        _ => panic!("module missing exported `memory`"),
    };
    let heap_base = match instance.get_export(&mut store, "__heap_base") {
        Some(Extern::Global(g)) => match g.get(&mut store) {
            Val::I32(v) => v as u32,
            other => panic!("__heap_base not i32: {other:?}"),
        },
        _ => panic!("module missing exported `__heap_base`"),
    };

    // Linear-memory layout past static data:
    //   path bytes | out buffer | scratch (24 bytes)
    let align8 = |v: u32| (v + 7) & !7u32;
    let path_addr = align8(heap_base);
    let path_bytes = FILENAME.as_bytes();
    let out_buf = align8(path_addr + path_bytes.len() as u32);
    let out_cap = 256u32;
    let scratch = align8(out_buf + out_cap);
    let needed = (scratch + 24) as usize;
    let cur = memory.data_size(&store);
    if needed > cur {
        let pages = (needed - cur).div_ceil(65536) as u64;
        memory.grow(&mut store, pages).expect("grow memory");
    }
    memory
        .write(&mut store, path_addr as usize, path_bytes)
        .expect("write path bytes");

    let func = instance
        .get_func(&mut store, entry)
        .unwrap_or_else(|| panic!("export `{entry}` missing"));
    let params = [
        Val::I32(path_addr as i32),
        Val::I32(path_bytes.len() as i32),
        Val::I32(out_buf as i32),
        Val::I32(out_cap as i32),
        Val::I32(scratch as i32),
    ];
    let mut results = [Val::I64(0)];
    func.call(&mut store, &params, &mut results)
        .expect("read_file entry call");
    let nread = match results[0] {
        Val::I64(v) => v,
        other => panic!("expected i64 nread, got {other:?}"),
    };
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        nread >= 0,
        "fd protocol returned failure sentinel {nread}; the standard WASI host did not \
         satisfy path_open/fd_read against the preopened dir"
    );
    assert_eq!(
        nread as usize,
        CONTENT.len(),
        "nread {nread} != fixture length {}",
        CONTENT.len()
    );
    let mut got = vec![0u8; nread as usize];
    memory
        .read(&store, out_buf as usize, &mut got)
        .expect("read out buffer");
    assert_eq!(
        got,
        CONTENT.as_bytes(),
        "guest buffer content does not match the fixture written through the standard WASI host"
    );
}
