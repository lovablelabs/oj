use std::ops::Deref;

use tsrs_core::collections::OrderedMap;

use super::json::Json;
use super::jsonvalue::{JSONData, JSONValueType};

#[repr(i8)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum ObjectKind {
    #[default]
    Unknown,
    Subpaths,
    Conditions,
    Imports,
    Invalid,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExportsOrImports {
    pub data: JSONData<ExportsOrImports>,
    // Go computes this lazily; it only depends on the keys, so it is computed when decoding.
    object_kind: ObjectKind,
}

impl ExportsOrImports {
    pub(crate) fn from_json(value: &Json) -> ExportsOrImports {
        let mut e = ExportsOrImports {
            data: JSONData::from_json(value, ExportsOrImports::from_json),
            object_kind: ObjectKind::Unknown,
        };
        e.init_object_kind();
        e
    }

    pub fn as_object(&self) -> &OrderedMap<String, ExportsOrImports> {
        assert!(self.type_() == JSONValueType::Object, "expected object");
        self.data.as_object()
    }

    pub fn as_array(&self) -> &[ExportsOrImports] {
        assert!(self.type_() == JSONValueType::Array, "expected array");
        self.data.as_array()
    }

    pub fn is_subpaths(&self) -> bool {
        self.object_kind == ObjectKind::Subpaths
    }

    pub fn is_imports(&self) -> bool {
        self.object_kind == ObjectKind::Imports
    }

    pub fn is_conditions(&self) -> bool {
        self.object_kind == ObjectKind::Conditions
    }

    fn init_object_kind(&mut self) {
        if self.object_kind == ObjectKind::Unknown && self.type_() == JSONValueType::Object {
            let obj = self.data.as_object();
            if !obj.is_empty() {
                let (mut seen_dot, mut seen_hash, mut seen_other) = (false, false, false);
                for k in obj.keys() {
                    let k = k.as_bytes();
                    if !k.is_empty() {
                        seen_dot = seen_dot || k[0] == b'.';
                        seen_hash = seen_hash || k[0] == b'#';
                        seen_other = seen_other || (k[0] != b'.' && k[0] != b'#');
                        if seen_other && (seen_dot || seen_hash) {
                            self.object_kind = ObjectKind::Invalid;
                            return;
                        }
                    }
                }
                if seen_dot {
                    self.object_kind = ObjectKind::Subpaths;
                    return;
                }
                if seen_hash {
                    self.object_kind = ObjectKind::Imports;
                    return;
                }
            }
            self.object_kind = ObjectKind::Conditions;
        }
    }
}

impl Deref for ExportsOrImports {
    type Target = JSONData<ExportsOrImports>;
    fn deref(&self) -> &JSONData<ExportsOrImports> {
        &self.data
    }
}
