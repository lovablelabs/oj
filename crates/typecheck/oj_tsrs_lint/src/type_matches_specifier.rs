//! Port of tsgolint's `internal/utils/type_matches_specifier.go`.

use tsrs_ast::{self as ast, Kind, Node, Symbol, SymbolFlags};
use tsrs_checker::{Checker, Type, TypeFlags};
use tsrs_compiler::Program;
use tsrs_core::{
    P,
    json::{self, Value},
    tspath,
};

use crate::utils::is_source_file_default_library;

#[derive(Clone, Copy)]
enum SpecifierFrom {
    File,
    Lib,
    Package,
    Name,
}

pub struct TypeOrValueSpecifier {
    from: SpecifierFrom,
    names: Vec<String>,
    path: String,
    package: String,
}

impl TypeOrValueSpecifier {
    pub fn new(value: &Value) -> Result<Self, String> {
        if matches!(value, Value::String(_) | Value::Null) {
            return Ok(Self {
                from: SpecifierFrom::Name,
                names: vec![match value {
                    Value::String(name) => name.clone(),
                    _ => String::new(),
                }],
                path: String::new(),
                package: String::new(),
            });
        }
        let Value::Object(object) = value else {
            return Err("type-or-value specifier must be a string or object".into());
        };
        let string = |name: &str| match object.get(name) {
            None | Some(Value::Null) => Ok(String::new()),
            Some(Value::String(value)) => Ok(value.clone()),
            _ => Err(format!("type-or-value specifier {name} must be a string")),
        };
        let from = match string("from")?.as_str() {
            "" | "file" => SpecifierFrom::File,
            "lib" => SpecifierFrom::Lib,
            "package" => SpecifierFrom::Package,
            _ => return Err("type-or-value specifier from must be file, lib, or package".into()),
        };
        let names = match object.get("name") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::String(name)) => vec![name.clone()],
            Some(Value::Array(names)) => names
                .iter()
                .map(|name| match name {
                    Value::String(name) => Ok(name.clone()),
                    _ => Err("type-or-value specifier name array must contain strings".into()),
                })
                .collect::<Result<_, String>>()?,
            _ => return Err("type-or-value specifier name must be a string or array".into()),
        };
        Ok(Self { from, names, path: string("path")?, package: string("package")? })
    }

    fn matches_name(&self, name: &str) -> bool {
        self.names.iter().any(|allowed| allowed == name)
    }

    fn matches_source(&self, declarations: &[P<Node>], program: &Program) -> bool {
        let files = || declarations.iter().filter_map(|&node| ast::get_source_file_of_node(node));
        match self.from {
            SpecifierFrom::Name => true,
            SpecifierFrom::File => {
                let cwd = program.host().get_current_directory();
                if self.path.is_empty() {
                    files().any(|file| file.file_name().starts_with(cwd))
                } else {
                    let path = tspath::get_normalized_absolute_path(&self.path, cwd);
                    files().any(|file| file.file_name() == path)
                }
            }
            SpecifierFrom::Lib => {
                declarations.is_empty()
                    || files().any(|file| is_source_file_default_library(program, file))
            }
            SpecifierFrom::Package => {
                if declarations.iter().any(|&node| {
                    find_parent_module_declaration(node)
                        .is_some_and(|module| module.name().unwrap().text() == self.package)
                }) {
                    return true;
                }
                if self.package.is_empty() {
                    return false;
                }
                let types_package = get_types_package_name(&self.package);
                files().any(|file| {
                    source_file_package_name(program, file.file_name()).is_some_and(|name| {
                        (path_is_in_package(&name, &self.package)
                            || path_is_in_package(
                                name.strip_prefix("@types/").unwrap_or(&name),
                                &types_package,
                            ))
                            && program.is_source_file_from_external_library(file)
                    })
                })
            }
        }
    }
}

fn find_parent_module_declaration(mut node: P<Node>) -> Option<P<Node>> {
    loop {
        match node.kind() {
            Kind::ModuleDeclaration => {
                let declaration = node.as_module_declaration();
                if declaration.keyword != Kind::NamespaceKeyword {
                    return (declaration.name().kind() == Kind::StringLiteral).then_some(node);
                }
            }
            Kind::SourceFile => return None,
            _ => {}
        }
        node = node.parent()?;
    }
}

fn contains_node_modules_segment(path: &str) -> bool {
    format!("/{path}/").contains("/node_modules/")
}

fn path_is_in_package(path: &str, package: &str) -> bool {
    path == package || path.strip_prefix(package).is_some_and(|suffix| suffix.starts_with('/'))
}

