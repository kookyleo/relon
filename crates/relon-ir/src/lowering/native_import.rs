//! Lowering sub-module: `#native` host-fn import resolution.
//!
//! Resolves host-registered native signatures + capability gates into
//! IR-ready [`NativeImport`] entries (`NativeImportBuilder`), and
//! surfaces the "signature registered but no gate declared" fail-open
//! diagnostic (`undeclared_gate_imports`).

use super::*;

/// A host-registered native fn resolved into IR-ready shape: the
/// param/return IR types plus the capability bit indices its gate
/// requires (declaration order). Built once per module from the
/// analyzer's `host_fn_signatures` + `host_fn_gates`.
#[derive(Debug, Clone)]
pub(super) struct HostFnEntry {
    pub(super) param_tys: Vec<IrType>,
    pub(super) ret_ty: IrType,
    pub(super) required_bits: Vec<u32>,
}

/// Module-wide accumulator for `#native` imports. Shared (via
/// `Rc<RefCell<…>>`) across the entry body, every schema-method body,
/// and every lambda body so a native call anywhere in the module
/// interns into one [`Module::imports`] table with stable
/// `import_idx`es. Mirrors the sharing discipline of
/// [`ConstInternTables`] and the lambda slot table.
#[derive(Debug, Default)]
pub(super) struct NativeImportBuilder {
    /// Name → resolved signature/gate. Immutable after construction;
    /// populated only when the analyzer supplied host-fn metadata
    /// (the legacy single-file `analyze` path leaves it empty, so
    /// no source ever resolves a native call there).
    pub(super) resolved: HashMap<String, HostFnEntry>,
    /// Emitted imports in `import_idx` order.
    pub(super) imports: Vec<NativeImport>,
    /// Name → already-assigned `import_idx` (dedup across call sites).
    index_of: HashMap<String, u32>,
}

