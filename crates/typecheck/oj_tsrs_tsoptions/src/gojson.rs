// Go `json.Marshal` / `json.Unmarshal` of core.CompilerOptions, core.TypeAcquisition, core.BuildOptions
// and core.ProjectReference (struct tags in tsc/internal/core): the representation the native API puts on
// the wire. Tristate is a JSON bool (`null`/absent = unknown), enums are their numeric values, zero
// values are omitted (`omitzero`). Unknown members are ignored like Go's decoder.

use tsrs_core::collections::OrderedMap;
use tsrs_core::json::{JsonMap, Value};
use tsrs_core::{
    BuildOptions, CompilerOptions, JsxEmit, ModuleDetectionKind, ModuleKind, ModuleResolutionKind,
    NewLineKind, PluginImport, ProjectReference, ScriptTarget, Tristate, TypeAcquisition,
};

use crate::parsinghelpers::for_each_compiler_options_field;

pub trait GoJson: Sized {
    /// `None` when the value is Go's zero value (omitted under `omitzero`).
    fn to_go_json(&self) -> Option<Value>;
    fn from_go_json(value: &Value, field: &str) -> Result<Self, String>;
    /// Width of the Go integer this field decodes from (32: int32 enums, 64: `*int`); 0 if not an integer.
    const INTEGER_BITS: u32 = 0;
    /// Like `from_go_json`, but an integer with no Rust enum variant (Go keeps any int32) is returned beside the
    /// default value instead of being an error.
    fn from_go_json_keeping_unknown(
        value: &Value,
        field: &str,
    ) -> Result<(Self, Option<i32>), String> {
        Self::from_go_json(value, field).map(|v| (v, None))
    }
}

fn type_error(field: &str, expected: &str, value: &Value) -> String {
    format!("json: cannot unmarshal {} into field {field:?} of type {expected}", kind_name(value))
}

