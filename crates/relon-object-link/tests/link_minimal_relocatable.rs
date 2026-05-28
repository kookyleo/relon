//! End-to-end fixture: cranelift-object -> link_to_dyn -> dlopen.
//!
//! Builds a tiny relocatable `.o` exporting
//! `relon_link_add(i64, i64) -> i64` via cranelift-object, runs it
//! through `link_to_dyn` (the public entry point of this crate),
//! then hands the resulting ET_DYN bytes to `relon-object-cache`'s
//! `LoadedObject::from_bytes` and actually calls the function. If
//! any link step in the v5-gamma pipeline regresses, this test will
//! catch it before the codegen-cranelift crate hits the integration
//! point.

#![cfg(target_os = "linux")]

use cranelift_codegen::ir::{types::I64, AbiParam, Function, InstBuilder, Signature, UserFuncName};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::Context as CodegenContext;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{Linkage, Module as CrModule};
use cranelift_object::{ObjectBuilder, ObjectModule};
use relon_object_link::{is_et_dyn, is_et_rel, link_to_dyn, LinkError, SubprocLinker};

/// Build the relocatable `.o` for the fixture and return its bytes +
/// the host triple the ISA was tuned for.
fn build_add_object() -> (Vec<u8>, String) {
    let mut flag_builder = settings::builder();
    flag_builder.set("opt_level", "speed").unwrap();
    // PIC is mandatory for `-shared` to succeed on most binutils
    // versions; cranelift defaults to non-PIC for ObjectModule.
    flag_builder.set("is_pic", "true").unwrap();
    let isa_builder = cranelift_native::builder().expect("host ISA must be supported");
    let isa = isa_builder
        .finish(settings::Flags::new(flag_builder))
        .expect("ISA finalises with valid flags");
    let triple = isa.triple().to_string();

    let builder = ObjectBuilder::new(
        isa,
        "relon-object-link-test",
        cranelift_module::default_libcall_names(),
    )
    .expect("ObjectBuilder accepts host ISA");
    let mut module = ObjectModule::new(builder);

    let mut sig = Signature::new(CallConv::SystemV);
    sig.params.push(AbiParam::new(I64));
    sig.params.push(AbiParam::new(I64));
    sig.returns.push(AbiParam::new(I64));

    let fn_id = module
        .declare_function("relon_link_add", Linkage::Export, &sig)
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
    (obj_bytes, triple)
}

#[test]
fn cranelift_output_is_et_rel() {
    let (obj_bytes, _triple) = build_add_object();
    assert!(
        is_et_rel(&obj_bytes),
        "cranelift-object should emit ET_REL; got something else"
    );
    assert!(
        !is_et_dyn(&obj_bytes),
        "cranelift-object must not emit ET_DYN — link pass would be unnecessary"
    );
}

#[test]
fn link_to_dyn_produces_et_dyn() {
    let (obj_bytes, triple) = build_add_object();
    let dyn_bytes = match link_to_dyn(&obj_bytes, &triple) {
        Ok(b) => b,
        Err(LinkError::LinkerNotFound) => {
            eprintln!("skipping: no system linker on PATH");
            return;
        }
        Err(LinkError::UnsupportedTriple(_)) => {
            eprintln!("skipping: host triple not in supported set: {triple}");
            return;
        }
        Err(e) => panic!("link_to_dyn failed: {e:?}"),
    };
    assert!(
        is_et_dyn(&dyn_bytes),
        "link output should be ET_DYN, got {:?}",
        relon_object_link::parse_elf_type(&dyn_bytes)
    );
}

#[test]
fn linked_dyn_loads_and_executes_via_object_cache() {
    let (obj_bytes, triple) = build_add_object();
    let dyn_bytes = match link_to_dyn(&obj_bytes, &triple) {
        Ok(b) => b,
        Err(LinkError::LinkerNotFound) | Err(LinkError::UnsupportedTriple(_)) => {
            eprintln!("skipping: linker / triple precondition not met");
            return;
        }
        Err(e) => panic!("link_to_dyn failed: {e:?}"),
    };
    let loaded =
        relon_object_cache::LoadedObject::from_bytes(&dyn_bytes, &triple, &["relon_link_add"])
            .expect("ET_DYN bytes must be loadable via memfd + dlopen");
    let ptr = loaded
        .resolve("relon_link_add")
        .expect("dlsym must surface relon_link_add");
    assert!(!ptr.is_null());
    // SAFETY: cranelift emitted a SystemV `extern "C"` function with
    // this signature; the dlopen handle keeps the mapping alive for
    // the rest of this scope.
    let add: extern "C" fn(i64, i64) -> i64 = unsafe { std::mem::transmute(ptr) };
    assert_eq!(add(40, 2), 42);
    assert_eq!(add(-7, 7), 0);
    assert_eq!(add(i64::MIN + 3, 2), i64::MIN + 5);
}

#[test]
fn concurrent_link_invocations_all_succeed() {
    // Each thread builds its own object + links independently. We
    // care that the subprocess linker has no hidden global state that
    // would corrupt parallel cold-starts in the host's executor.
    let (obj_bytes, triple) = build_add_object();
    let linker = match SubprocLinker::new() {
        Ok(l) => l,
        Err(LinkError::LinkerNotFound) => return,
        Err(e) => panic!("{e:?}"),
    };
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let bytes = obj_bytes.clone();
            let triple = triple.clone();
            let linker = linker.clone();
            std::thread::spawn(move || linker.link(&bytes, &triple))
        })
        .collect();
    for h in handles {
        let res = h.join().expect("worker thread panicked");
        match res {
            Ok(out) => assert!(is_et_dyn(&out), "thread produced non-ET_DYN output"),
            Err(LinkError::UnsupportedTriple(_)) => {
                eprintln!("skipping: unsupported triple");
                return;
            }
            Err(e) => panic!("worker link failed: {e:?}"),
        }
    }
}

#[cfg(feature = "lld-inproc")]
#[test]
fn lld_inproc_stub_reports_feature_not_implemented() {
    use relon_object_link::LldLinker;
    let err = LldLinker::new().expect_err("stub must surface FeatureNotImplemented");
    assert!(
        matches!(err, LinkError::FeatureNotImplemented),
        "got {err:?}"
    );
}
