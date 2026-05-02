use crate::eval::Scope;
use ordered_float::OrderedFloat;
use relon_parser::Node;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(OrderedFloat<f64>),
    String(String),
    List(Vec<Value>),
    Dict(HashMap<String, Value>),
    /// A unified closure (can be used as a function or a decorator)
    #[serde(skip)]
    Closure {
        params: Vec<String>,
        body: Node,
        captured_env: Arc<Scope>,
    },
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
            Value::Dict(d) => !d.is_empty(),
            Value::Closure { .. } => true,
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
        }
    }
}