fn kind_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) | Value::Integer(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

impl GoJson for Tristate {
    fn to_go_json(&self) -> Option<Value> {
        match self {
            Tristate::Unknown => None,
            Tristate::False => Some(Value::Bool(false)),
            Tristate::True => Some(Value::Bool(true)),
        }
    }
    fn from_go_json(value: &Value, _field: &str) -> Result<Self, String> {
        // core.Tristate.UnmarshalJSON: "true" / "false", anything else is unknown.
        Ok(match value {
            Value::Bool(true) => Tristate::True,
            Value::Bool(false) => Tristate::False,
            _ => Tristate::Unknown,
        })
    }
}

impl GoJson for bool {
    fn to_go_json(&self) -> Option<Value> {
        if *self { Some(Value::Bool(true)) } else { None }
    }
    fn from_go_json(value: &Value, field: &str) -> Result<Self, String> {
        match value {
            Value::Null => Ok(false),
            Value::Bool(b) => Ok(*b),
            v => Err(type_error(field, "bool", v)),
        }
    }
}

impl GoJson for String {
    fn to_go_json(&self) -> Option<Value> {
        if self.is_empty() { None } else { Some(Value::String(self.clone())) }
    }
    fn from_go_json(value: &Value, field: &str) -> Result<Self, String> {
        match value {
            Value::Null => Ok(String::new()),
            Value::String(s) => Ok(s.clone()),
            v => Err(type_error(field, "string", v)),
        }
    }
}

fn int_from(value: &Value, field: &str) -> Result<i32, String> {
    match value {
        Value::Number(n) if n.fract() == 0.0 && *n >= i32::MIN as f64 && *n <= i32::MAX as f64 => {
            Ok(*n as i32)
        }
        v => Err(type_error(field, "int", v)),
    }
}

impl GoJson for Option<i32> {
    const INTEGER_BITS: u32 = 32;
    fn to_go_json(&self) -> Option<Value> {
        self.map(|v| Value::Number(v as f64))
    }
    fn from_go_json(value: &Value, field: &str) -> Result<Self, String> {
        match value {
            Value::Null => Ok(None),
            v => int_from(v, field).map(Some),
        }
    }
}

impl GoJson for Option<i64> {
    const INTEGER_BITS: u32 = 64;
    fn to_go_json(&self) -> Option<Value> {
        // Exactly: f64 cannot hold every Go int.
        self.map(|v| {
            if v.unsigned_abs() <= 1 << 53 { Value::Number(v as f64) } else { Value::Integer(v) }
        })
    }
    fn from_go_json(value: &Value, field: &str) -> Result<Self, String> {
        // The API checks the literal's int64 range and replaces this with the exact literal (`predecode`); i64::MAX
        // itself parses as the f64 2^63, which saturates back to i64::MAX here.
        match value {
            Value::Null => Ok(None),
            Value::Number(n)
                if n.fract() == 0.0
                    && *n >= -9_223_372_036_854_775_808.0
                    && *n <= 9_223_372_036_854_775_808.0 =>
            {
                Ok(Some(*n as i64))
            }
            Value::Integer(n) => Ok(Some(*n)),
            v => Err(type_error(field, "int", v)),
        }
    }
}

fn strings_from(value: &Value, field: &str) -> Result<Vec<String>, String> {
    match value {
        Value::Array(items) => items.iter().map(|v| String::from_go_json(v, field)).collect(),
        v => Err(type_error(field, "[]string", v)),
    }
}

fn strings_to(v: &[String]) -> Value {
    Value::Array(v.iter().map(|s| Value::String(s.clone())).collect())
}

impl GoJson for Option<Vec<String>> {
    fn to_go_json(&self) -> Option<Value> {
        self.as_deref().map(strings_to)
    }
    fn from_go_json(value: &Value, field: &str) -> Result<Self, String> {
        match value {
            Value::Null => Ok(None),
            v => strings_from(v, field).map(Some),
        }
    }
}

impl GoJson for Option<OrderedMap<String, Option<Vec<String>>>> {
    fn to_go_json(&self) -> Option<Value> {
        self.as_ref().map(|m| {
            let mut o = JsonMap::new();
            // encoding/json/v2 marshals a nil slice as `[]` (and unmarshals `null` to nil).
            o.extend(
                m.iter().map(|(k, v)| (k.clone(), strings_to(v.as_deref().unwrap_or_default()))),
            );
            Value::Object(o)
        })
    }
    fn from_go_json(value: &Value, field: &str) -> Result<Self, String> {
        match value {
            Value::Null => Ok(None),
            Value::Object(entries) => {
                let mut m = OrderedMap::default();
                m = entries.iter().try_fold(m, |mut m, (k, v)| {
                    m.insert(k.to_string(), Option::<Vec<String>>::from_go_json(v, field)?);
                    Ok::<_, String>(m)
                })?;
                Ok(Some(m))
            }
            v => Err(type_error(field, "map[string][]string", v)),
        }
    }
}

impl GoJson for Option<std::sync::Arc<OrderedMap<String, Option<Vec<String>>>>> {
    fn to_go_json(&self) -> Option<Value> {
        self.as_ref().map(|m| {
            let mut o = JsonMap::new();
            // encoding/json/v2 marshals a nil slice as `[]` (and unmarshals `null` to nil).
            o.extend(
                m.iter().map(|(k, v)| (k.clone(), strings_to(v.as_deref().unwrap_or_default()))),
            );
            Value::Object(o)
        })
    }
    fn from_go_json(value: &Value, field: &str) -> Result<Self, String> {
        Option::<OrderedMap<String, Option<Vec<String>>>>::from_go_json(value, field)
            .map(|m| m.map(std::sync::Arc::new))
    }
}

impl GoJson for Option<Vec<PluginImport>> {
    fn to_go_json(&self) -> Option<Value> {
        self.as_ref().map(|v| {
            Value::Array(
                v.iter()
                    .map(|p| {
                        let mut o = JsonMap::new();
                        o.insert("name".to_string(), Value::String(p.name.clone()));
                        Value::Object(o)
                    })
                    .collect(),
            )
        })
    }
    fn from_go_json(value: &Value, field: &str) -> Result<Self, String> {
        match value {
            Value::Null => Ok(None),
            Value::Array(items) => items
                .iter()
                .map(|item| match item {
                    Value::Object(o) => Ok(PluginImport {
                        name: String::from_go_json(o.get("name").unwrap_or(&Value::Null), field)?,
                    }),
                    v => Err(type_error(field, "PluginImport", v)),
                })
                .collect::<Result<Vec<_>, _>>()
                .map(Some),
            v => Err(type_error(field, "[]PluginImport", v)),
        }
    }
}

macro_rules! enum_go_json {
    ($($ty:ident { $($variant:ident),* $(,)? })*) => {
        $(impl GoJson for $ty {
            fn to_go_json(&self) -> Option<Value> {
                if *self == <$ty>::default() { None } else { Some(Value::Number((*self as i32) as f64)) }
            }
            fn from_go_json(value: &Value, field: &str) -> Result<Self, String> {
                if matches!(value, Value::Null) {
                    return Ok(<$ty>::default());
                }
                let n = int_from(value, field)?;
                $(if n == $ty::$variant as i32 { return Ok($ty::$variant); })*
                Err(format!("json: invalid value {n} for field {field:?} of type {}", stringify!($ty)))
            }
            const INTEGER_BITS: u32 = 32;
            fn from_go_json_keeping_unknown(value: &Value, field: &str) -> Result<(Self, Option<i32>), String> {
                if matches!(value, Value::Null) {
                    return Ok((<$ty>::default(), None));
                }
                let n = int_from(value, field)?;
                $(if n == $ty::$variant as i32 { return Ok(($ty::$variant, None)); })*
                Ok((<$ty>::default(), Some(n)))
            }
        })*
    };
}

enum_go_json! {
    ModuleDetectionKind { None, Auto, Legacy, Force }
    NewLineKind { None, CRLF, LF }
}

// Go int32 option types held as Rust newtypes: every int32 decodes and compiles as Go does (no named constant
// needed).
macro_rules! int32_go_json {
    ($($ty:ident)*) => {
        $(impl GoJson for $ty {
            fn to_go_json(&self) -> Option<Value> {
                if *self == <$ty>::default() { None } else { Some(Value::Number(self.0 as f64)) }
            }
            fn from_go_json(value: &Value, field: &str) -> Result<Self, String> {
                if matches!(value, Value::Null) {
                    return Ok(<$ty>::default());
                }
                int_from(value, field).map($ty)
            }
            const INTEGER_BITS: u32 = 32;
        })*
    };
}

int32_go_json! { ModuleKind ScriptTarget JsxEmit ModuleResolutionKind }

fn object<'a>(value: &'a Value, what: &str) -> Result<Option<&'a JsonMap>, String> {
    match value {
        Value::Null => Ok(None),
        Value::Object(o) => Ok(Some(o)),
        v => Err(format!("json: cannot unmarshal {} into Go value of type {what}", kind_name(v))),
    }
}

