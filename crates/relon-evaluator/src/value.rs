use crate::eval::Scope;
use ordered_float::OrderedFloat;
use relon_parser::Node;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct ValueDict {
    pub map: BTreeMap<String, Value>,
    pub brand: Option<String>,
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
        Ok(ValueDict { map, brand: None })
    }
}

impl PartialEq for ValueDict {
    fn eq(&self, other: &Self) -> bool {
        self.map == other.map && self.brand == other.brand
    }
}

#[derive(Debug, PartialEq, Clone)]
pub struct SchemaField {
    pub type_hint: relon_parser::TypeNode,
    pub predicate: Value,
    pub custom_error: Option<String>,
    pub default_value: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(OrderedFloat<f64>),
    String(String),
    List(Vec<Value>),
    Dict(ValueDict),
    /// A unified closure (can be used as a function or a decorator)
    #[serde(skip)]
    Closure {
        params: Vec<String>,
        body: Node,
        captured_env: Arc<Scope>,
    },
    /// A user-defined type schema: Key -> SchemaField
    #[serde(skip)]
    Schema(std::collections::HashMap<String, SchemaField>),
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
            (Self::Schema(_), Self::Schema(_)) => false,
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
            Value::Schema(_) => true,
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
            Value::Schema(_) => "Schema",
            Value::Type(_) => "Type",
            Value::Wildcard => "Wildcard",
        }
    }

    pub fn deep_merge(&mut self, patch: &Value) {
        match (self, patch) {
            (Value::Dict(base), Value::Dict(patch)) => {
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
            (Value::Schema(base_fields), Value::Schema(patch_fields)) => {
                for (k, v) in patch_fields {
                    if let Some(base_field) = base_fields.get_mut(k) {
                        base_field.type_hint = v.type_hint.clone();
                        base_field.predicate = v.predicate.clone();
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
            (Value::Schema(base_fields), Value::Dict(patch_data)) => {
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
