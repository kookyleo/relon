use crate::scope::Scope;
use crate::smol_str::SmolStr;
use ordered_float::OrderedFloat;
use relon_parser::Node;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct ValueDict {
    pub map: BTreeMap<String, Value>,
    pub brand: Option<String>,
    /// Name of the parent sum-type Enum when this dict is a tagged-enum
    /// variant. `Some("Notification")` distinguishes a `Notification.Email`
    /// payload from a plain `#schema Email { ... }` value (both have
    /// `brand = Some("Email")`); the JSON serializer uses it to wrap the
    /// payload as `{ Email: { ... } }` only for the variant case.
    pub variant_of: Option<String>,
}

impl ValueDict {
    pub fn new(map: BTreeMap<String, Value>) -> Self {
        Self {
            map,
            brand: None,
            variant_of: None,
        }
    }

    pub fn with_brand(map: BTreeMap<String, Value>, brand: Option<String>) -> Self {
        Self {
            map,
            brand,
            variant_of: None,
        }
    }
}

impl Serialize for ValueDict {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.map.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ValueDict {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let map = BTreeMap::deserialize(deserializer)?;
        Ok(ValueDict {
            map,
            brand: None,
            variant_of: None,
        })
    }
}

impl PartialEq for ValueDict {
    fn eq(&self, other: &Self) -> bool {
        self.map == other.map && self.brand == other.brand && self.variant_of == other.variant_of
    }
}

#[derive(Debug, PartialEq, Clone)]
pub struct SchemaField {
    pub type_hint: relon_parser::TypeNode,
    /// Predicates that the field's value must satisfy.
    ///
    /// Multiple predicates are AND-combined at validation time. `Wildcard`
    /// entries are skipped. Stored as a `Vec` (rather than a single `Value`)
    /// so `Schema + Schema` composition can accumulate constraints from both
    /// sides instead of letting the right-hand operand silently overwrite the
    /// left.
    pub predicates: Vec<Value>,
    pub custom_error: Option<String>,
    pub default_value: Option<Value>,
}

/// Inline payload for `Value::Closure`, boxed out of the enum so the
/// `Value` discriminant width is governed by the cheap variants. The
/// closure body (`Node`) plus captured scope dwarf the other variants
/// by tens of bytes — keeping them inline widens every `Value` on the
/// stack, every HashMap bucket holding `Value`, and every list slot.
#[derive(Debug, Clone)]
pub struct ClosureData {
    pub params: Vec<String>,
    pub body: Node,
    pub captured_env: Arc<Scope>,
}

/// Inline payload for `Value::Schema`, boxed for the same reason as
/// [`ClosureData`]: the inner `HashMap<String, SchemaField>` keeps a
/// raw-table header that pushes the enum width into the >100-byte
/// range when stored inline.
#[derive(Debug, Clone)]
pub struct SchemaData {
    pub generics: Vec<String>,
    pub fields: std::collections::HashMap<String, SchemaField>,
}

/// Inline payload for `Value::EnumSchema`, boxed for the same reason:
/// the nested `HashMap<String, HashMap<String, SchemaField>>` is the
/// largest variant we hold today, and box-indirection collapses it to
/// a single pointer in the enum layout.
#[derive(Debug, Clone)]
pub struct EnumSchemaData {
    pub name: String,
    pub generics: Vec<String>,
    pub variants: std::collections::HashMap<String, std::collections::HashMap<String, SchemaField>>,
}