impl NativeImportBuilder {
    /// Resolve every host-fn signature the analyzer attached to `tree`
    /// into IR-ready form. Signatures outside the native-call type
    /// envelope (see [`type_node_to_ir_type`]) are dropped so a call
    /// to such a name still surfaces the stdlib-unknown error rather
    /// than a mis-typed import.
    pub(super) fn from_tree(tree: &AnalyzedTree) -> Self {
        let mut resolved = HashMap::new();
        for (name, sig) in &tree.host_fn_signatures {
            let mut param_tys = Vec::with_capacity(sig.params.len());
            let mut ok = true;
            for p in &sig.params {
                match type_node_to_ir_type(&p.ty) {
                    Some(ty) => param_tys.push(ty),
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok {
                continue;
            }
            let Some(ret_ty) = type_node_to_ir_type(&sig.return_type) else {
                continue;
            };
            let required_bits = tree
                .host_fn_gates
                .get(name)
                .map(|g| g.required_bit_indices())
                .unwrap_or_default();
            resolved.insert(
                name.clone(),
                HostFnEntry {
                    param_tys,
                    ret_ty,
                    required_bits,
                },
            );
        }
        // Fail-open guard (observability). A native whose *type signature*
        // the host declared but whose capability *gate* it never declared
        // lowers with an empty `required_bits` above → no `Op::CheckCap`
        // is emitted → the compiled call runs with no capability
        // requirement. That is correct for a genuinely pure fn and also
        // exactly what a forgotten gate looks like — but the two are now
        // distinguishable: `register_pure_fn`'s "this is pure" intent is
        // preserved through `Context::pure_fn_names` →
        // `AnalyzeOptions::host_fn_pure` → `tree.host_fn_pure`. So the
        // warning fires only for names that carry neither a gate nor a
        // purity declaration, no longer false-triggering on legitimately
        // pure fns. We still do NOT change lowering (no behavior change);
        // this stays a `warn!` for operators who want the fail-open
        // surfaced without opting into the hard
        // `require_declared_native_gates` gate (which the analyzer
        // enforces as an Error before lowering is ever reached). Emitting
        // is inert unless a subscriber is installed, so it adds no stderr
        // noise by default.
        for name in undeclared_gate_imports(&resolved, &tree.host_fn_gates, &tree.host_fn_pure) {
            tracing::warn!(
                native_fn = %name,
                "host declared a signature for native `{name}` but no capability gate; \
                 the compiled call will run with no capability requirement. If it is pure, \
                 register an empty `NativeFnGate` to make that explicit; if it touches \
                 files / network / clock / env / rng, register the matching gate so the \
                 runtime can enforce it."
            );
        }
        Self {
            resolved,
            imports: Vec::new(),
            index_of: HashMap::new(),
        }
    }

    /// Get-or-assign the `import_idx` for `name`. The import carries
    /// [`NO_CAPABILITY_BIT`]: the capability guard is emitted as
    /// dedicated `Op::CheckCap` ops (one per required bit) ahead of the
    /// call, so the cranelift backend keys the host-fn slot off
    /// `import_idx` and a multi-bit gate needs no single-bit encoding.
    pub(super) fn intern(&mut self, name: &str, entry: &HostFnEntry) -> u32 {
        if let Some(idx) = self.index_of.get(name) {
            return *idx;
        }
        let idx = self.imports.len() as u32;
        self.imports.push(NativeImport {
            name: name.to_string(),
            param_tys: entry.param_tys.clone(),
            ret_ty: entry.ret_ty,
            cap_bit: NO_CAPABILITY_BIT,
        });
        self.index_of.insert(name.to_string(), idx);
        idx
    }
}

/// Names of resolved native imports the host declared a *signature* for
/// but declared neither a capability *gate* (`host_fn_gates`) nor an
/// explicit *purity* marker (`host_fn_pure`). These lower with empty
/// `required_bits` and so run ungated in the compiled backends — the
/// fail-open the caller warns about. Sorted for deterministic
/// diagnostics/tests.
///
/// The rule is presence-of-key in either table:
///
/// * A present `host_fn_gates` entry (including an explicitly-empty
///   `NativeFnGate` — the `register_fn(name, NativeFnGate::default(), …)`
///   shape) means "declared, needs exactly these caps" → NOT reported.
/// * A present `host_fn_pure` entry — the `register_pure_fn` intent
///   mirrored through `AnalyzeOptions::host_fn_pure` — means "declared
///   pure, needs no cap" → NOT reported. This is what eliminates the
///   prior false-positive on legitimately pure fns.
///
/// Only a name absent from *both* tables — the shape of "host filled
/// `host_fn_signatures` but forgot to declare a gate" — is reported.
/// Generic over the gate value type so this leaf helper needs no
/// capability-type import.
pub(super) fn undeclared_gate_imports<G>(
    resolved: &HashMap<String, HostFnEntry>,
    gates: &HashMap<String, G>,
    pure: &HashSet<String>,
) -> Vec<String> {
    let mut out: Vec<String> = resolved
        .keys()
        .filter(|name| !gates.contains_key(*name) && !pure.contains(*name))
        .cloned()
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod undeclared_gate_import_tests {
    use super::*;

    fn entry() -> HostFnEntry {
        HostFnEntry {
            param_tys: Vec::new(),
            ret_ty: IrType::I64,
            required_bits: Vec::new(),
        }
    }

    #[test]
    fn signature_without_gate_or_purity_is_reported() {
        // Host declared a signature but neither a gate nor a purity
        // marker → under-declared (a forgotten gate looks exactly like
        // this shape).
        let mut resolved = HashMap::new();
        resolved.insert("read_net".to_string(), entry());
        let gates: HashMap<String, ()> = HashMap::new();
        let pure: HashSet<String> = HashSet::new();
        assert_eq!(
            undeclared_gate_imports(&resolved, &gates, &pure),
            vec!["read_net".to_string()]
        );
    }

    #[test]
    fn empty_gate_entry_declares_intent_and_is_silent() {
        // A present (even empty) gate entry records intent → NOT
        // reported.
        let mut resolved = HashMap::new();
        resolved.insert("pure_add".to_string(), entry());
        let mut gates: HashMap<String, ()> = HashMap::new();
        gates.insert("pure_add".to_string(), ());
        let pure: HashSet<String> = HashSet::new();
        assert!(undeclared_gate_imports(&resolved, &gates, &pure).is_empty());
    }

    #[test]
    fn declared_pure_via_purity_set_is_silent() {
        // The `register_pure_fn` intent, mirrored through
        // `host_fn_pure`: no gate entry at all, but the name is in the
        // purity set → NOT reported. This is the false-positive the
        // refinement eliminates.
        let mut resolved = HashMap::new();
        resolved.insert("pure_add".to_string(), entry());
        let gates: HashMap<String, ()> = HashMap::new();
        let mut pure: HashSet<String> = HashSet::new();
        pure.insert("pure_add".to_string());
        assert!(undeclared_gate_imports(&resolved, &gates, &pure).is_empty());
    }

    #[test]
    fn gated_effectful_fn_is_silent() {
        // A properly gated effectful fn has a present entry → silent.
        let mut resolved = HashMap::new();
        resolved.insert("clock_add".to_string(), entry());
        let mut gates: HashMap<String, ()> = HashMap::new();
        gates.insert("clock_add".to_string(), ());
        let pure: HashSet<String> = HashSet::new();
        assert!(undeclared_gate_imports(&resolved, &gates, &pure).is_empty());
    }

    #[test]
    fn report_is_sorted_and_only_covers_undeclared() {
        let mut resolved = HashMap::new();
        resolved.insert("zeta".to_string(), entry());
        resolved.insert("alpha".to_string(), entry());
        resolved.insert("declared".to_string(), entry());
        resolved.insert("pure_one".to_string(), entry());
        let mut gates: HashMap<String, ()> = HashMap::new();
        gates.insert("declared".to_string(), ());
        let mut pure: HashSet<String> = HashSet::new();
        pure.insert("pure_one".to_string());
        assert_eq!(
            undeclared_gate_imports(&resolved, &gates, &pure),
            vec!["alpha".to_string(), "zeta".to_string()]
        );
    }
}
