//! Entry bytes (schema v3): little-endian, LEB128 lengths, no field names.
//!
//! ```text
//! magic "SXC" + schema byte | total length u32
//! fingerprint | root | flags (bit 0 modified, bit 1 map present)
//! deps:    count, then per event a tag byte and its fields
//! rules:   count, then per rule a flags byte, its strings, priority f64 bits
//! code | map (when flagged)
//! objects: count, then per `create` call its optional name and value tree
//! ```
//!
//! A hit that wants less than the whole result stops early: the sections sit in
//! the order callers drop them. Every read is bounds-checked and every string
//! validated, so a damaged file decodes to [`Corrupt`], never to a panic.

use std::sync::Arc;

use serde_json::{Number, Value};

use crate::eval::value::{EvalValue, JsObjectMap};
use crate::rules::StylexRule;

use super::CACHE_SCHEMA_VERSION;

pub(super) const MAGIC: [u8; 4] = [b'S', b'X', b'C', CACHE_SCHEMA_VERSION as u8];
/// The magic and the entry's total byte length.
pub(super) const PRELUDE_LEN: usize = 8;

const FLAG_MODIFIED: u8 = 1;
const FLAG_MAP: u8 = 1 << 1;

const RULE_RTL: u8 = 1;
const RULE_CONST_KEY: u8 = 1 << 1;
const RULE_CONST_VAL: u8 = 1 << 2;

const DEP_NEAREST_PACKAGE: u8 = 0;
const DEP_RESOLVE_IMPORT: u8 = 1;
const DEP_EXISTS: u8 = 2;
const DEP_EXISTS_ANY: u8 = 3;

const PATH_REL: u8 = 0;
const PATH_ABS: u8 = 1;

const VALUE_NULL: u8 = 0;
const VALUE_UNDEFINED: u8 = 1;
const VALUE_FALSE: u8 = 2;
const VALUE_TRUE: u8 = 3;
const VALUE_NUM: u8 = 4;
const VALUE_STR: u8 = 5;
const VALUE_ARR: u8 = 6;
const VALUE_OBJ: u8 = 7;

const JSON_NULL: u8 = 0;
const JSON_FALSE: u8 = 1;
const JSON_TRUE: u8 = 2;
const JSON_U64: u8 = 3;
const JSON_I64: u8 = 4;
const JSON_F64: u8 = 5;
const JSON_NUMBER_TEXT: u8 = 6;
const JSON_STRING: u8 = 7;
const JSON_ARRAY: u8 = 8;
const JSON_OBJECT: u8 = 9;

// Value trees nest by recursion; compiled StyleX objects are a few levels deep,
// so anything past this is a damaged file rather than a stack to overflow.
const MAX_DEPTH: u32 = 128;

// The most elements a count reserves room for ahead of reading them. A count
// is checked against the bytes left alone, and an element in memory is many
// times its fewest bytes on disk: a damaged count, at any depth of a tree,
// reserves no more than this, and a longer run grows as it is read.
const MAX_RESERVED: usize = 1024;

// The fewest bytes an element of each counted run takes.
const MIN_RULE_BYTES: usize = 11; // flags, two string lengths, the priority
const MIN_VALUE_BYTES: usize = 1; // a tag
const MIN_ENTRY_BYTES: usize = 2; // a key's length, its value's tag
const MIN_OBJECT_BYTES: usize = 3; // the name's flag, an entry count, the syntax's flag

/// The entry does not decode: truncated, damaged, or another format.
#[derive(Debug)]
pub(super) struct Corrupt;

type Decoded<T> = Result<T, Corrupt>;

// ------------------------------------------------------------------ writing

pub(super) fn put_varint(buf: &mut Vec<u8>, mut n: u64) {
    while n >= 0x80 {
        buf.push(n as u8 | 0x80);
        n >>= 7;
    }
    buf.push(n as u8);
}

pub(super) fn put_str(buf: &mut Vec<u8>, text: &str) {
    put_varint(buf, text.len() as u64);
    buf.extend_from_slice(text.as_bytes());
}

