//! v5-γ stage 2 latency-breakdown smoke. Probes each phase of the
//! cached cold-start path so the stage-2 report can attribute the
//! ~340 µs total to dlopen vs schema-decode vs vtable populate.
//!
//! Not a benchmark — Criterion lives in `relon-bench` — but this
//! test prints the per-phase timing on a `cargo test
//! --nocapture` run so the report writer can grab the numbers
//! quickly without reaching for `perf` or `flamegraph`.

use std::time::Instant;

use relon_codegen_native::object_cache_integration as cache_int;
use relon_codegen_native::vtable::{populate_vtable, VTABLE_SYMBOL};
use relon_codegen_native::CraneliftAotEvaluator;
use tempfile::tempdir;

#[test]
fn cached_cold_start_phase_breakdown_prints() {
    let cache = tempdir().expect("tempdir");
    let src = "#main(Int x, Int y) -> Int\nx + y";

    // Pre-warm. Not timed.
    let warm = CraneliftAotEvaluator::from_source_with_cache(src, cache.path())
        .expect("from_source_with_cache");
    drop(warm);

    // Phase 1: cache integrity validation. Reads ELF bytes + IR
    // bytes from disk, verifies HMAC.
    let sandbox = relon_codegen_native::SandboxConfig::default();
    let source_hash = cache_int::compute_source_hash(src, &sandbox);
    let metadata = phony_metadata(src, &sandbox);

    let t0 = Instant::now();
    let loaded = cache_int::try_load_from_cache(cache.path(), source_hash, &metadata)
        .expect("load")
        .expect("hit");
    let t_load = t0.elapsed();

    // Phase 2: schema cache decode. #171: the sidecar is HMAC-sealed
    // against the source + object hashes; mirror what `from_cache_dir`
    // does at the production layer.
    let schema_path =
        relon_codegen_native::schema_cache::schema_cache_path_for(cache.path(), source_hash);
    let t1 = Instant::now();
    let schema_bytes = std::fs::read(&schema_path).expect("schema read");
    let schema_entry = relon_codegen_native::schema_cache::deserialize(
        &schema_bytes,
        &source_hash,
        &loaded.object_sha256,
        &loaded.hmac_key,
    )
    .expect("schema decode");
    let t_schema = t1.elapsed();

    // Phase 3: dlopen + dlsym.
    let mut symbols = vec!["run_main".to_string(), VTABLE_SYMBOL.to_string()];
    for i in 0..schema_entry.closure_count {
        symbols.push(format!("__closure_{i}"));
    }
    let sym_refs: Vec<&str> = symbols.iter().map(|s| s.as_str()).collect();
    let t2 = Instant::now();
    let lo = relon_object_cache::LoadedObject::from_bytes(
        &loaded.object_bytes,
        cache_int::host_target_triple(),
        &sym_refs,
    )
    .expect("dlopen");
    let t_dlopen = t2.elapsed();

    // Phase 4: vtable populate.
    let vtable_ptr = lo.resolve(VTABLE_SYMBOL).expect("vtable").cast::<u8>() as *mut u8;
    let t3 = Instant::now();
    unsafe { populate_vtable(vtable_ptr) };
    let t_populate = t3.elapsed();

    let total = t_load + t_schema + t_dlopen + t_populate;
    eprintln!(
        "v5-γ stage 2 cached cold-start breakdown:\n  cache_load = {:>8?}\n  schema_decode = {:>8?}\n  dlopen+dlsym = {:>8?}\n  vtable_populate = {:>8?}\n  total = {:>8?}",
        t_load, t_schema, t_dlopen, t_populate, total
    );
}

fn phony_metadata(
    source: &str,
    sandbox: &relon_codegen_native::SandboxConfig,
) -> relon_object_cache::Metadata {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(b"relon-main-signature/v1\0");
    hasher.update(source.as_bytes());
    let mut sig = [0u8; 32];
    sig.copy_from_slice(&hasher.finalize());
    cache_int::build_metadata(sandbox, 0, sig, Vec::new())
}
