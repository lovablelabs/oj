use crate::fxhash::FxHashMap;
use std::sync::Arc;

use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum EvalValue {
    Null,
    Undefined,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<EvalValue>),
    /// Rc-shared: clones are refcount bumps; mutation goes through make_mut.
    Obj(Arc<JsObjectMap>),
}

impl EvalValue {
    // parity: JSON.stringify — undefined props drop, undefined array elements
    // and non-finite numbers become null.
    pub fn to_json(&self) -> Value {
        match self {
            EvalValue::Null | EvalValue::Undefined => Value::Null,
            EvalValue::Bool(b) => Value::Bool(*b),
            EvalValue::Num(n) => num_to_json(*n),
            EvalValue::Str(s) => Value::String(s.clone()),
            EvalValue::Arr(items) => Value::Array(items.iter().map(EvalValue::to_json).collect()),
            EvalValue::Obj(map) => map.to_json(),
        }
    }

    /// Object key order follows the `Value` map's own iteration order
    /// (alphabetical without serde_json's `preserve_order`); index keys re-sort anyway.
    pub fn from_json(value: &Value) -> Self {
        match value {
            Value::Null => EvalValue::Null,
            Value::Bool(b) => EvalValue::Bool(*b),
            Value::Number(n) => EvalValue::Num(n.as_f64().unwrap_or(f64::NAN)),
            Value::String(s) => EvalValue::Str(s.clone()),
            Value::Array(items) => EvalValue::Arr(items.iter().map(Self::from_json).collect()),
            Value::Object(map) => {
                let mut obj = JsObjectMap::new();
                for (k, v) in map {
                    obj.insert(k.clone(), Self::from_json(v));
                }
                EvalValue::Obj(Arc::new(obj))
            }
        }
    }
}

fn num_to_json(n: f64) -> Value {
    if !n.is_finite() {
        return Value::Null;
    }
    if n.fract() == 0.0 && n.abs() < (i64::MAX as f64) {
        return Value::Number((n as i64).into());
    }
    serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number)
}

/// String-keyed map iterating in ES OwnPropertyKeys order: canonical array-index
/// keys ascending first, then the remaining keys in insertion order.
#[derive(Clone, Default)]
pub struct JsObjectMap {
    index_entries: Vec<(u32, String, EvalValue)>,
    named_entries: Vec<(String, EvalValue)>,
    /// Lazy key→position table so wide objects avoid quadratic scans.
    // Boxed to keep the usually-None field one word; the map sits inline in EvalValue.
    #[allow(clippy::box_collection)]
    named_index: Option<Box<FxHashMap<String, usize>>>,
    /// CSSType `instanceof` brand (the syntax) — representation, not data:
    /// never enumerated, serialized, or copied by spread.
    css_type: Option<String>,
}

// Manual Debug: the lazy name index is derived state with randomized HashMap
// order, and the cache key fingerprints this repr — it must stay canonical.
impl std::fmt::Debug for JsObjectMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsObjectMap")
            .field("index_entries", &self.index_entries)
            .field("named_entries", &self.named_entries)
            .field("css_type", &self.css_type())
            .finish()
    }
}

impl PartialEq for JsObjectMap {
    fn eq(&self, other: &Self) -> bool {
        self.index_entries == other.index_entries
            && self.named_entries == other.named_entries
            && self.css_type == other.css_type
    }
}

pub(crate) const NAMED_INDEX_THRESHOLD: usize = 32;

// parity: ES ArrayIndex — ToString(ToUint32(key)) == key and key != 2^32 - 1,
// i.e. the canonical decimal form of an integer in [0, 2^32 - 1).
pub fn array_index(key: &str) -> Option<u32> {
    let bytes = key.as_bytes();
    if bytes.is_empty() || bytes.len() > 10 || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    if bytes.len() > 1 && bytes[0] == b'0' {
        return None;
    }
    let n: u64 = key.parse().ok()?;
    if n >= u64::from(u32::MAX) {
        return None;
    }
    Some(n as u32)
}