fn put_opt_str(buf: &mut Vec<u8>, text: Option<&str>) {
    match text {
        None => buf.push(0),
        Some(text) => {
            buf.push(1);
            put_str(buf, text);
        }
    }
}

/// A dep-log path as stored: in-root paths root-relative so a hit can
/// revalidate from any worktree, the rest absolute and pinned to their root.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum PathRef<'a> {
    Rel(&'a str),
    Abs(&'a str),
}

/// One recorded fs observation, borrowed from the entry bytes.
#[derive(Debug, PartialEq)]
pub(super) enum DepRecord<'a> {
    NearestPackage {
        from: PathRef<'a>,
        found: Option<(&'a str, PathRef<'a>)>,
        hash: Option<&'a str>,
    },
    ResolveImport {
        specifier: &'a str,
        importer: PathRef<'a>,
        resolved: Option<PathRef<'a>>,
    },
    Exists {
        path: PathRef<'a>,
        found: bool,
    },
    ExistsAny {
        path: PathRef<'a>,
        found: bool,
    },
}

fn put_path(buf: &mut Vec<u8>, path: PathRef<'_>) {
    let (tag, text) = match path {
        PathRef::Rel(text) => (PATH_REL, text),
        PathRef::Abs(text) => (PATH_ABS, text),
    };
    buf.push(tag);
    put_str(buf, text);
}

pub(super) fn put_dep(buf: &mut Vec<u8>, dep: &DepRecord<'_>) {
    match *dep {
        DepRecord::NearestPackage { from, found, hash } => {
            buf.push(DEP_NEAREST_PACKAGE);
            put_path(buf, from);
            match found {
                None => buf.push(0),
                Some((name, dir)) => {
                    buf.push(1);
                    put_str(buf, name);
                    put_path(buf, dir);
                }
            }
            put_opt_str(buf, hash);
        }
        DepRecord::ResolveImport {
            specifier,
            importer,
            resolved,
        } => {
            buf.push(DEP_RESOLVE_IMPORT);
            put_str(buf, specifier);
            put_path(buf, importer);
            match resolved {
                None => buf.push(0),
                Some(resolved) => {
                    buf.push(1);
                    put_path(buf, resolved);
                }
            }
        }
        DepRecord::Exists { path, found } => {
            buf.push(DEP_EXISTS);
            put_path(buf, path);
            buf.push(found as u8);
        }
        DepRecord::ExistsAny { path, found } => {
            buf.push(DEP_EXISTS_ANY);
            put_path(buf, path);
            buf.push(found as u8);
        }
    }
}

/// Opens an entry in `buf` (cleared first): the prelude, with the length left
/// for [`finish_entry`], then the header fields.
pub(super) fn begin_entry(
    buf: &mut Vec<u8>,
    fingerprint: &str,
    root: &str,
    modified: bool,
    has_map: bool,
) {
    buf.clear();
    buf.extend_from_slice(&MAGIC);
    buf.extend_from_slice(&[0; 4]);
    put_str(buf, fingerprint);
    put_str(buf, root);
    buf.push(if modified { FLAG_MODIFIED } else { 0 } | if has_map { FLAG_MAP } else { 0 });
}

/// Seals the entry; `false` when it outgrew the length field and must not be stored.
pub(super) fn finish_entry(buf: &mut [u8]) -> bool {
    let Ok(total) = u32::try_from(buf.len()) else {
        return false;
    };
    buf[MAGIC.len()..PRELUDE_LEN].copy_from_slice(&total.to_le_bytes());
    true
}

pub(super) fn put_rules(buf: &mut Vec<u8>, rules: &[StylexRule]) {
    put_varint(buf, rules.len() as u64);
    for rule in rules {
        buf.push(
            if rule.rtl.is_some() { RULE_RTL } else { 0 }
                | if rule.const_key.is_some() {
                    RULE_CONST_KEY
                } else {
                    0
                }
                | if rule.const_val.is_some() {
                    RULE_CONST_VAL
                } else {
                    0
                },
        );
        put_str(buf, &rule.class_name);
        put_str(buf, &rule.ltr);
        if let Some(rtl) = &rule.rtl {
            put_str(buf, rtl);
        }
        if let Some(const_key) = &rule.const_key {
            put_str(buf, const_key);
        }
        if let Some(const_val) = &rule.const_val {
            put_json(buf, const_val);
        }
        buf.extend_from_slice(&rule.priority.to_bits().to_le_bytes());
    }
}