/// Aggregate value type produced by the evaluator.
///
/// `List` and `Dict` payloads are reference-counted: cloning a `Value::List`
/// or `Value::Dict` only bumps an `Arc` and does not copy the underlying
/// collection. Mutations go through `Arc::make_mut` (see [`Value::list_mut`]
/// and [`Value::dict_mut`]), which clones the inner value lazily on first
/// write — so existing aliases keep their snapshot semantics. This matters
/// because the evaluator caches resolved paths and module results in shared
/// `path_cache`/`module_cache` maps; without `Arc`-sharing every cache hit
/// would deep-clone the cached structure.
///
/// The "heavy" variants (`Closure`, `Schema`, `EnumSchema`) are boxed so
/// the enum stays narrow: the comprehension hot loop stores `Value`s in
/// per-iteration scope HashMaps, and the bucket size scales with the enum
/// width. Boxing trades one allocation at construction (already a cold
/// path: closures / schemas materialise once and are reused) for a much
/// cheaper hash table grow path on every iteration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(OrderedFloat<f64>),
    /// Short-string-optimized: ≤ 22 byte payloads inline in the value
    /// slot (no heap alloc), longer payloads ride a refcounted
    /// `Arc<str>` so clones stay O(1). See [`SmolStr`].
    String(SmolStr),
    List(Arc<Vec<Value>>),
    Dict(Arc<ValueDict>),
    /// A unified closure (can be used as a function or a decorator).
    /// Payload is boxed; see [`ClosureData`].
    #[serde(skip)]
    Closure(Box<ClosureData>),
    /// A user-defined type schema. Payload is boxed; see [`SchemaData`].
    #[serde(skip)]
    Schema(Box<SchemaData>),
    /// A tagged-enum (sum-type) schema: variants by name, each with its
    /// own field set. Built from `#schema Name: Enum<Var1 { ... }, ...>`.
    /// Construction with `Name.Var1 { ... }` is dispatched via this value.
    /// Payload is boxed; see [`EnumSchemaData`].
    #[serde(skip)]
    EnumSchema(Box<EnumSchemaData>),
    /// A single type description. The payload (`TypeNode`) carries a
    /// `TokenRange` plus a `Vec<TypeNode>` of generics that together push
    /// the inline size past 100 bytes; boxing keeps the enum compact
    /// (matching the rationale for [`ClosureData`] et al.).
    #[serde(skip)]
    Type(Box<relon_parser::TypeNode>),
    /// A wildcard predicate (*)
    Wildcard,
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Null, Self::Null) => true,
            (Self::Bool(l), Self::Bool(r)) => l == r,
            (Self::Int(l), Self::Int(r)) => l == r,
            (Self::Float(l), Self::Float(r)) => l == r,
            (Self::String(l), Self::String(r)) => l == r,
            (Self::List(l), Self::List(r)) => l == r,
            (Self::Dict(l), Self::Dict(r)) => l == r,
            (Self::Schema(_), Self::Schema(_)) => false,
            (Self::EnumSchema(_), Self::EnumSchema(_)) => false,
            (Self::Type(l), Self::Type(r)) => l == r,
            (Self::Wildcard, Self::Wildcard) => true,
            (Self::Closure(a), Self::Closure(b)) => {
                a.params == b.params
                    && a.body == b.body
                    && Arc::ptr_eq(&a.captured_env, &b.captured_env)
            }
            _ => false,
        }
    }
}

impl Value {
    /// Build a `Value::List` from a `Vec`, taking ownership and wrapping it
    /// in `Arc` so subsequent clones are O(1).
    pub fn list(items: Vec<Value>) -> Self {
        Self::List(Arc::new(items))
    }

    /// Build a `Value::Dict` from a `BTreeMap`. Use [`Value::branded_dict`]
    /// when the dict carries a nominal-type brand.
    pub fn dict(map: BTreeMap<String, Value>) -> Self {
        Self::Dict(Arc::new(ValueDict {
            map,
            brand: None,
            variant_of: None,
        }))
    }

    /// Build a `Value::Dict` with an explicit brand (the typed-dict tag set
    /// after a successful `User x: { ... }` validation, etc.).
    pub fn branded_dict(map: BTreeMap<String, Value>, brand: Option<String>) -> Self {
        Self::Dict(Arc::new(ValueDict {
            map,
            brand,
            variant_of: None,
        }))
    }

    /// Build a `Value::Dict` representing a tagged-enum variant: carries a
    /// `brand` (the variant name) plus `variant_of` (the parent enum name).
    /// The JSON projector uses `variant_of` to externally tag the output.
    pub fn variant_dict(map: BTreeMap<String, Value>, variant: String, enum_name: String) -> Self {
        Self::Dict(Arc::new(ValueDict {
            map,
            brand: Some(variant),
            variant_of: Some(enum_name),
        }))
    }

    /// In-place mutable handle to a `Value::List`'s inner `Vec`. Clones the
    /// inner allocation only if the `Arc` is shared with another holder.
    /// Returns `None` for non-list values.
    pub fn list_mut(&mut self) -> Option<&mut Vec<Value>> {
        match self {
            Value::List(arc) => Some(Arc::make_mut(arc)),
            _ => None,
        }
    }