impl JsObjectMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(named: usize) -> Self {
        Self {
            named_entries: Vec::with_capacity(named),
            ..Self::default()
        }
    }

    /// The map sequential `insert` would build from `entries`, which must not
    /// repeat a key (callers hand over another map's or object's entries).
    pub fn from_unique_entries(entries: Vec<(String, EvalValue)>) -> Self {
        debug_assert!(
            entries
                .iter()
                .map(|(k, _)| k.as_str())
                .collect::<crate::fxhash::FxHashSet<_>>()
                .len()
                == entries.len()
        );
        if !entries.iter().any(|(k, _)| array_index(k).is_some()) {
            return Self {
                named_entries: entries,
                ..Self::default()
            };
        }
        let mut index_entries = Vec::new();
        let mut named_entries = Vec::with_capacity(entries.len() - 1);
        for (key, value) in entries {
            match array_index(&key) {
                Some(n) => index_entries.push((n, key, value)),
                None => named_entries.push((key, value)),
            }
        }
        index_entries.sort_unstable_by_key(|e| e.0);
        Self {
            index_entries,
            named_entries,
            ..Self::default()
        }
    }

    pub fn len(&self) -> usize {
        self.index_entries.len() + self.named_entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index_entries.is_empty() && self.named_entries.is_empty()
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub fn get(&self, key: &str) -> Option<&EvalValue> {
        if let Some(n) = array_index(key) {
            self.index_entries
                .binary_search_by_key(&n, |e| e.0)
                .ok()
                .map(|i| &self.index_entries[i].2)
        } else if let Some(index) = &self.named_index {
            index.get(key).map(|&i| &self.named_entries[i].1)
        } else {
            self.named_entries
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v)
        }
    }

    /// Position of `key` in `entries()` order.
    pub fn position(&self, key: &str) -> Option<usize> {
        if let Some(n) = array_index(key) {
            return self.index_entries.binary_search_by_key(&n, |e| e.0).ok();
        }
        let named = match &self.named_index {
            Some(index) => index.get(key).copied(),
            None => self.named_entries.iter().position(|(k, _)| k == key),
        };
        named.map(|i| i + self.index_entries.len())
    }

    /// The entry at a `position()` / `entries()` index.
    pub fn entry_at(&self, i: usize) -> (&str, &EvalValue) {
        let indexed = self.index_entries.len();
        if i < indexed {
            let (_, k, v) = &self.index_entries[i];
            (k, v)
        } else {
            let (k, v) = &self.named_entries[i - indexed];
            (k, v)
        }
    }

    fn named_position(&mut self, key: &str) -> Option<usize> {
        if self.named_index.is_none() && self.named_entries.len() >= NAMED_INDEX_THRESHOLD {
            self.named_index = Some(Box::new(
                self.named_entries
                    .iter()
                    .enumerate()
                    .map(|(i, (k, _))| (k.clone(), i))
                    .collect(),
            ));
        }
        match &self.named_index {
            Some(index) => index.get(key).copied(),
            None => self.named_entries.iter().position(|(k, _)| k == key),
        }
    }

    /// Returns the previous value when overwriting; an overwrite keeps the key's position.
    pub fn insert(&mut self, key: impl Into<String>, value: EvalValue) -> Option<EvalValue> {
        let key = key.into();
        if let Some(n) = array_index(&key) {
            match self.index_entries.binary_search_by_key(&n, |e| e.0) {
                Ok(i) => Some(std::mem::replace(&mut self.index_entries[i].2, value)),
                Err(i) => {
                    self.index_entries.insert(i, (n, key, value));
                    None
                }
            }
        } else if let Some(i) = self.named_position(&key) {
            Some(std::mem::replace(&mut self.named_entries[i].1, value))
        } else {
            if let Some(index) = &mut self.named_index {
                index.insert(key.clone(), self.named_entries.len());
            }
            self.named_entries.push((key, value));
            None
        }
    }

    pub fn remove(&mut self, key: &str) -> Option<EvalValue> {
        if let Some(n) = array_index(key) {
            match self.index_entries.binary_search_by_key(&n, |e| e.0) {
                Ok(i) => Some(self.index_entries.remove(i).2),
                Err(_) => None,
            }
        } else {
            let pos = self.named_position(key)?;
            if let Some(index) = &mut self.named_index {
                index.remove(key);
                for i in index.values_mut() {
                    if *i > pos {
                        *i -= 1;
                    }
                }
            }
            Some(self.named_entries.remove(pos).1)
        }
    }

    pub fn entries(&self) -> impl Iterator<Item = (&str, &EvalValue)> {
        self.index_entries
            .iter()
            .map(|(_, k, v)| (k.as_str(), v))
            .chain(self.named_entries.iter().map(|(k, v)| (k.as_str(), v)))
    }

    pub fn into_entries(self) -> impl Iterator<Item = (String, EvalValue)> {
        self.index_entries
            .into_iter()
            .map(|(_, k, v)| (k, v))
            .chain(self.named_entries)
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries().map(|(k, _)| k)
    }

    pub fn css_type(&self) -> Option<&str> {
        self.css_type.as_deref()
    }

    pub fn set_css_type(&mut self, syntax: String) {
        self.css_type = Some(syntax);
    }

    pub fn to_json(&self) -> Value {
        let mut map = serde_json::Map::new();
        for (k, v) in self.entries() {
            if matches!(v, EvalValue::Undefined) {
                continue;
            }
            map.insert(k.to_string(), v.to_json());
        }
        Value::Object(map)
    }
}

