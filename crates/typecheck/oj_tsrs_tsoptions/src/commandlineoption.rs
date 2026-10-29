use std::sync::LazyLock;

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_core::collections::OrderedMap;
use tsrs_core::{
    JsxEmit, ModuleDetectionKind, ModuleKind, ModuleResolutionKind, NewLineKind, ScriptTarget,
    Tristate,
};
use tsrs_diagnostics::Message;

use crate::enummaps::{
    JSX_OPTION_MAP, LIB_MAP, MODULE_DETECTION_OPTION_MAP, MODULE_OPTION_MAP,
    MODULE_RESOLUTION_OPTION_MAP, NEW_LINE_OPTION_MAP, TARGET_OPTION_MAP,
};
use crate::tsconfigparsing::CommandLineOptionNameMap;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum CommandLineOptionKind {
    String,
    Number,
    Boolean,
    Object,
    List,
    ListOrElement,
    Enum,
}

impl CommandLineOptionKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            CommandLineOptionKind::String => "string",
            CommandLineOptionKind::Number => "number",
            CommandLineOptionKind::Boolean => "boolean",
            CommandLineOptionKind::Object => "object",
            CommandLineOptionKind::List => "list",
            CommandLineOptionKind::ListOrElement => "listOrElement",
            CommandLineOptionKind::Enum => "enum",
        }
    }
}

