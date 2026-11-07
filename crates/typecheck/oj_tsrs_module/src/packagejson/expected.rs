use rustc_hash::FxHashMap;

use super::json::Json;
use super::packagejson::ContentMapperFields;
use super::validated::TypeValidatedField;

// Types that can sit inside `Expected<T>`: how Go's `json.Unmarshal` decodes a raw value into `T`
// and which JSON type `reflect` reports for `T`.
pub trait ExpectedValue: Default + Clone {
    // Decodes `value` into `self`, mirroring `json.Unmarshal(data, &e.Value)` (which merges into
    // maps and leaves `self` unchanged on a type mismatch). Returns false where Go returns an error.
    fn unmarshal_from(&mut self, value: &Json) -> bool;
    fn expected_json_type() -> &'static str;
}

impl ExpectedValue for String {
    fn unmarshal_from(&mut self, value: &Json) -> bool {
        match value {
            Json::String(s) => {
                self.clone_from(s);
                true
            }
            Json::Null => {
                *self = String::new();
                true
            }
            _ => false,
        }
    }
    fn expected_json_type() -> &'static str {
        "string"
    }
}

impl ExpectedValue for bool {
    fn unmarshal_from(&mut self, value: &Json) -> bool {
        match value {
            Json::Bool(b) => {
                *self = *b;
                true
            }
            Json::Null => {
                *self = false;
                true
            }
            _ => false,
        }
    }
    fn expected_json_type() -> &'static str {
        "boolean"
    }
}

impl ExpectedValue for Vec<String> {
    fn unmarshal_from(&mut self, value: &Json) -> bool {
        match value {
            Json::Array(elements) => {
                let mut result = Vec::with_capacity(elements.len());
                result = match elements.iter().try_fold(result, |mut result, element| {
                    let mut s = String::new();
                    if !s.unmarshal_from(element) {
                        return None;
                    }
                    result.push(s);
                    Some(result)
                }) {
                    Some(result) => result,
                    None => return false,
                };
                *self = result;
                true
            }
            Json::Null => {
                self.clear();
                true
            }
            _ => false,
        }
    }
    fn expected_json_type() -> &'static str {
        "array"
    }
}

#[expect(
    clippy::implicit_hasher,
    reason = "The API deliberately accepts only FxHashMap, as required by the workspace hashing policy."
)]
impl ExpectedValue for FxHashMap<String, String> {
    fn unmarshal_from(&mut self, value: &Json) -> bool {
        match value {
            Json::Object(members) => {
                for (name, member) in members {
                    let mut s = String::new();
                    if !s.unmarshal_from(member) {
                        return false;
                    }
                    self.insert(name.clone(), s);
                }
                true
            }
            Json::Null => {
                self.clear();
                true
            }
            _ => false,
        }
    }
    fn expected_json_type() -> &'static str {
        "object"
    }
}

impl ExpectedValue for ContentMapperFields {
    fn unmarshal_from(&mut self, value: &Json) -> bool {
        match value {
            Json::Object(members) => {
                for (name, member) in members {
                    match name.as_str() {
                        "exec" => self.exec.unmarshal_json(member),
                        "compilerOptions" => self.compiler_options.unmarshal_json(member),
                        "dynamicConfig" => self.dynamic_config.unmarshal_json(member),
                        "outputExtensions" => self.output_extensions.unmarshal_json(member),
                        _ => {}
                    }
                }
                true
            }
            Json::Null => {
                *self = ContentMapperFields::default();
                true
            }
            _ => false,
        }
    }
    fn expected_json_type() -> &'static str {
        "unknown"
    }
}

#[derive(Clone, Debug, Default)]
pub struct Expected<T> {
    actual_json_type: &'static str,
    pub null: bool,
    pub valid: bool,
    pub value: T,
}

impl<T: ExpectedValue> Expected<T> {
    pub fn unmarshal_json(&mut self, data: &Json) {
        if matches!(data, Json::Null) {
            *self = Expected {
                actual_json_type: "null",
                null: true,
                valid: false,
                value: T::default(),
            };
            return;
        }
        // Go decodes the raw value with a fresh `json.Unmarshal`, which rejects duplicate names.
        if !data.has_duplicate_names() && self.value.unmarshal_from(data) {
            self.valid = true;
        }
        self.actual_json_type = match data.first_byte() {
            b'"' => "string",
            b't' | b'f' => "boolean",
            b'[' => "array",
            b'{' => "object",
            _ => "number",
        };
    }

    pub fn is_present(&self) -> bool {
        !self.actual_json_type.is_empty()
    }

    pub fn get_value(&self) -> Option<&T> {
        if self.valid { Some(&self.value) } else { None }
    }

    pub fn is_valid(&self) -> bool {
        self.valid
    }

    fn expected_json_type() -> &'static str {
        T::expected_json_type()
    }

    fn actual_json_type(&self) -> &'static str {
        self.actual_json_type
    }
}

impl<T: ExpectedValue> TypeValidatedField for Expected<T> {
    fn is_present(&self) -> bool {
        Expected::is_present(self)
    }
    fn is_valid(&self) -> bool {
        Expected::is_valid(self)
    }
    fn expected_json_type(&self) -> &'static str {
        Expected::<T>::expected_json_type()
    }
    fn actual_json_type(&self) -> &'static str {
        Expected::actual_json_type(self)
    }
}

// Compares the exported fields only (like Go's `cmpopts.IgnoreUnexported`).
impl<T: PartialEq> PartialEq for Expected<T> {
    fn eq(&self, other: &Self) -> bool {
        self.null == other.null && self.valid == other.valid && self.value == other.value
    }
}

pub fn expected_of<T: ExpectedValue>(value: T) -> Expected<T> {
    Expected { value, valid: true, null: false, actual_json_type: T::expected_json_type() }
}
