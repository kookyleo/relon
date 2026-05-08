use crate::scope::Scope;
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(OrderedFloat<f64>),
    String(String),
    List(Arc<Vec<Value>>),
    Dict(Arc<ValueDict>),
    /// A unified closure (can be used as a function or a decorator)
    #[serde(skip)]
    Closure {
        params: Vec<String>,
        body: Node,
        captured_env: Arc<Scope>,
    },
    /// A user-defined type schema: generic params and Key -> SchemaField map
    #[serde(skip)]
    Schema {
        generics: Vec<String>,
        fields: std::collections::HashMap<String, SchemaField>,
    },
    /// A tagged-enum (sum-type) schema: variants by name, each with its
    /// own field set. Built from `#schema Name: Enum<Var1 { ... }, ...>`.
    /// Construction with `Name.Var1 { ... }` is dispatched via this value.
    #[serde(skip)]
    EnumSchema {
        name: String,
        variants: std::collections::HashMap<String, std::collections::HashMap<String, SchemaField>>,
    },
    /// A single type description
    #[serde(skip)]
    Type(relon_parser::TypeNode),
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
            (Self::Schema { .. }, Self::Schema { .. }) => false,
            (Self::EnumSchema { .. }, Self::EnumSchema { .. }) => false,
            (Self::Type(l), Self::Type(r)) => l == r,
            (Self::Wildcard, Self::Wildcard) => true,
            (
                Self::Closure {
                    params: p1,
                    body: b1,
                    captured_env: c1,
                },
                Self::Closure {
                    params: p2,
                    body: b2,
                    captured_env: c2,
                },
            ) => p1 == p2 && b1 == b2 && Arc::ptr_eq(c1, c2),
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
            Value::Closure { .. } => true,
            Value::Schema { .. } => true,
            Value::EnumSchema { .. } => true,
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
            Value::Closure { .. } => "Closure",
            Value::Schema { .. } => "Schema",
            Value::EnumSchema { .. } => "EnumSchema",
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
            (
                Value::Schema {
                    fields: base_fields,
                    ..
                },
                Value::Schema {
                    fields: patch_fields,
                    ..
                },
            ) => {
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
            (
                Value::Schema {
                    fields: base_fields,
                    ..
                },
                Value::Dict(patch_data),
            ) => {
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
