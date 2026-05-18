//! End-to-end loader smoke test. We emit a real ELF object with
//! cranelift-object that exports an `add(a: i64, b: i64) -> i64`
//! function, link it into a shared object with the system linker,
//! run those bytes through the memfd + dlopen path, and `dlsym`
//! the result back into a function pointer. Then we actually
//! invoke it and check the math.
//!
//! ## Why the linker step
//!
//! cranelift-object emits `ET_REL` (relocatable `.o`); glibc /
//! musl `dlopen` only accept `ET_DYN` (shared library `.so`). The
//! v5-gamma codegen pipeline will therefore have to take cranelift's
//! `.o` output and run a final `ld -shared` (or an in-process
//! linker) before handing the bytes to this crate. The cache itself
//! stores whatever the codegen produces — opaque to us — so the
//! test models the full pipeline end-to-end.
//!
//! Linux-only — on other targets the loader returns
//! `LoaderError::UnsupportedPlatform`, which we exercise from a
//! separate test below.

#![allow(clippy::missing_safety_doc)]

#[cfg(target_os = "linux")]
mod linux {
    use cranelift_codegen::ir::{
        types::I64, AbiParam, Function, InstBuilder, Signature, UserFuncName,
    };
    use cranelift_codegen::isa::CallConv;
    use cranelift_codegen::settings::{self, Configurable};
    use cranelift_codegen::Context as CodegenContext;
    use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
    use cranelift_module::{Linkage, Module as CrModule};
    use cranelift_object::{ObjectBuilder, ObjectModule};
    use relon_object_cache::LoadedObject;
    use std::process::Command;

    /// Build a tiny relocatable ELF that exports `add(i64, i64) -> i64`,
    /// then link it into a shared object with the system linker.
    /// Returns the shared-object bytes plus the target triple.
    ///
    /// Skips the test (returns `None`) if no usable linker is on
    /// `$PATH` so we do not break sandboxed CI environments.
    pub(super) fn build_add_object() -> Option<(Vec<u8>, String)> {
        let mut flag_builder = settings::builder();
        flag_builder.set("opt_level", "speed").unwrap();
        // Position-independent code so dlopen can map the object
        // anywhere in the address space.
        flag_builder.set("is_pic", "true").unwrap();
        let isa_builder = cranelift_native::builder().expect("host ISA must be supported");
        let isa = isa_builder
            .finish(settings::Flags::new(flag_builder))
            .expect("ISA finalises with valid flags");

        let triple = isa.triple().to_string();

        let builder = ObjectBuilder::new(
            isa,
            "relon-object-cache-test",
            cranelift_module::default_libcall_names(),
        )
        .expect("ObjectBuilder accepts the host ISA");
        let mut module = ObjectModule::new(builder);

        let mut sig = Signature::new(CallConv::SystemV);
        sig.params.push(AbiParam::new(I64));
        sig.params.push(AbiParam::new(I64));
        sig.returns.push(AbiParam::new(I64));

        let fn_id = module
            .declare_function("relon_test_add", Linkage::Export, &sig)
            .unwrap();

        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        {
            let mut fb_ctx = FunctionBuilderContext::new();
            let mut fb = FunctionBuilder::new(&mut func, &mut fb_ctx);
            let block = fb.create_block();
            fb.append_block_params_for_function_params(block);
            fb.switch_to_block(block);
            fb.seal_block(block);
            let params = fb.block_params(block).to_vec();
            let sum = fb.ins().iadd(params[0], params[1]);
            fb.ins().return_(&[sum]);
            fb.finalize();
        }

        let mut ctx = CodegenContext::for_function(func);
        module.define_function(fn_id, &mut ctx).unwrap();

        let product = module.finish();
        let obj_bytes = product.emit().unwrap();

        // Link `.o` -> `.so` via the system linker. We try `ld`
        // directly with `-shared`; if that fails we fall back to
        // `cc -shared -nostdlib` which a wider set of distros ship
        // on the default path. Both produce a valid ET_DYN that
        // glibc / musl will accept.
        let tmp = tempfile::tempdir().expect("tempdir available");
        let obj_path = tmp.path().join("add.o");
        let so_path = tmp.path().join("libadd.so");
        std::fs::write(&obj_path, &obj_bytes).unwrap();

        let mut linked = false;
        for argv in [
            vec!["ld", "-shared", "-o"],
            vec!["cc", "-shared", "-nostdlib", "-o"],
        ] {
            let mut cmd = Command::new(argv[0]);
            cmd.args(&argv[1..]).arg(&so_path).arg(&obj_path);
            match cmd.output() {
                Ok(out) if out.status.success() && so_path.exists() => {
                    linked = true;
                    break;
                }
                Ok(_) | Err(_) => continue,
            }
        }
        if !linked {
            eprintln!("skipping: no system linker (ld / cc) on PATH");
            return None;
        }
        let bytes = std::fs::read(&so_path).unwrap();
        Some((bytes, triple))
    }

