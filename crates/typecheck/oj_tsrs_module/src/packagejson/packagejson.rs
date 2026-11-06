use rustc_hash::FxHashMap;
use tsrs_core::collections::{Set, new_set_with_size_hint};

use super::expected::{Expected, ExpectedValue};
use super::exportsorimports::ExportsOrImports;
use super::json::{self, Json};
use super::jsonvalue::JSONValue;

// Go's `Fields` embeds `HeaderFields`, `PathFields` and `DependencyFields`; their fields are
// flattened into this one struct (Go code reaches all of them through field promotion).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Fields {
    // HeaderFields
    pub name: Expected<String>,
    pub version: Expected<String>,
    pub type_: Expected<String>,

    // PathFields
    pub tsconfig: Expected<String>,
    pub main: Expected<String>,
    pub types: Expected<String>,
    pub typings: Expected<String>,
    pub types_versions: JSONValue,
    pub imports: ExportsOrImports,
    pub exports: ExportsOrImports,

    // DependencyFields
    pub dependencies: Expected<FxHashMap<String, String>>,
    pub dev_dependencies: Expected<FxHashMap<String, String>>,
    pub peer_dependencies: Expected<FxHashMap<String, String>>,
    pub optional_dependencies: Expected<FxHashMap<String, String>>,

    pub content_mapper: Expected<ContentMapperFields>,
}

impl Fields {
    // HasDependency returns true if the package.json has a dependency with the given name
    // under any of the dependency fields (dependencies, devDependencies, peerDependencies,
    // optionalDependencies).
    pub fn has_dependency(&self, name: &str) -> bool {
        if let Some(deps) = self.dependencies.get_value()
            && deps.contains_key(name)
        {
            return true;
        }
        if let Some(dev_deps) = self.dev_dependencies.get_value()
            && dev_deps.contains_key(name)
        {
            return true;
        }
        if let Some(peer_deps) = self.peer_dependencies.get_value()
            && peer_deps.contains_key(name)
        {
            return true;
        }
        if let Some(opt_deps) = self.optional_dependencies.get_value()
            && opt_deps.contains_key(name)
        {
            return true;
        }
        false
    }

    // Go ranges over each map in random order, and a caller (string completions) lists the names in the order it is
    // given; each field's names are visited sorted instead.
    pub fn range_dependencies(&self, mut f: impl FnMut(&str, &str, &str) -> bool) {
        for (deps, field) in [
            (&self.dependencies, "dependencies"),
            (&self.dev_dependencies, "devDependencies"),
            (&self.peer_dependencies, "peerDependencies"),
            (&self.optional_dependencies, "optionalDependencies"),
        ] {
            if let Some(deps) = deps.get_value() {
                let mut sorted: Vec<(&String, &String)> = deps.iter().collect();
                sorted.sort_unstable_by(|a, b| a.0.cmp(b.0));
                if !sorted.into_iter().all(|(name, version)| f(name, version, field)) {
                    return;
                }
            }
        }
    }

    // collects the names into a set
    pub fn get_runtime_dependency_names(&self) -> Set<String> {
        let deps = &self.dependencies.value;
        let peer_deps = &self.peer_dependencies.value;
        let opt_deps = &self.optional_dependencies.value;
        let mut names = new_set_with_size_hint(deps.len() + peer_deps.len() + opt_deps.len());
        names.m.extend(deps.keys().map(std::clone::Clone::clone));
        names.m.extend(peer_deps.keys().map(std::clone::Clone::clone));
        names.m.extend(opt_deps.keys().map(std::clone::Clone::clone));
        names
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ContentMapperFields {
    pub exec: Expected<Vec<String>>,
    pub compiler_options: Expected<Vec<String>>,
    pub dynamic_config: Expected<bool>,
    pub output_extensions: Expected<FxHashMap<String, String>>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct TypeScriptFields {
    content_mapper: Expected<ContentMapperFields>,
}

impl ExpectedValue for TypeScriptFields {
    fn unmarshal_from(&mut self, value: &Json) -> bool {
        match value {
            Json::Object(members) => {
                for (name, member) in members {
                    if name == "contentMapper" {
                        self.content_mapper.unmarshal_json(member);
                    }
                }
                true
            }
            Json::Null => {
                *self = TypeScriptFields::default();
                true
            }
            _ => false,
        }
    }
    fn expected_json_type() -> &'static str {
        "unknown"
    }
}

pub fn parse(data: &str) -> Result<Fields, String> {
    let value = json::parse(data)?;
    let mut parsed = Fields::default();
    let mut type_script: Expected<TypeScriptFields> = Expected::default();
    match &value {
        // Unmarshaling `null` into a struct leaves it zero.
        Json::Null => {}
        Json::Object(members) => {
            // Member names match struct tags exactly; repeated names decode again into the same field.
            for (name, member) in members {
                match name.as_str() {
                    "name" => parsed.name.unmarshal_json(member),
                    "version" => parsed.version.unmarshal_json(member),
                    "type" => parsed.type_.unmarshal_json(member),
                    "tsconfig" => parsed.tsconfig.unmarshal_json(member),
                    "main" => parsed.main.unmarshal_json(member),
                    "types" => parsed.types.unmarshal_json(member),
                    "typings" => parsed.typings.unmarshal_json(member),
                    "typesVersions" => parsed.types_versions = JSONValue::from_json(member),
                    "imports" => parsed.imports = ExportsOrImports::from_json(member),
                    "exports" => parsed.exports = ExportsOrImports::from_json(member),
                    "dependencies" => parsed.dependencies.unmarshal_json(member),
                    "devDependencies" => parsed.dev_dependencies.unmarshal_json(member),
                    "peerDependencies" => parsed.peer_dependencies.unmarshal_json(member),
                    "optionalDependencies" => parsed.optional_dependencies.unmarshal_json(member),
                    "typescript" => type_script.unmarshal_json(member),
                    _ => {}
                }
            }
        }
        _ => {
            return Err(format!(
                "json: cannot unmarshal JSON {} into Go packagejson.Fields",
                json_kind_name(&value)
            ));
        }
    }
    // Go ignores the validity flag here and takes whatever was decoded.
    parsed.content_mapper = type_script.value.content_mapper;
    Ok(parsed)
}

fn json_kind_name(value: &Json) -> &'static str {
    match value {
        Json::Null => "null",
        Json::Bool(_) => "boolean",
        Json::Number(_) => "number",
        Json::String(_) => "string",
        Json::Array(_) => "array",
        Json::Object(_) => "object",
    }
}
