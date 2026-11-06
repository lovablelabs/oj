use std::fmt;
use std::ops::Deref;

use tsrs_core::collections::{OrderedMap, new_ordered_map_with_size_hint};

use super::json::Json;

#[repr(i8)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum JSONValueType {
    #[default]
    NotPresent,
    Null,
    String,
    Number,
    Boolean,
    Array,
    Object,
}

impl fmt::Display for JSONValueType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JSONValueType::Null => f.write_str("null"),
            JSONValueType::String => f.write_str("string"),
            JSONValueType::Number => f.write_str("number"),
            JSONValueType::Boolean => f.write_str("boolean"),
            JSONValueType::Array => f.write_str("array"),
            JSONValueType::Object => f.write_str("object"),
            JSONValueType::NotPresent => write!(f, "unknown({})", *self as i8),
        }
    }
}

// Go's `JSONValue{Type, Value any}`; `T` is the element type of arrays and objects (`JSONValue` or
// `ExportsOrImports`, like Go's generic `unmarshalJSONValueFrom[T]`).
#[derive(Clone, Debug, Default, PartialEq)]
pub enum JSONData<T> {
    #[default]
    NotPresent,
    Null,
    String(String),
    Number(f64),
    Boolean(bool),
    Array(Vec<T>),
    Object(OrderedMap<String, T>),
}

impl<T> JSONData<T> {
    pub fn type_(&self) -> JSONValueType {
        match self {
            JSONData::NotPresent => JSONValueType::NotPresent,
            JSONData::Null => JSONValueType::Null,
            JSONData::String(_) => JSONValueType::String,
            JSONData::Number(_) => JSONValueType::Number,
            JSONData::Boolean(_) => JSONValueType::Boolean,
            JSONData::Array(_) => JSONValueType::Array,
            JSONData::Object(_) => JSONValueType::Object,
        }
    }

    pub fn is_present(&self) -> bool {
        self.type_() != JSONValueType::NotPresent
    }

    pub fn is_falsy(&self) -> bool {
        match self {
            JSONData::NotPresent | JSONData::Null => true,
            JSONData::String(s) => s.is_empty(),
            JSONData::Boolean(b) => !*b,
            // Go compares the decoded `float64` against the untyped constant `0` boxed as an `int`
            // (`v.Value == 0`), which is never equal, so numbers are never falsy.
            _ => false,
        }
    }

    pub fn as_object(&self) -> &OrderedMap<String, T> {
        match self {
            JSONData::Object(o) => o,
            _ => panic!("expected object, got {}", self.type_()),
        }
    }

    pub fn as_array(&self) -> &[T] {
        match self {
            JSONData::Array(a) => a,
            _ => panic!("expected array, got {}", self.type_()),
        }
    }

    pub fn as_string(&self) -> &str {
        match self {
            JSONData::String(s) => s,
            _ => panic!("expected string, got {}", self.type_()),
        }
    }

    pub(crate) fn from_json(value: &Json, element: impl Fn(&Json) -> T + Copy) -> JSONData<T> {
        match value {
            Json::Null => JSONData::Null,
            Json::String(s) => JSONData::String(s.clone()),
            Json::Array(elements) => JSONData::Array(elements.iter().map(element).collect()),
            Json::Object(members) => {
                let mut object = new_ordered_map_with_size_hint(members.len());
                object.extend(members.iter().map(|(name, member)| {
                    // OrderedMap.Set keeps the first position of a repeated name and takes the last value.
                    (name.clone(), element(member))
                }));
                JSONData::Object(object)
            }
            Json::Bool(b) => JSONData::Boolean(*b),
            Json::Number(n) => JSONData::Number(*n),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct JSONValue {
    pub data: JSONData<JSONValue>,
}

impl JSONValue {
    pub fn from_json(value: &Json) -> JSONValue {
        JSONValue { data: JSONData::from_json(value, JSONValue::from_json) }
    }
}

impl Deref for JSONValue {
    type Target = JSONData<JSONValue>;
    fn deref(&self) -> &JSONData<JSONValue> {
        &self.data
    }
}