fn put_json(buf: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Null => buf.push(JSON_NULL),
        Value::Bool(false) => buf.push(JSON_FALSE),
        Value::Bool(true) => buf.push(JSON_TRUE),
        Value::Number(number) => put_json_number(buf, number),
        Value::String(text) => {
            buf.push(JSON_STRING);
            put_str(buf, text);
        }
        Value::Array(items) => {
            buf.push(JSON_ARRAY);
            put_varint(buf, items.len() as u64);
            for item in items {
                put_json(buf, item);
            }
        }
        Value::Object(map) => {
            buf.push(JSON_OBJECT);
            put_varint(buf, map.len() as u64);
            for (key, value) in map {
                put_str(buf, key);
                put_json(buf, value);
            }
        }
    }
}

// A number goes out as its bits only when the bits rebuild an equal `Number`:
// a build that unifies serde_json's `arbitrary_precision` in keeps the source
// text as the number's identity, and that text is then what is stored.
fn put_json_number(buf: &mut Vec<u8>, number: &Number) {
    if let Some(n) = number.as_u64()
        && Number::from(n) == *number
    {
        buf.push(JSON_U64);
        buf.extend_from_slice(&n.to_le_bytes());
    } else if let Some(n) = number.as_i64()
        && Number::from(n) == *number
    {
        buf.push(JSON_I64);
        buf.extend_from_slice(&n.to_le_bytes());
    } else if let Some(n) = number.as_f64()
        && Number::from_f64(n).as_ref() == Some(number)
    {
        buf.push(JSON_F64);
        buf.extend_from_slice(&n.to_bits().to_le_bytes());
    } else {
        buf.push(JSON_NUMBER_TEXT);
        put_str(buf, &number.to_string());
    }
}

pub(super) fn put_output(buf: &mut Vec<u8>, code: &str, map: Option<&str>) {
    put_str(buf, code);
    if let Some(map) = map {
        put_str(buf, map);
    }
}

pub(super) fn put_objects(buf: &mut Vec<u8>, objects: &[(Option<String>, Arc<JsObjectMap>)]) {
    put_varint(buf, objects.len() as u64);
    for (name, object) in objects {
        put_opt_str(buf, name.as_deref());
        put_object(buf, object);
    }
}

fn put_object(buf: &mut Vec<u8>, object: &JsObjectMap) {
    put_varint(buf, object.len() as u64);
    for (key, value) in object.entries() {
        put_str(buf, key);
        put_value(buf, value);
    }
    put_opt_str(buf, object.css_type());
}

pub(super) fn put_value(buf: &mut Vec<u8>, value: &EvalValue) {
    match value {
        EvalValue::Null => buf.push(VALUE_NULL),
        EvalValue::Undefined => buf.push(VALUE_UNDEFINED),
        EvalValue::Bool(false) => buf.push(VALUE_FALSE),
        EvalValue::Bool(true) => buf.push(VALUE_TRUE),
        // The bits, so NaN payloads, -0.0 and the infinities come back exact.
        EvalValue::Num(n) => {
            buf.push(VALUE_NUM);
            buf.extend_from_slice(&n.to_bits().to_le_bytes());
        }
        EvalValue::Str(text) => {
            buf.push(VALUE_STR);
            put_str(buf, text);
        }
        EvalValue::Arr(items) => {
            buf.push(VALUE_ARR);
            put_varint(buf, items.len() as u64);
            for item in items {
                put_value(buf, item);
            }
        }
        EvalValue::Obj(object) => {
            buf.push(VALUE_OBJ);
            put_object(buf, object);
        }
    }
}

// ------------------------------------------------------------------ reading