    #[test]
    fn dlopen_and_call_add() {
        let Some((object_bytes, triple)) = build_add_object() else {
            return;
        };
        let loaded = LoadedObject::from_bytes(&object_bytes, &triple, &["relon_test_add"])
            .expect("memfd + dlopen + dlsym must succeed on linux");
        let ptr = loaded
            .resolve("relon_test_add")
            .expect("dlsym resolved relon_test_add");
        assert!(!ptr.is_null());

        // SAFETY: cranelift emitted a SystemV `extern "C"` function
        // with this exact signature; the dlopen handle keeps the
        // code mapping alive for the rest of this scope.
        let add: extern "C" fn(i64, i64) -> i64 = unsafe { std::mem::transmute(ptr) };
        assert_eq!(add(40, 2), 42);
        assert_eq!(add(-1, 1), 0);
        assert_eq!(add(i64::MIN + 5, 4), i64::MIN + 9);
    }

    #[test]
    fn missing_symbol_surfaces_error() {
        let Some((object_bytes, triple)) = build_add_object() else {
            return;
        };
        let err =
            LoadedObject::from_bytes(&object_bytes, &triple, &["definitely_not_a_real_symbol"])
                .expect_err("missing symbol must surface SymbolNotFound");
        match err {
            relon_object_cache::LoaderError::SymbolNotFound(name) => {
                assert_eq!(name, "definitely_not_a_real_symbol");
            }
            other => panic!("expected SymbolNotFound, got {other:?}"),
        }
    }

    #[test]
    fn iter_symbols_enumerates_resolved_entries() {
        let Some((object_bytes, triple)) = build_add_object() else {
            return;
        };
        let loaded = LoadedObject::from_bytes(&object_bytes, &triple, &["relon_test_add"]).unwrap();
        let names: Vec<_> = loaded.iter_symbols().map(|(n, _)| n.to_owned()).collect();
        assert_eq!(names, vec!["relon_test_add".to_owned()]);
    }

    #[test]
    fn garbage_object_bytes_surface_dlopen_error() {
        // Plausible-looking ELF magic + nothing else — the linker
        // should reject.
        let mut bytes = vec![0u8; 64];
        bytes[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        let err = LoadedObject::from_bytes(&bytes, "x86_64-unknown-linux-gnu", &[])
            .expect_err("garbage bytes must fail to dlopen");
        assert!(
            matches!(err, relon_object_cache::LoaderError::Dlopen(_)),
            "got {err:?}"
        );
    }
}

#[cfg(not(target_os = "linux"))]
#[test]
fn loader_returns_unsupported_platform_off_linux() {
    use relon_object_cache::{LoadedObject, LoaderError};
    let bytes = vec![0u8; 16];
    let err = LoadedObject::from_bytes(&bytes, "irrelevant", &[])
        .expect_err("non-linux targets must return UnsupportedPlatform");
    assert!(
        matches!(err, LoaderError::UnsupportedPlatform),
        "got {err:?}"
    );
}