pub fn compiler_options_to_go_json(options: &CompilerOptions) -> Value {
    let mut o = JsonMap::new();
    let unknown = |json: &str| {
        options
            .api_unknown_enum_values
            .iter()
            .find(|(k, _)| *k == json)
            .map(|(_, n)| Value::Number(*n as f64))
    };
    macro_rules! fields {
        ($($field:ident: $json:literal,)*) => {
            $(if let Some(v) = unknown($json).or_else(|| options.$field.to_go_json()) { o.insert($json.to_string(), v); })*
        };
    }
    for_each_compiler_options_field!(fields);
    Value::Object(o)
}

/// Distinguishes an unknown compiler-option name from a zero-valued option.
///
/// Go `json.Marshal` of the one core.CompilerOptions field whose json tag is `name`, as
/// `contentmapper.MarshalDeclaredOptions` finds it by reflection: `None` when no field has that tag,
/// `Some(None)` when the field is zero (`reflect.Value.IsZero`, the same values `omitzero` omits).
#[expect(
    clippy::option_option,
    reason = "Distinguishes no matching serializer or field from a matching nil value, as in Go."
)]
pub(crate) fn compiler_option_to_go_json(
    options: &CompilerOptions,
    name: &str,
) -> Option<Option<Value>> {
    let unknown = |json: &str| {
        options
            .api_unknown_enum_values
            .iter()
            .find(|(k, _)| *k == json)
            .map(|(_, n)| Value::Number(*n as f64))
    };
    macro_rules! fields {
        ($($field:ident: $json:literal,)*) => {
            match name {
                $($json => Some(unknown($json).or_else(|| options.$field.to_go_json())),)*
                _ => None,
            }
        };
    }
    for_each_compiler_options_field!(fields)
}