/// The header fields every hit checks before it replays the dep log.
pub(super) struct Header<'a> {
    pub fingerprint: &'a str,
    pub root: &'a str,
    pub modified: bool,
    pub has_map: bool,
}

/// The length an entry declares for itself, once its prelude has been read.
pub(super) fn declared_len(prelude: &[u8]) -> Option<usize> {
    let prelude = prelude.get(..PRELUDE_LEN)?;
    if prelude[..MAGIC.len()] != MAGIC {
        return None;
    }
    let total = u32::from_le_bytes(prelude[MAGIC.len()..].try_into().ok()?);
    Some(total as usize)
}

pub(super) struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    /// A reader over one whole entry, positioned after the prelude.
    pub fn open(entry: &'a [u8]) -> Decoded<Self> {
        if declared_len(entry) != Some(entry.len()) {
            return Err(Corrupt);
        }
        Ok(Reader {
            bytes: entry,
            at: PRELUDE_LEN,
        })
    }

    #[cfg(test)]
    pub fn raw(bytes: &'a [u8]) -> Self {
        Reader { bytes, at: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.at == self.bytes.len()
    }

    fn take(&mut self, len: usize) -> Decoded<&'a [u8]> {
        let end = self.at.checked_add(len).ok_or(Corrupt)?;
        let bytes = self.bytes.get(self.at..end).ok_or(Corrupt)?;
        self.at = end;
        Ok(bytes)
    }

    fn byte(&mut self) -> Decoded<u8> {
        let byte = *self.bytes.get(self.at).ok_or(Corrupt)?;
        self.at += 1;
        Ok(byte)
    }

    fn flag(&mut self) -> Decoded<bool> {
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Corrupt),
        }
    }

    fn varint(&mut self) -> Decoded<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = self.byte()?;
            let bits = u64::from(byte & 0x7f);
            if shift == 63 && bits > 1 {
                return Err(Corrupt);
            }
            value |= bits << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(Corrupt)
    }

    /// An element count: every element takes at least a byte, so a count past
    /// the remaining bytes is damage.
    fn count(&mut self) -> Decoded<usize> {
        let count = usize::try_from(self.varint()?).map_err(|_| Corrupt)?;
        if count > self.bytes.len() - self.at {
            return Err(Corrupt);
        }
        Ok(count)
    }

    /// [`Self::count`] with the capacity to reserve for the elements: no
    /// more than the remaining bytes hold at `min_bytes` each, nor than
    /// [`MAX_RESERVED`], whatever the count says.
    fn counted(&mut self, min_bytes: usize) -> Decoded<(usize, usize)> {
        let count = self.count()?;
        let fit = (self.bytes.len() - self.at) / min_bytes;
        Ok((count, count.min(fit).min(MAX_RESERVED)))
    }

    fn blob(&mut self) -> Decoded<&'a [u8]> {
        let len = usize::try_from(self.varint()?).map_err(|_| Corrupt)?;
        self.take(len)
    }

    fn str(&mut self) -> Decoded<&'a str> {
        std::str::from_utf8(self.blob()?).map_err(|_| Corrupt)
    }

    fn opt_str(&mut self) -> Decoded<Option<&'a str>> {
        Ok(if self.flag()? {
            Some(self.str()?)
        } else {
            None
        })
    }

    fn f64(&mut self) -> Decoded<f64> {
        let bits = self.take(8)?.try_into().map_err(|_| Corrupt)?;
        Ok(f64::from_bits(u64::from_le_bytes(bits)))
    }

    pub fn header(&mut self) -> Decoded<Header<'a>> {
        let fingerprint = self.str()?;
        let root = self.str()?;
        let flags = self.byte()?;
        if flags & !(FLAG_MODIFIED | FLAG_MAP) != 0 {
            return Err(Corrupt);
        }
        Ok(Header {
            fingerprint,
            root,
            modified: flags & FLAG_MODIFIED != 0,
            has_map: flags & FLAG_MAP != 0,
        })
    }

    pub fn dep_count(&mut self) -> Decoded<usize> {
        self.count()
    }

    fn path(&mut self) -> Decoded<PathRef<'a>> {
        match self.byte()? {
            PATH_REL => Ok(PathRef::Rel(self.str()?)),
            PATH_ABS => Ok(PathRef::Abs(self.str()?)),
            _ => Err(Corrupt),
        }
    }

    pub fn dep(&mut self) -> Decoded<DepRecord<'a>> {
        match self.byte()? {
            DEP_NEAREST_PACKAGE => {
                let from = self.path()?;
                let found = if self.flag()? {
                    Some((self.str()?, self.path()?))
                } else {
                    None
                };
                let hash = self.opt_str()?;
                Ok(DepRecord::NearestPackage { from, found, hash })
            }
            DEP_RESOLVE_IMPORT => {
                let specifier = self.str()?;
                let importer = self.path()?;
                let resolved = if self.flag()? {
                    Some(self.path()?)
                } else {
                    None
                };
                Ok(DepRecord::ResolveImport {
                    specifier,
                    importer,
                    resolved,
                })
            }
            DEP_EXISTS => Ok(DepRecord::Exists {
                path: self.path()?,
                found: self.flag()?,
            }),
            DEP_EXISTS_ANY => Ok(DepRecord::ExistsAny {
                path: self.path()?,
                found: self.flag()?,
            }),
            _ => Err(Corrupt),
        }
    }

    pub fn rules(&mut self) -> Decoded<Vec<StylexRule>> {
        let (count, reserved) = self.counted(MIN_RULE_BYTES)?;
        let mut rules = Vec::with_capacity(reserved);
        for _ in 0..count {
            let flags = self.byte()?;
            if flags & !(RULE_RTL | RULE_CONST_KEY | RULE_CONST_VAL) != 0 {
                return Err(Corrupt);
            }
            let class_name = self.str()?.into();
            let ltr = self.str()?.into();
            let rtl = match flags & RULE_RTL {
                0 => None,
                _ => Some(self.str()?.into()),
            };
            let const_key = match flags & RULE_CONST_KEY {
                0 => None,
                _ => Some(self.str()?.into()),
            };
            let const_val = match flags & RULE_CONST_VAL {
                0 => None,
                _ => Some(Box::new(self.json(0)?)),
            };
            rules.push(StylexRule {
                class_name,
                ltr,
                rtl,
                const_key,
                const_val,
                priority: self.f64()?,
            });
        }
        Ok(rules)
    }

    fn json(&mut self, depth: u32) -> Decoded<Value> {
        if depth > MAX_DEPTH {
            return Err(Corrupt);
        }
        Ok(match self.byte()? {
            JSON_NULL => Value::Null,
            JSON_FALSE => Value::Bool(false),
            JSON_TRUE => Value::Bool(true),
            JSON_U64 => {
                let bits = self.take(8)?.try_into().map_err(|_| Corrupt)?;
                Value::Number(u64::from_le_bytes(bits).into())
            }
            JSON_I64 => {
                let bits = self.take(8)?.try_into().map_err(|_| Corrupt)?;
                Value::Number(i64::from_le_bytes(bits).into())
            }
            JSON_F64 => Value::Number(Number::from_f64(self.f64()?).ok_or(Corrupt)?),
            JSON_NUMBER_TEXT => {
                Value::Number(serde_json::from_str(self.str()?).map_err(|_| Corrupt)?)
            }
            JSON_STRING => Value::String(self.str()?.to_string()),
            JSON_ARRAY => {
                let (count, reserved) = self.counted(MIN_VALUE_BYTES)?;
                let mut items = Vec::with_capacity(reserved);
                for _ in 0..count {
                    items.push(self.json(depth + 1)?);
                }
                Value::Array(items)
            }
            JSON_OBJECT => {
                let count = self.count()?;
                let mut map = serde_json::Map::new();
                for _ in 0..count {
                    let key = self.str()?.to_string();
                    map.insert(key, self.json(depth + 1)?);
                }
                Value::Object(map)
            }
            _ => return Err(Corrupt),
        })
    }

    pub fn code(&mut self) -> Decoded<String> {
        Ok(self.str()?.to_string())
    }

    pub fn objects(&mut self) -> Decoded<Vec<(Option<String>, Arc<JsObjectMap>)>> {
        let (count, reserved) = self.counted(MIN_OBJECT_BYTES)?;
        let mut objects = Vec::with_capacity(reserved);
        for _ in 0..count {
            let name = self.opt_str()?.map(str::to_string);
            objects.push((name, Arc::new(self.object(0)?)));
        }
        Ok(objects)
    }

    fn object(&mut self, depth: u32) -> Decoded<JsObjectMap> {
        let (count, reserved) = self.counted(MIN_ENTRY_BYTES)?;
        let mut object = JsObjectMap::with_capacity(reserved);
        for _ in 0..count {
            let key = self.str()?;
            object.insert(key, self.value(depth + 1)?);
        }
        if let Some(syntax) = self.opt_str()? {
            object.set_css_type(syntax.to_string());
        }
        Ok(object)
    }

    pub fn value(&mut self, depth: u32) -> Decoded<EvalValue> {
        if depth > MAX_DEPTH {
            return Err(Corrupt);
        }
        Ok(match self.byte()? {
            VALUE_NULL => EvalValue::Null,
            VALUE_UNDEFINED => EvalValue::Undefined,
            VALUE_FALSE => EvalValue::Bool(false),
            VALUE_TRUE => EvalValue::Bool(true),
            VALUE_NUM => EvalValue::Num(self.f64()?),
            VALUE_STR => EvalValue::Str(self.str()?.to_string()),
            VALUE_ARR => {
                let (count, reserved) = self.counted(MIN_VALUE_BYTES)?;
                let mut items = Vec::with_capacity(reserved);
                for _ in 0..count {
                    items.push(self.value(depth + 1)?);
                }
                EvalValue::Arr(items)
            }
            VALUE_OBJ => EvalValue::Obj(Arc::new(self.object(depth)?)),
            _ => return Err(Corrupt),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_count_reserves_no_more_than_the_bytes_left_could_hold() {
        let counted = |count: u64, left: usize, min_bytes: usize| {
            let mut bytes = Vec::new();
            put_varint(&mut bytes, count);
            bytes.resize(bytes.len() + left, 0);
            Reader::raw(&bytes).counted(min_bytes).ok()
        };
        assert_eq!(counted(40, 40, MIN_VALUE_BYTES), Some((40, 40)));
        assert_eq!(counted(40, 40, MIN_RULE_BYTES), Some((40, 3)));
        assert_eq!(counted(40, 40, MIN_OBJECT_BYTES), Some((40, 13)));
        assert_eq!(counted(41, 40, MIN_VALUE_BYTES), None);
        assert_eq!(
            counted(1 << 20, 1 << 20, MIN_VALUE_BYTES),
            Some((1 << 20, MAX_RESERVED))
        );
    }

    #[test]
    fn a_run_longer_than_its_reserve_decodes_whole() {
        let count = MAX_RESERVED * 3 + 1;
        let items = EvalValue::Arr((0..count).map(|i| EvalValue::Num(i as f64)).collect());
        let mut bytes = Vec::new();
        put_value(&mut bytes, &items);
        let mut reader = Reader::raw(&bytes);
        let decoded = reader.value(0).expect("decodes");
        assert!(reader.is_empty());
        assert!(matches!(&decoded, EvalValue::Arr(items) if items.len() == count));
        let mut again = Vec::new();
        put_value(&mut again, &decoded);
        assert!(bytes == again, "round-trip drifted");

        let rules: Vec<StylexRule> = (0..count)
            .map(|i| StylexRule {
                class_name: format!("x{i}").into(),
                ltr: format!(".x{i}{{order:{i}}}").into(),
                rtl: None,
                const_key: None,
                const_val: None,
                priority: 3000.0,
            })
            .collect();
        let mut bytes = Vec::new();
        put_rules(&mut bytes, &rules);
        let mut reader = Reader::raw(&bytes);
        assert_eq!(reader.rules().expect("decodes"), rules);
        assert!(reader.is_empty());
    }
}
