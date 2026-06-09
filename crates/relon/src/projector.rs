//! Pluggable [`Value`] → host-format projection.
//!
//! `to_json_value` historically hard-coded the `Value` → `serde_json::Value`
//! shape. The [`Projector`] trait inverts that: hosts implement it once for
//! their target representation (JSON, YAML, BSON, a typed builder, …) and
//! plug it into [`crate::project_with`] / [`crate::project_from_str`].
//!
//! Implementors recurse through [`Value::List`] / [`Value::Tuple`] /
//! [`Value::Dict`]
//! themselves; the trait deliberately doesn't bake in a visitor walk so
//! exotic projectors (streaming serializers, custom error policies, …) keep
//! full control over traversal order and short-circuiting.

use relon_evaluator::Value;

/// Strategy for converting an evaluated [`Value`] into a host
/// representation. See module docs.
pub trait Projector {
    /// The host representation produced by a successful projection.
    type Output;
    /// Error type surfaced when projection fails (e.g. encountering an
    /// unsupported variant or a non-finite float).
    type Error;

    /// Project a single [`Value`] tree into [`Self::Output`].
    fn project(&self, value: &Value) -> Result<Self::Output, Self::Error>;
}

/// Default projector that mirrors the `to_json_value` behavior shipped in
/// pre-trait versions: closures/schemas/types/wildcards inside dicts are
/// dropped, top-level closures/schemas/types/wildcards are errors, and
/// non-finite floats are errors.
pub struct JsonProjector;

impl Projector for JsonProjector {
    type Output = serde_json::Value;
    type Error = crate::Error;

    fn project(&self, value: &Value) -> Result<Self::Output, Self::Error> {
        match value {
            Value::Bool(b) => Ok(serde_json::Value::Bool(*b)),
            Value::Int(i) => Ok(serde_json::Value::Number((*i).into())),
            Value::Float(f) => {
                let raw = f.into_inner();
                serde_json::Number::from_f64(raw)
                    .map(serde_json::Value::Number)
                    .ok_or(crate::Error::NonFiniteFloat(raw))
            }
            Value::String(s) => Ok(serde_json::Value::String(s.as_str().to_owned())),
            Value::List(items) | Value::Tuple(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items.iter() {
                    out.push(self.project(item)?);
                }
                Ok(serde_json::Value::Array(out))
            }
            Value::Dict(d) => {
                if value.is_option_none() {
                    return Ok(serde_json::Value::Null);
                }
                if let Some(inner) = value.option_some_value() {
                    return self.project(inner);
                }
                let tuple_variant_len = if d.variant_of.is_some() && !d.map.is_empty() {
                    let mut indexes = Vec::with_capacity(d.map.len());
                    let mut tuple_like = true;
                    for key in d.map.keys() {
                        match key.as_str().parse::<usize>() {
                            Ok(index) => indexes.push(index),
                            Err(_) => {
                                tuple_like = false;
                                break;
                            }
                        }
                    }
                    if tuple_like {
                        indexes.sort_unstable();
                        indexes
                            .iter()
                            .enumerate()
                            .all(|(expected, actual)| expected == *actual)
                            .then_some(indexes.len())
                    } else {
                        None
                    }
                } else {
                    None
                };

                let inner = if let Some(len) = tuple_variant_len {
                    let mut out = Vec::with_capacity(len);
                    for idx in 0..len {
                        if let Some(value) = d.map.get(idx.to_string().as_str()) {
                            out.push(self.project(value)?);
                        }
                    }
                    serde_json::Value::Array(out)
                } else {
                    let mut map = serde_json::Map::new();
                    for (key, val) in d.map.iter() {
                        if matches!(
                            val,
                            Value::Closure(_)
                                | Value::Schema(_)
                                | Value::EnumSchema(_)
                                | Value::Type(_)
                                | Value::Wildcard
                        ) {
                            // These variants have no JSON analogue; silently
                            // dropping them keeps internal helpers (closures
                            // used as decorators, schemas defined for
                            // validation) from polluting serialized output.
                            continue;
                        }
                        map.insert(key.as_str().to_owned(), self.project(val)?);
                    }
                    serde_json::Value::Object(map)
                };
                // Externally-tagged sum-type variant: wrap as
                // `{ VariantName: ...payload... }` only when the dict
                // originated from a tagged-enum constructor. Struct variants
                // use an object payload; tuple variants use an array payload.
                if let (Some(_), Some(brand)) = (d.variant_of.as_ref(), d.brand.as_ref()) {
                    let mut wrapper = serde_json::Map::new();
                    wrapper.insert(brand.clone(), inner);
                    Ok(serde_json::Value::Object(wrapper))
                } else {
                    Ok(inner)
                }
            }
            Value::Closure(_) => Err(crate::Error::UnsupportedClosure),
            Value::Schema(_) | Value::EnumSchema(_) | Value::Type(_) | Value::Wildcard => {
                Err(crate::Error::UnsupportedSchema)
            }
        }
    }
}