/// JSON names and Go integer widths of the core.CompilerOptions fields Go decodes from JSON integers.
pub fn compiler_options_integer_fields() -> Vec<(&'static str, u32)> {
    fn bits<T: GoJson>(_: &T) -> u32 {
        T::INTEGER_BITS
    }

    let mut out = Vec::new();
    let options = tsrs_core::CompilerOptions::default();
    macro_rules! fields {
        ($($field:ident: $json:literal,)*) => {
            $(if bits(&options.$field) != 0 { out.push(($json, bits(&options.$field))); })*
        };
    }
    for_each_compiler_options_field!(fields);
    out
}

/// JSON names and Go integer widths of the core.BuildOptions fields Go decodes from JSON integers.
pub const BUILD_OPTIONS_INTEGER_FIELDS: &[(&str, u32)] = &[("builders", 64)];

pub fn compiler_options_from_go_json(value: &Value) -> Result<CompilerOptions, String> {
    let mut options = tsrs_core::CompilerOptions::default();
    let Some(o) = object(value, "core.CompilerOptions")? else {
        return Ok(options);
    };
    macro_rules! fields {
        ($($field:ident: $json:literal,)*) => {
            $(if let Some(v) = o.get($json) {
                let (value, unknown) = GoJson::from_go_json_keeping_unknown(v, $json)?;
                options.$field = value;
                if let Some(n) = unknown {
                    options.api_unknown_enum_values.push(($json, n));
                }
            })*
        };
    }
    for_each_compiler_options_field!(fields);
    Ok(options)
}

pub fn type_acquisition_to_go_json(t: &TypeAcquisition) -> Value {
    let mut o = JsonMap::new();
    let mut add = |k: &str, v: Option<Value>| {
        if let Some(v) = v {
            o.insert(k.to_string(), v);
        }
    };
    add("enable", t.enable.to_go_json());
    add("include", t.include.to_go_json());
    add("exclude", t.exclude.to_go_json());
    add(
        "disableFilenameBasedTypeAcquisition",
        t.disable_filename_based_type_acquisition.to_go_json(),
    );
    Value::Object(o)
}

pub fn build_options_to_go_json(b: &BuildOptions) -> Value {
    let mut o = JsonMap::new();
    let mut add = |k: &str, v: Option<Value>| {
        if let Some(v) = v {
            o.insert(k.to_string(), v);
        }
    };
    add("dry", b.dry.to_go_json());
    add("force", b.force.to_go_json());
    add("verbose", b.verbose.to_go_json());
    add("builders", b.builders.to_go_json());
    add("stopBuildOnErrors", b.stop_build_on_errors.to_go_json());
    add("clean", b.clean.to_go_json());
    Value::Object(o)
}

pub fn build_options_from_go_json(value: &Value) -> Result<BuildOptions, String> {
    let mut b = BuildOptions::default();
    let Some(o) = object(value, "core.BuildOptions")? else {
        return Ok(b);
    };
    let null = Value::Null;
    let get = |k: &str| o.get(k).unwrap_or(&null);
    b.dry = GoJson::from_go_json(get("dry"), "dry")?;
    b.force = GoJson::from_go_json(get("force"), "force")?;
    b.verbose = GoJson::from_go_json(get("verbose"), "verbose")?;
    b.builders = GoJson::from_go_json(get("builders"), "builders")?;
    b.stop_build_on_errors = GoJson::from_go_json(get("stopBuildOnErrors"), "stopBuildOnErrors")?;
    b.clean = GoJson::from_go_json(get("clean"), "clean")?;
    Ok(b)
}

pub fn project_reference_to_go_json(r: &ProjectReference) -> Value {
    let mut o = JsonMap::new();
    o.insert("path".to_string(), Value::String(r.path.clone()));
    if !r.original_path.is_empty() {
        o.insert("originalPath".to_string(), Value::String(r.original_path.clone()));
    }
    // encoding/json/v2 `omitempty` never omits booleans.
    o.insert("circular".to_string(), Value::Bool(r.circular));
    Value::Object(o)
}

pub fn project_reference_from_go_json(value: &Value) -> Result<ProjectReference, String> {
    let Some(o) = object(value, "core.ProjectReference")? else {
        return Ok(ProjectReference::default());
    };
    let null = Value::Null;
    Ok(ProjectReference {
        path: String::from_go_json(o.get("path").unwrap_or(&null), "path")?,
        original_path: String::from_go_json(
            o.get("originalPath").unwrap_or(&null),
            "originalPath",
        )?,
        circular: bool::from_go_json(o.get("circular").unwrap_or(&null), "circular")?,
    })
}