fn source_file_package_name(program: &Program, file_name: &str) -> Option<String> {
    let file_name = tspath::normalize_slashes(file_name);
    if !contains_node_modules_segment(&file_name) {
        return None;
    }
    let mut directory = tspath::get_directory_path(&file_name);
    while !directory.is_empty() && contains_node_modules_segment(&directory) {
        let path = tspath::combine_paths(&directory, &["package.json"]);
        if let Some(contents) = program.host().fs().read_file(&path)
            && let Ok(Value::Object(object)) = json::unmarshal(&contents)
            && let Some(Value::String(name)) = object.get("name")
            && !name.is_empty()
        {
            return Some(name.clone());
        }
        let parent = tspath::get_directory_path(&directory);
        if parent == directory {
            break;
        }
        directory = parent;
    }
    let segments: Vec<_> = file_name.split('/').collect();
    for (index, &segment) in segments.iter().enumerate().rev() {
        if segment != "node_modules" {
            continue;
        }
        let Some(&name) = segments.get(index + 1) else {
            continue;
        };
        if matches!(name, "" | "." | "..") {
            continue;
        }
        if name.starts_with('@') {
            if let Some(&package) = segments.get(index + 2)
                && !package.is_empty()
            {
                return Some(format!("{name}/{package}"));
            }
            continue;
        }
        return Some(name.into());
    }
    None
}

fn get_types_package_name(package: &str) -> String {
    if let Some(scoped) = package.strip_prefix('@')
        && let Some((scope, name)) = scoped.split_once('/')
        && !scope.is_empty()
        && !name.is_empty()
    {
        return format!("{scope}__{name}");
    }
    package.into()
}

fn type_matches_specifier(t: P<Type>, specifier: &TypeOrValueSpecifier, program: &Program) -> bool {
    let name_symbol = if let Some(alias) = t.alias() { alias.symbol() } else { t.symbol() };
    if !(name_symbol.is_some_and(|symbol| specifier.matches_name(symbol.name()))
        || t.flags().intersects(TypeFlags::Intrinsic)
            && specifier.matches_name(t.as_intrinsic_type().intrinsic_name()))
    {
        return false;
    }
    let source_symbol = t.symbol().or_else(|| t.alias()?.symbol());
    specifier.matches_source(source_symbol.map_or(&[], |symbol| symbol.declarations()), program)
}

pub fn type_matches_some_specifier(
    t: P<Type>,
    specifiers: &[TypeOrValueSpecifier],
    program: &Program,
) -> bool {
    let matches = |t: P<Type>| {
        if t.flags().intersects(TypeFlags::Intrinsic)
            && t.as_intrinsic_type().intrinsic_name() == "error"
        {
            return false;
        }
        specifiers.iter().any(|specifier| type_matches_specifier(t, specifier, program))
    };
    matches(t)
        || t.flags().intersects(TypeFlags::Intersection)
            && t.types().iter().any(|&part| part != t && matches(part))
}

pub fn symbol_matches_specifier_name_and_source(
    symbol: Option<P<Symbol>>,
    name: &str,
    specifier: &TypeOrValueSpecifier,
    program: &Program,
) -> bool {
    symbol.is_some_and(|symbol| {
        specifier.matches_name(name) && specifier.matches_source(symbol.declarations(), program)
    })
}

fn static_name(node: P<Node>) -> &'static str {
    match node.kind() {
        Kind::Identifier | Kind::StringLiteral => node.text(),
        Kind::PrivateIdentifier => node.text().strip_prefix('#').unwrap_or(node.text()),
        _ => "",
    }
}

pub fn value_matches_some_specifier(
    node: P<Node>,
    specifiers: &[TypeOrValueSpecifier],
    program: &Program,
    checker: &mut Checker,
) -> bool {
    let name = static_name(node);
    if name.is_empty() {
        return false;
    }
    specifiers.iter().any(|specifier| {
        if !specifier.matches_name(name) {
            return false;
        }
        if matches!(specifier.from, SpecifierFrom::Name) {
            return true;
        }
        let mut symbol = checker.get_symbol_at_location_exported(node);
        if let Some(declaration) = symbol.and_then(|symbol| symbol.value_declaration())
            && declaration.kind() == Kind::ShorthandPropertyAssignment
        {
            symbol = checker.get_shorthand_assignment_value_symbol(Some(declaration));
        }
        if let Some(declaration) = symbol.and_then(|symbol| symbol.value_declaration())
            && declaration.kind() == Kind::BindingElement
            && let Some(parent) = declaration.parent()
            && parent.kind() == Kind::ObjectBindingPattern
            && declaration.as_binding_element().dot_dot_dot_token().is_none()
        {
            let property_name = static_name(declaration.property_name_or_name().unwrap());
            if property_name.is_empty() {
                return false;
            }
            let source = checker.get_type_at_location(parent);
            symbol = checker.get_property_of_type(source, property_name);
        }
        if let Some(alias) = symbol
            && alias.flags().intersects(SymbolFlags::Alias)
        {
            symbol = Some(checker.get_aliased_symbol(alias));
        }
        symbol_matches_specifier_name_and_source(symbol, name, specifier, program)
    })
}
