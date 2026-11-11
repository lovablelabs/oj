//! Headless input protocol, ported from tsgolint's `cmd/tsgolint/payload.go`.

use tsrs_api_transport::strictjson::{self, DecodeError};
use tsrs_core::collections::OrderedMap;
use tsrs_core::json::{JsonMap, Value};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Payload {
    pub configs: Vec<Config>,
    pub source_overrides: OrderedMap<String, String>,
    pub report_syntactic: bool,
    pub report_semantic: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Config {
    pub file_paths: Vec<String>,
    pub rules: Vec<ConfiguredRule>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConfiguredRule {
    pub name: String,
    pub options: Value,
}

pub fn deserialize_payload(data: &str) -> Result<Payload, String> {
    let value = strictjson::decode(data.as_bytes())
        .map_err(|(DecodeError::Invalid(error) | DecodeError::OutOfRange(error))| error)?
        .value;
    let empty = JsonMap::default();
    let object = if value == Value::Null { &empty } else { object(&value)? };
    let version = match object.get("version") {
        None | Some(Value::Null) => 0,
        Some(Value::Number(n))
            if n.fract() == 0.0 && *n >= i64::MIN as f64 && *n < -(i64::MIN as f64) =>
        {
            *n as i64
        }
        _ => return Err("version must be an integer".into()),
    };
    match version {
        2 => deserialize_v2(object).map_err(|e| format!("failed to deserialize V2 payload: {e}")),
        0 => {
            let payload = deserialize_v1(object)
                .map_err(|e| format!("failed to deserialize V1 payload: {e}"))?;
            if payload.configs.is_empty() {
                return Err("V1 payload has no files".into());
            }
            Ok(payload)
        }
        _ => Err(format!("unsupported version `{version}`: expected `unset` or `2`")),
    }
}

fn deserialize_v1(value: &JsonMap) -> Result<Payload, String> {
    let files = array(value.get("files"))?;
    let mut configs = Vec::with_capacity(files.len());
    for file in files {
        let file = object(file)?;
        let rules = array(file.get("rules"))?
            .iter()
            .map(|rule| Ok(ConfiguredRule { name: string(Some(rule))?, options: Value::Null }))
            .collect::<Result<Vec<_>, String>>()?;
        configs.push(Config { file_paths: vec![string(file.get("file_path"))?], rules });
    }
    Ok(Payload { configs, ..Payload::default() })
}

fn deserialize_v2(value: &JsonMap) -> Result<Payload, String> {
    let mut configs = Vec::new();
    for config in array(value.get("configs"))? {
        let config = object(config)?;
        let file_paths = array(config.get("file_paths"))?
            .iter()
            .map(|path| string(Some(path)))
            .collect::<Result<Vec<_>, _>>()?;
        let mut rules = Vec::new();
        for rule in array(config.get("rules"))? {
            let rule = object(rule)?;
            rules.push(ConfiguredRule {
                name: string(rule.get("name"))?,
                options: rule.get("options").cloned().unwrap_or_default(),
            });
        }
        configs.push(Config { file_paths, rules });
    }
    let mut source_overrides = OrderedMap::default();
    if let Some(overrides) = value.get("source_overrides").filter(|v| **v != Value::Null) {
        for (path, text) in object(overrides)? {
            source_overrides.insert(path.to_owned(), string(Some(text))?);
        }
    }
    Ok(Payload {
        configs,
        source_overrides,
        report_syntactic: boolean(value.get("report_syntactic"))?,
        report_semantic: boolean(value.get("report_semantic"))?,
    })
}

pub(crate) fn object(value: &Value) -> Result<&JsonMap, String> {
    match value {
        Value::Object(object) => Ok(object),
        _ => Err("expected object".into()),
    }
}

fn array(value: Option<&Value>) -> Result<&[Value], String> {
    match value {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(array)) => Ok(array),
        _ => Err("expected array".into()),
    }
}

fn string(value: Option<&Value>) -> Result<String, String> {
    match value {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(string)) => Ok(string.clone()),
        _ => Err("expected string".into()),
    }
}

fn boolean(value: Option<&Value>) -> Result<bool, String> {
    match value {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        _ => Err("expected boolean".into()),
    }
}