impl std::fmt::Display for CommandLineOptionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// Go `DefaultValueDescription any`: only the value shapes the declarations use.
#[derive(Clone, Copy, Debug)]
pub enum DefaultValueDescription {
    None,
    Bool(bool),
    Str(&'static str),
    Int(i32),
    Message(&'static Message),
    Tristate(Tristate),
    ScriptTarget(ScriptTarget),
}

#[derive(Debug)]
pub struct CommandLineOption {
    pub name: &'static str,
    pub short_name: &'static str,
    pub kind: CommandLineOptionKind,

    // used in parsing
    pub is_file_path: bool,
    pub is_tsconfig_only: bool,
    pub is_command_line_only: bool,

    // used in output
    pub description: Option<&'static Message>,
    pub default_value_description: DefaultValueDescription,
    pub show_in_simplified_help_view: bool,

    // used in output in serializing and generate tsconfig
    pub category: Option<&'static Message>,

    // What kind of extra validation `validateJsonOptionValue` should do
    pub extra_validation: ExtraValidation,

    // checks that option with number type has value >= minValue
    pub min_value: i32,

    // true or undefined
    // used for configDirTemplateSubstitutionOptions
    pub allow_config_dir_template_substitution: bool,

    // used for filter in compilerrunner
    pub affects_declaration_path: bool,
    pub affects_program_structure: bool,
    pub affects_semantic_diagnostics: bool,
    pub affects_build_info: bool,
    pub affects_bind_diagnostics: bool,
    pub affects_source_file: bool,
    pub affects_module_resolution: bool,
    pub affects_emit: bool,

    pub allow_js_flag: bool,
    pub strict_flag: bool,

    // used in transpileoptions worker
    // todo: revisit to see if this can be reduced to boolean
    pub transpile_option_value: Tristate,

    // used for CommandLineOptionTypeList
    pub list_preserve_falsy_values: bool,
    // used for COMPILER_OPTIONS_DECLARATION
    pub element_options: Option<CommandLineOptionNameMap>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExtraValidation {
    None,
    Locale,
}

impl CommandLineOption {
    pub const DEFAULT: CommandLineOption = CommandLineOption {
        name: "",
        short_name: "",
        kind: CommandLineOptionKind::String,
        is_file_path: false,
        is_tsconfig_only: false,
        is_command_line_only: false,
        description: None,
        default_value_description: DefaultValueDescription::None,
        show_in_simplified_help_view: false,
        category: None,
        extra_validation: ExtraValidation::None,
        min_value: 0,
        allow_config_dir_template_substitution: false,
        affects_declaration_path: false,
        affects_program_structure: false,
        affects_semantic_diagnostics: false,
        affects_build_info: false,
        affects_bind_diagnostics: false,
        affects_source_file: false,
        affects_module_resolution: false,
        affects_emit: false,
        allow_js_flag: false,
        strict_flag: false,
        transpile_option_value: Tristate::Unknown,
        list_preserve_falsy_values: false,
        element_options: None,
    };

    pub(crate) fn deprecated_keys(&self) -> Option<&'static FxHashSet<&'static str>> {
        if self.kind != CommandLineOptionKind::Enum {
            return None;
        }
        COMMAND_LINE_OPTION_DEPRECATED.get(self.name)
    }

    pub fn enum_map(&self) -> Option<&'static OrderedMap<&'static str, CompilerOptionsValue>> {
        if self.kind != CommandLineOptionKind::Enum {
            return None;
        }
        COMMAND_LINE_OPTION_ENUM_MAP.get(self.name).map(|m| {
            let m: &'static LazyLock<OrderedMap<&'static str, CompilerOptionsValue>> = m;
            &**m
        })
    }

    pub fn elements(&self) -> Option<&'static CommandLineOption> {
        if self.kind != CommandLineOptionKind::List
            && self.kind != CommandLineOptionKind::ListOrElement
        {
            return None;
        }
        COMMAND_LINE_OPTION_ELEMENTS.get(self.name)
    }

    pub(crate) fn disallow_null_or_undefined(&self) -> bool {
        self.name == "extends"
    }
}

// CommandLineOption.Elements()
static COMMAND_LINE_OPTION_ELEMENTS: LazyLock<FxHashMap<&'static str, CommandLineOption>> =
    LazyLock::new(|| {
        let mut m = FxHashMap::default();
        m.insert(
            "lib",
            CommandLineOption {
                name: "lib",
                kind: CommandLineOptionKind::Enum, // libMap,
                default_value_description: DefaultValueDescription::Tristate(Tristate::Unknown),
                ..CommandLineOption::DEFAULT
            },
        );
        m.insert(
            "rootDirs",
            CommandLineOption {
                name: "rootDirs",
                kind: CommandLineOptionKind::String,
                is_file_path: true,
                ..CommandLineOption::DEFAULT
            },
        );
        m.insert(
            "typeRoots",
            CommandLineOption {
                name: "typeRoots",
                kind: CommandLineOptionKind::String,
                is_file_path: true,
                ..CommandLineOption::DEFAULT
            },
        );
        m.insert(
            "types",
            CommandLineOption {
                name: "types",
                kind: CommandLineOptionKind::String,
                ..CommandLineOption::DEFAULT
            },
        );
        m.insert(
            "moduleSuffixes",
            CommandLineOption {
                name: "moduleSuffixes",
                kind: CommandLineOptionKind::String,
                ..CommandLineOption::DEFAULT
            },
        );
        m.insert(
            "customConditions",
            CommandLineOption {
                name: "condition",
                kind: CommandLineOptionKind::String,
                ..CommandLineOption::DEFAULT
            },
        );
        m.insert(
            "plugins",
            CommandLineOption {
                name: "plugin",
                kind: CommandLineOptionKind::Object,
                ..CommandLineOption::DEFAULT
            },
        );
        // For tsconfig root options
        m.insert(
            "references",
            CommandLineOption {
                name: "references",
                kind: CommandLineOptionKind::Object,
                ..CommandLineOption::DEFAULT
            },
        );
        m.insert(
            "contentMappers",
            CommandLineOption {
                name: "contentMappers",
                kind: CommandLineOptionKind::Object,
                ..CommandLineOption::DEFAULT
            },
        );
        m.insert(
            "files",
            CommandLineOption {
                name: "files",
                kind: CommandLineOptionKind::String,
                ..CommandLineOption::DEFAULT
            },
        );
        m.insert(
            "include",
            CommandLineOption {
                name: "include",
                kind: CommandLineOptionKind::String,
                ..CommandLineOption::DEFAULT
            },
        );
        m.insert(
            "exclude",
            CommandLineOption {
                name: "exclude",
                kind: CommandLineOptionKind::String,
                ..CommandLineOption::DEFAULT
            },
        );
        m.insert(
            "extends",
            CommandLineOption {
                name: "extends",
                kind: CommandLineOptionKind::String,
                ..CommandLineOption::DEFAULT
            },
        );
        // Test infra options
        m.insert(
            "libFiles",
            CommandLineOption {
                name: "libFiles",
                kind: CommandLineOptionKind::String,
                ..CommandLineOption::DEFAULT
            },
        );
        m
    });

// CommandLineOption.EnumMap()
static COMMAND_LINE_OPTION_ENUM_MAP: LazyLock<
    FxHashMap<&'static str, &'static LazyLock<OrderedMap<&'static str, CompilerOptionsValue>>>,
> = LazyLock::new(|| {
    let mut m: FxHashMap<
        &'static str,
        &'static LazyLock<OrderedMap<&'static str, CompilerOptionsValue>>,
    > = FxHashMap::default();
    m.insert("lib", &LIB_MAP);
    m.insert("moduleResolution", &MODULE_RESOLUTION_OPTION_MAP);
    m.insert("module", &MODULE_OPTION_MAP);
    m.insert("target", &TARGET_OPTION_MAP);
    m.insert("moduleDetection", &MODULE_DETECTION_OPTION_MAP);
    m.insert("jsx", &JSX_OPTION_MAP);
    m.insert("newLine", &NEW_LINE_OPTION_MAP);
    m
});

// CommandLineOption.DeprecatedKeys()
static COMMAND_LINE_OPTION_DEPRECATED: LazyLock<FxHashMap<&'static str, FxHashSet<&'static str>>> =
    LazyLock::new(|| {
        let mut m = FxHashMap::default();
        m.insert("module", ["none", "amd", "system", "umd"].into_iter().collect());
        m.insert("moduleResolution", ["node", "classic", "node10"].into_iter().collect());
        m.insert("target", std::iter::once("es5").collect());
        m
    });

// Go `any` as used for option values: JSON values produced from the tsconfig AST (float64 numbers,
// []any arrays, *OrderedMap objects), command-line values (int numbers, []string lists) and the
// converted enum values stored in the enum maps.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum CompilerOptionsValue {
    // Go nil
    #[default]
    Null,
    Bool(bool),
    // Go float64
    Float(f64),
    // Go int
    Int(i64),
    String(String),
    // Go []any
    Array(Vec<CompilerOptionsValue>),
    // Go []any(nil) stored in an `any`: a non-nil interface holding a nil slice
    NilArray,
    // Go []string
    StringArray(Vec<String>),
    // Go *collections.OrderedMap[string, any]
    Object(OrderedMap<String, CompilerOptionsValue>),
    // Go struct{}{} (convertToJson of a file without a root expression)
    EmptyStruct,
    Tristate(Tristate),
    ModuleKind(ModuleKind),
    ModuleResolutionKind(ModuleResolutionKind),
    ModuleDetectionKind(ModuleDetectionKind),
    ScriptTarget(ScriptTarget),
    JsxEmit(JsxEmit),
    NewLineKind(NewLineKind),
}

impl CompilerOptionsValue {
    pub fn is_null(&self) -> bool {
        matches!(self, CompilerOptionsValue::Null)
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            CompilerOptionsValue::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&Vec<CompilerOptionsValue>> {
        match self {
            CompilerOptionsValue::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&OrderedMap<String, CompilerOptionsValue>> {
        match self {
            CompilerOptionsValue::Object(m) => Some(m),
            _ => None,
        }
    }

    pub(crate) fn as_object_mut(
        &mut self,
    ) -> Option<&mut OrderedMap<String, CompilerOptionsValue>> {
        match self {
            CompilerOptionsValue::Object(m) => Some(m),
            _ => None,
        }
    }

    // reflect.TypeOf(value).Kind() == reflect.Slice
    pub(crate) fn is_slice(&self) -> bool {
        matches!(
            self,
            CompilerOptionsValue::Array(_)
                | CompilerOptionsValue::StringArray(_)
                | CompilerOptionsValue::NilArray
        )
    }

    pub(crate) fn is_string(&self) -> bool {
        matches!(self, CompilerOptionsValue::String(_))
    }
}

impl From<&str> for CompilerOptionsValue {
    fn from(s: &str) -> Self {
        CompilerOptionsValue::String(s.to_string())
    }
}

impl From<String> for CompilerOptionsValue {
    fn from(s: String) -> Self {
        CompilerOptionsValue::String(s)
    }
}

impl From<bool> for CompilerOptionsValue {
    fn from(b: bool) -> Self {
        CompilerOptionsValue::Bool(b)
    }
}
