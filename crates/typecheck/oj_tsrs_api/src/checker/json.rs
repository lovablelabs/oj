// Small JSON object builder mirroring Go encoding/json tags (`omitempty`, `omitzero`).

use tsrs_checker::LiteralValue;
use tsrs_core::json::{JsonMap, Value};

pub struct Obj {
    map: JsonMap,
}

impl Default for Obj {
    fn default() -> Obj {
        // Most response objects have a handful of fields: start with room for them instead of growing.
        Obj { map: JsonMap::with_capacity(8) }
    }
}

impl Obj {
    pub(crate) fn new() -> Obj {
        Obj::default()
    }

    /// Response field names are literals, which the map borrows.
    pub fn set(&mut self, key: &'static str, value: Value) {
        self.map.insert(key, value);
    }

    pub(crate) fn num(&mut self, key: &'static str, n: f64) {
        self.set(key, Value::Number(n));
    }

    /// `omitzero` numbers (encoding/json v2 `omitempty` would still emit 0).
    pub(crate) fn nonzero(&mut self, key: &'static str, n: f64) {
        if n != 0.0 {
            self.num(key, n);
        }
    }

    /// `omitempty` strings ("" is an empty JSON value under encoding/json v2).
    pub(crate) fn str_nonempty(&mut self, key: &'static str, s: &str) {
        if !s.is_empty() {
            self.set(key, Value::String(s.to_string()));
        }
    }

    /// `omitempty` id slices ([] is an empty JSON value under encoding/json v2).
    pub(crate) fn ids(&mut self, key: &'static str, ids: impl Iterator<Item = f64>) {
        let ids: Vec<Value> = ids.map(Value::Number).collect();
        if !ids.is_empty() {
            self.set(key, Value::Array(ids));
        }
    }

    pub(crate) fn extend(&mut self, other: Obj) {
        self.map.append(other.map);
    }

    pub fn build(self) -> Value {
        Value::Object(self.map)
    }

    /// Emits the fields in `order` (unknown fields keep their relative order at the end).
    pub(crate) fn build_ordered(mut self, order: &[&str]) -> Value {
        // A stable sort: listed fields by their position in `order`, then the others as they were.
        self.map.sort_by_cached_key(|key, _| {
            order.iter().position(|o| *o == key).unwrap_or(order.len())
        });
        Value::Object(self.map)
    }
}

pub fn obj() -> Obj {
    Obj::new()
}

/// Go `literalValueToJSON`.
pub(crate) fn literal_value_to_json(value: Option<LiteralValue>) -> Value {
    match value {
        Some(LiteralValue::String(s)) => Value::String(s.to_string()),
        Some(LiteralValue::Number(n)) => {
            let f = n.0;
            if f.is_infinite() {
                Value::String(if f > 0.0 { "+Infinity" } else { "-Infinity" }.to_string())
            } else if f.is_nan() {
                Value::String("NaN".to_string())
            } else {
                Value::Number(f)
            }
        }
        Some(LiteralValue::Boolean(b)) => Value::Bool(b),
        Some(LiteralValue::BigInt(b)) => Value::String(b.to_string()),
        None => Value::Null,
    }
}