    /// In-place mutable handle to a `Value::Dict`'s inner [`ValueDict`].
    /// CoW semantics — see [`Value::list_mut`].
    pub fn dict_mut(&mut self) -> Option<&mut ValueDict> {
        match self {
            Value::Dict(arc) => Some(Arc::make_mut(arc)),
            _ => None,
        }
    }

    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Int(i) => *i != 0,
            Value::Float(f) => f.into_inner() != 0.0,
            Value::String(s) => !s.is_empty(),
            Value::List(l) => !l.is_empty(),
            Value::Dict(d) => !d.map.is_empty(),
            Value::Closure(_) => true,
            Value::Schema(_) => true,
            Value::EnumSchema(_) => true,
            Value::Type(_) => true,
            Value::Wildcard => true,
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "Null",
            Value::Bool(_) => "Bool",
            Value::Int(_) => "Int",
            Value::Float(_) => "Float",
            Value::String(_) => "String",
            Value::List(_) => "List",
            Value::Dict(_) => "Dict",
            Value::Closure(_) => "Closure",
            Value::Schema(_) => "Schema",
            Value::EnumSchema(_) => "EnumSchema",
            Value::Type(_) => "Type",
            Value::Wildcard => "Wildcard",
        }
    }

    pub fn deep_merge(&mut self, patch: &Value) {
        match (self, patch) {
            (Value::Dict(base), Value::Dict(patch)) => {
                let base = Arc::make_mut(base);
                for (k, v) in &patch.map {
                    if v == &Value::Null {
                        base.map.remove(k);
                    } else if let Some(base_val) = base.map.get_mut(k) {
                        base_val.deep_merge(v);
                    } else {
                        base.map.insert(k.clone(), v.clone());
                    }
                }
            }
            (Value::Schema(base), Value::Schema(patch)) => {
                let base_fields = &mut base.fields;
                let patch_fields = &patch.fields;
                for (k, v) in patch_fields {
                    if let Some(base_field) = base_fields.get_mut(k) {
                        base_field.type_hint = v.type_hint.clone();
                        // AND-merge predicates rather than overwrite, mirroring
                        // the static `extract_schema_for_node` composition path.
                        for pred in &v.predicates {
                            if !matches!(pred, Value::Wildcard) {
                                base_field.predicates.push(pred.clone());
                            }
                        }
                        if v.custom_error.is_some() {
                            base_field.custom_error = v.custom_error.clone();
                        }
                        if v.default_value.is_some() {
                            base_field.default_value = v.default_value.clone();
                        }
                    } else {
                        base_fields.insert(k.clone(), v.clone());
                    }
                }
            }
            (Value::Schema(base), Value::Dict(patch_data)) => {
                let base_fields = &mut base.fields;
                for (k, v) in &patch_data.map {
                    if let Some(base_field) = base_fields.get_mut(k) {
                        base_field.default_value = Some(v.clone());
                    }
                }
            }
            (b, p) => *b = p.clone(),
        }
    }
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{}", b),
            Value::Int(i) => write!(f, "{}", i),
            Value::Float(fl) => write!(f, "{}", fl),
            Value::String(s) => write!(f, "{}", s),
            Value::List(l) => write!(f, "{:?}", l),
            Value::Dict(d) => write!(f, "{:?}", d.map),
            Value::Closure(_) => write!(f, "<closure>"),
            Value::Schema(_) => write!(f, "<schema>"),
            Value::EnumSchema(enum_data) => write!(f, "<enum {}>", enum_data.name),
            Value::Type(t) => write!(f, "Type<{}>", relon_analyzer::format_type(t)),
            Value::Wildcard => write!(f, "*"),
        }
    }
}

#[cfg(test)]
mod size_guard {
    use super::Value;

    /// Hard ceiling on `Value` enum width. The comprehension hot loop
    /// stores `Value`s in per-iteration scope HashMaps; bucket size scales
    /// with the enum width, so a regression here translates directly into
    /// MB-scale waste on the comprehension workload (dhat profile attributes
    /// it to `HashMap::insert`'s grow path). 48 bytes leaves headroom for
    /// the existing `String(String)` (24 B) + 1-byte tag, plus a couple of
    /// future smallvec / cow-string tweaks before we have to rebox.
    #[test]
    fn value_enum_is_compact() {
        let size = std::mem::size_of::<Value>();
        eprintln!("Value enum size: {} bytes", size);
        assert!(size <= 48, "Value enum grew: {} bytes", size);
    }
}