impl<K: Into<String>> FromIterator<(K, EvalValue)> for JsObjectMap {
    fn from_iter<T: IntoIterator<Item = (K, EvalValue)>>(iter: T) -> Self {
        let mut map = Self::new();
        for (k, v) in iter {
            map.insert(k, v);
        }
        map
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn s(v: &str) -> EvalValue {
        EvalValue::Str(v.to_string())
    }

    fn key_vec(map: &JsObjectMap) -> Vec<&str> {
        map.keys().collect()
    }

    #[test]
    fn array_index_recognition() {
        assert_eq!(array_index("0"), Some(0));
        assert_eq!(array_index("2"), Some(2));
        assert_eq!(array_index("10"), Some(10));
        assert_eq!(array_index("4294967294"), Some(4294967294));
        // 2^32 - 1 is a valid property key but NOT an array index per spec.
        assert_eq!(array_index("4294967295"), None);
        assert_eq!(array_index("4294967296"), None);
        assert_eq!(array_index("02"), None);
        assert_eq!(array_index("1.0"), None);
        assert_eq!(array_index("-1"), None);
        assert_eq!(array_index("+1"), None);
        assert_eq!(array_index(""), None);
        assert_eq!(array_index("00"), None);
        assert_eq!(array_index("1e3"), None);
        assert_eq!(array_index("a"), None);
    }

    #[test]
    fn numeric_keys_sort_ascending_before_named_keys() {
        let mut map = JsObjectMap::new();
        map.insert("b", s("b"));
        map.insert("10", s("ten"));
        map.insert("a", s("a"));
        map.insert("2", s("two"));
        assert_eq!(key_vec(&map), vec!["2", "10", "b", "a"]);
    }

    #[test]
    fn non_canonical_numeric_strings_keep_insertion_order() {
        let mut map = JsObjectMap::new();
        map.insert("02", s("x"));
        map.insert("1", s("one"));
        map.insert("1.0", s("y"));
        map.insert("z", s("z"));
        assert_eq!(key_vec(&map), vec!["1", "02", "1.0", "z"]);
    }

    #[test]
    fn overwrite_keeps_position_and_returns_old() {
        let mut map = JsObjectMap::new();
        map.insert("a", s("1"));
        map.insert("b", s("2"));
        map.insert("c", s("3"));
        assert_eq!(map.insert("a", s("updated")), Some(s("1")));
        assert_eq!(key_vec(&map), vec!["a", "b", "c"]);
        assert_eq!(map.get("a"), Some(&s("updated")));
        assert_eq!(map.len(), 3);

        map.insert("5", s("five"));
        map.insert("3", s("three"));
        assert_eq!(map.insert("5", s("FIVE")), Some(s("five")));
        assert_eq!(key_vec(&map), vec!["3", "5", "a", "b", "c"]);
        assert_eq!(map.get("5"), Some(&s("FIVE")));
    }

    #[test]
    fn remove_and_len() {
        let mut map = JsObjectMap::new();
        map.insert("2", s("two"));
        map.insert("x", s("x"));
        map.insert("y", s("y"));
        assert_eq!(map.remove("x"), Some(s("x")));
        assert_eq!(map.remove("x"), None);
        assert_eq!(map.remove("3"), None);
        assert_eq!(map.remove("2"), Some(s("two")));
        assert_eq!(key_vec(&map), vec!["y"]);
        assert_eq!(map.len(), 1);
        assert!(!map.is_empty());
        assert!(map.contains_key("y"));
        assert!(!map.contains_key("x"));
    }

    #[test]
    fn iteration_is_stable_across_calls() {
        let map: JsObjectMap = [
            ("zeta", s("1")),
            ("7", s("2")),
            ("alpha", s("3")),
            ("0", s("4")),
        ]
        .into_iter()
        .collect();
        let first: Vec<_> = map.entries().collect();
        let second: Vec<_> = map.entries().collect();
        assert_eq!(first, second);
        assert_eq!(key_vec(&map), vec!["0", "7", "zeta", "alpha"]);
    }

    #[test]
    fn wide_maps_keep_scan_semantics_past_the_index_threshold() {
        let n = NAMED_INDEX_THRESHOLD * 3;
        let mut map = JsObjectMap::new();
        let mut scan = Vec::new();
        for i in 0..n {
            map.insert(format!("key{i}"), s(&i.to_string()));
            scan.push((format!("key{i}"), s(&i.to_string())));
        }
        assert_eq!(map.insert("key5", s("updated")), Some(s("5")));
        scan[5].1 = s("updated");
        assert_eq!(map.remove("key7"), Some(s("7")));
        assert_eq!(map.remove("key7"), None);
        scan.remove(7);
        map.insert("key7", s("readded"));
        scan.push(("key7".to_string(), s("readded")));
        let expected: Vec<(&str, &EvalValue)> = scan.iter().map(|(k, v)| (k.as_str(), v)).collect();
        assert_eq!(map.entries().collect::<Vec<_>>(), expected);
        assert_eq!(map.get("key5"), Some(&s("updated")));
        assert_eq!(map.get("missing"), None);
        let narrow: JsObjectMap = scan.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        assert_eq!(map, narrow);
    }

    #[test]
    fn from_unique_entries_matches_sequential_insert() {
        let n = NAMED_INDEX_THRESHOLD * 2;
        let entries: Vec<(String, EvalValue)> = (0..n)
            .map(|i| {
                let key = match i % 5 {
                    0 => (n - i).to_string(),
                    1 => format!("0{i}"),
                    2 => "4294967295".to_string() + &i.to_string(),
                    _ => format!("key{i}"),
                };
                (key, s(&i.to_string()))
            })
            .chain([("__proto__".to_string(), s("p")), ("".to_string(), s("e"))])
            .collect();
        let sequential: JsObjectMap = entries.iter().cloned().collect();
        let direct = JsObjectMap::from_unique_entries(entries.clone());
        assert_eq!(direct, sequential);
        assert_eq!(
            direct.entries().collect::<Vec<_>>(),
            sequential.entries().collect::<Vec<_>>()
        );
        for (k, v) in &entries {
            assert_eq!(direct.get(k), Some(v));
        }
        let named_only: Vec<(String, EvalValue)> =
            (0..3).map(|i| (format!("k{i}"), s("v"))).collect();
        let direct = JsObjectMap::from_unique_entries(named_only.clone());
        assert_eq!(direct, named_only.into_iter().collect());
        assert!(JsObjectMap::from_unique_entries(Vec::new()).is_empty());
    }

    #[test]
    fn to_json_follows_json_stringify_semantics() {
        let mut obj = JsObjectMap::new();
        obj.insert("kept", EvalValue::Num(1.0));
        obj.insert("dropped", EvalValue::Undefined);
        obj.insert("nan", EvalValue::Num(f64::NAN));
        obj.insert("frac", EvalValue::Num(0.5));
        obj.insert(
            "arr",
            EvalValue::Arr(vec![EvalValue::Undefined, EvalValue::Null, s("v")]),
        );
        assert_eq!(
            obj.to_json(),
            json!({ "kept": 1, "nan": null, "frac": 0.5, "arr": [null, null, "v"] })
        );
        assert_eq!(EvalValue::Undefined.to_json(), Value::Null);
    }

    #[test]
    fn from_json_roundtrip_reorders_index_keys() {
        let v = json!({ "10": "ten", "2": "two", "name": "x" });
        let EvalValue::Obj(map) = EvalValue::from_json(&v) else {
            panic!("expected object");
        };
        assert_eq!(key_vec(&map), vec!["2", "10", "name"]);
        assert_eq!(map.to_json(), v);
    }
}
