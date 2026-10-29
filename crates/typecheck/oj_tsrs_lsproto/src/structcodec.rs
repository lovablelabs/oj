// Go's structcodec.go is a reflection-driven object decoder: it enforces that the value is an object,
// that every `lsp:"required"` field is present, and that a JSON null is rejected for a nilable field
// whose spec is not nullable. Without reflection, the generator emits the per-type field loop and the
// strictness decisions (which fields are required / reject null) itself, and calls the helpers below.

use crate::json::{Json, JsonError, Value, is_null, kind};
use crate::lsp::{err_missing, err_not_object, err_null};
use tsrs_core::json::JsonMap;

// unmarshalStruct's object check. A non-strict structure (one Go decodes with json/v2's default struct
// codec) decodes null as its zero value and rejects other non-objects with json/v2's kind error.
pub fn struct_members<'a>(
    v: &'a Value,
    go_type: &str,
    strict: bool,
) -> Result<Option<&'a JsonMap>, JsonError> {
    match v {
        Value::Object(members) => Ok(Some(members)),
        Value::Null if !strict => Ok(None),
        _ if strict => Err(JsonError::method(go_type, err_not_object(kind(v)))),
        _ => Err(JsonError::mismatch(v, go_type)),
    }
}

// A field decoded with json/v2's codec for its type.
pub fn field<T: Json>(name: &str, v: &Value) -> Result<T, JsonError> {
    T::from_json(v).map_err(|e| e.within(name))
}

// A required nilable field that rejects null.
pub(crate) fn field_non_null<T: Json>(
    name: &str,
    v: &Value,
    go_type: &str,
) -> Result<T, JsonError> {
    if is_null(v) {
        return Err(JsonError::method(go_type, err_null(name)).within(name));
    }
    field(name, v)
}

// An optional (pointer) field: null decodes as nil.
pub(crate) fn opt<T: Json>(name: &str, v: &Value) -> Result<Option<T>, JsonError> {
    if is_null(v) {
        return Ok(None);
    }
    field(name, v).map(Some)
}

// An optional (pointer) field that rejects null.
pub fn opt_non_null<T: Json>(name: &str, v: &Value, go_type: &str) -> Result<Option<T>, JsonError> {
    if is_null(v) {
        return Err(JsonError::method(go_type, err_null(name)).within(name));
    }
    field(name, v).map(Some)
}

// structcodec.go:100
pub fn check_required(seen: u64, required_names: &[&str], go_type: &str) -> Result<(), JsonError> {
    let required_mask =
        if required_names.len() == 64 { u64::MAX } else { (1u64 << required_names.len()) - 1 };
    let missing = required_mask & !seen;
    if missing != 0 {
        let mut missing_props = Vec::new();
        missing_props.extend(
            required_names
                .iter()
                .enumerate()
                .filter_map(|(id, n)| if missing & (1 << id) != 0 { Some(*n) } else { None }),
        );
        return Err(JsonError::method(go_type, err_missing(&missing_props)));
    }
    Ok(())
}

// A union arm decoded with the union's decoder (Go `o.X = new(T); json.UnmarshalDecode(dec, o.X)`).
pub(crate) fn arm<T: Json>(v: &Value) -> Result<Option<T>, JsonError> {
    T::from_json(v).map(Some)
}

// A union arm decoded from the buffered value (Go `o.X = new(T); json.Unmarshal(data, o.X)`).
pub(crate) fn arm_buffered<T: Json>(v: &Value) -> Result<Option<T>, JsonError> {
    T::from_json(v).map(Some).map_err(JsonError::seal)
}

fn value_str(v: &Value) -> Option<&str> {
    match v {
        Value::String(s) => Some(s),
        _ => None,
    }
}

// structcodec.go:172
pub struct DiscriminatedStructDecoder<'a> {
    type_name: &'static str,
    discriminator: &'static str,
    discriminator_value: Option<&'a Value>,
}

// structcodec.go:184
pub fn scan_discriminated_struct<'a>(
    v: &'a Value,
    go_type: &str,
    type_name: &'static str,
    discriminator: &'static str,
) -> Result<DiscriminatedStructDecoder<'a>, JsonError> {
    let Value::Object(members) = v else {
        return Err(JsonError::method(go_type, err_not_object(kind(v))));
    };
    Ok(DiscriminatedStructDecoder {
        type_name,
        discriminator,
        discriminator_value: members.get(discriminator),
    })
}

impl DiscriminatedStructDecoder<'_> {
    // Go compares the raw discriminator value against the quoted literal.
    pub fn discriminator_str(&self) -> Option<&str> {
        self.discriminator_value.and_then(value_str)
    }

    // structcodec.go:226
    pub(crate) fn invalid_discriminator(&self, go_type: &str) -> JsonError {
        let Some(value) = self.discriminator_value else {
            return JsonError::method(
                go_type,
                format!(
                    "invalid {}: missing discriminator {}",
                    self.type_name,
                    tsrs_core::json::marshal_string(self.discriminator)
                ),
            );
        };
        JsonError::method(
            go_type,
            format!(
                "invalid {} discriminator {}: {}",
                self.type_name,
                tsrs_core::json::marshal_string(self.discriminator),
                tsrs_core::json::marshal(value).unwrap_or_default()
            ),
        )
    }
}

// structcodec.go:235: the arm is decoded with its struct spec. Go decodes the discriminator first, then
// replays the fields that preceded it (each with a fresh json.Unmarshal), then decodes the remaining fields
// from the stream; the arm's own errors are reported against the union's type.
pub fn unmarshal_discriminated_arm<T: Json>(
    v: &Value,
    union_go_type: &str,
    discriminator: &str,
) -> Result<T, JsonError> {
    let err = match T::from_json(v) {
        Ok(arm) => return Ok(arm),
        Err(err) => err,
    };
    let Value::Object(members) = v else {
        return Err(err);
    };
    let discriminator_index = members.get_index_of(discriminator);
    // Go's field order decides which error is reported first.
    let mut err = match discriminator_index {
        Some(d) if d > 0 => {
            let mut reordered = JsonMap::new();
            reordered.reserve(members.len());
            reordered.extend(
                members
                    .iter()
                    .skip(d)
                    .take(1)
                    .chain(members.iter().take(d))
                    .chain(members.iter().skip(d + 1))
                    .map(|(k, item)| (k.to_string(), item.clone())),
            );
            match T::from_json(&Value::Object(reordered)) {
                Ok(arm) => return Ok(arm),
                Err(err) => err,
            }
        }
        _ => err,
    };
    if err.go_type == T::GO_TYPE {
        err.go_type = union_go_type.to_string();
    }
    if !err.sealed
        && let Some(field) = err.pointer.last()
    {
        let replayed = match (members.get_index_of(field.as_str()), discriminator_index) {
            (Some(f), Some(d)) => f <= d,
            (Some(_), None) => true,
            _ => false,
        };
        if replayed {
            err.pointer.pop();
            err = err.seal();
        }
    }
    Err(err)
}

// Go `cmp.Compare`.
pub(crate) fn cmp_compare<T: PartialOrd>(a: &T, b: &T) -> std::cmp::Ordering {
    if a < b {
        std::cmp::Ordering::Less
    } else if a > b {
        std::cmp::Ordering::Greater
    } else {
        std::cmp::Ordering::Equal
    }
}
