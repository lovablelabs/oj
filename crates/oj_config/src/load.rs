use crate::schema::*;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config parse error in {0}: {1}")]
    Parse(PathBuf, String),
    #[error("config evaluation error in {0}: {1}")]
    Eval(PathBuf, String),
    #[error("config schema error in {0}: {1}")]
    Schema(PathBuf, String),
}

const EVAL_TIME_LIMIT: std::time::Duration = std::time::Duration::from_secs(5);

const EVAL_MEMORY_LIMIT: usize = 64 * 1024 * 1024;

const CANDIDATES: &[&str] = &[
    "oj.config.ts",
    "oj.config.mjs",
    "oj.config.js",
    "oj.config.json",
];

pub fn load(root: &Path) -> Result<OjConfig, ConfigError> {
    load_with(root, "serve", "development")
}

pub fn load_with(root: &Path, command: &str, mode: &str) -> Result<OjConfig, ConfigError> {
    let Some(path) = CANDIDATES
        .iter()
        .map(|c| root.join(c))
        .find(|p| p.is_file())
    else {
        return Ok(OjConfig::default());
    };
    let source = std::fs::read_to_string(&path)
        .map_err(|e| ConfigError::Parse(path.clone(), e.to_string()))?;

    let json = if path.extension().and_then(|e| e.to_str()) == Some("json") {
        source
    } else {
        evaluate(&path, &source, command, mode)?
    };

    let value: serde_json::Value = serde_json::from_str(&json)
        .map_err(|e| ConfigError::Schema(path.clone(), e.to_string()))?;
    match &value {
        serde_json::Value::Object(_) => {
            serde_json::from_value(value).map_err(|e| ConfigError::Schema(path, e.to_string()))
        }
        serde_json::Value::Null => Err(ConfigError::Eval(
            path,
            "no config was exported; a config file must `export default` an object".into(),
        )),
        other => Err(ConfigError::Schema(
            path,
            format!("expected an object, found {}", json_type_name(other)),
        )),
    }
}

fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

fn evaluate(path: &Path, source: &str, command: &str, mode: &str) -> Result<String, ConfigError> {
    let js = strip_types(path, source)?;
    let script = to_script(&js);

    let rt = rquickjs::Runtime::new()
        .map_err(|e| ConfigError::Eval(path.to_path_buf(), e.to_string()))?;
    rt.set_memory_limit(EVAL_MEMORY_LIMIT);
    let deadline = std::time::Instant::now() + EVAL_TIME_LIMIT;
    rt.set_interrupt_handler(Some(Box::new(move || {
        std::time::Instant::now() >= deadline
    })));
    let ctx = rquickjs::Context::full(&rt)
        .map_err(|e| ConfigError::Eval(path.to_path_buf(), e.to_string()))?;

    ctx.with(|ctx| {
        let env_obj: String = std::env::vars()
            .map(|(k, v)| {
                format!(
                    "{}:{}",
                    serde_json::to_string(&k).unwrap(),
                    serde_json::to_string(&v).unwrap()
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let prelude = format!(
            "var defineConfig = function (x) {{ return x; }};\n\
             var process = {{ env: {{ {env_obj} }} }};\n\
             var globalThis = globalThis || this;\n"
        );
        let env_arg = format!(
            "{{ command: {}, mode: {}, isSsrBuild: false, isPreview: false }}",
            serde_json::to_string(command).unwrap(),
            serde_json::to_string(mode).unwrap()
        );
        let full = format!(
            "{prelude}{script}\n\
             var __ojC = globalThis.__ojConfig;\n\
             if (typeof __ojC === 'function') __ojC = __ojC({env_arg});\n\
             function __ojMark(o) {{\n\
               if (!o || typeof o !== 'object') return;\n\
               for (var k in o) {{\n\
                 var v = o[k];\n\
                 if (typeof v === 'function') o[k] = '__oj_fn__';\n\
                 else if (v instanceof RegExp) o[k] = {{ __oj_regex__: v.source }};\n\
                 else __ojMark(v);\n\
               }}\n\
             }}\n\
             __ojMark(__ojC && __ojC.build && (__ojC.build.rolldownOptions || __ojC.build.rollupOptions));\n\
             __ojMark(__ojC && __ojC.css && __ojC.css.modules);\n\
             JSON.stringify(__ojC ?? null)"
        );
        let result: rquickjs::Value = ctx.eval(full).map_err(|e| {
            let caught = ctx.catch();
            let mut detail = caught
                .as_exception()
                .map(|ex| ex.to_string())
                .unwrap_or_else(|| format!("{e}"));
            if std::time::Instant::now() >= deadline {
                detail = format!(
                    "evaluation exceeded the {}s limit; a config file must not \
                     block (no infinite loops, no blocking work)",
                    EVAL_TIME_LIMIT.as_secs()
                );
            }
            if detail.contains("is not defined") {
                detail.push_str(
                    "\nnote: oj.config is evaluated in a sandbox without module imports; \
                     if this file is a plugins array, put it in oj.plugins.mjs instead",
                );
            }
            ConfigError::Eval(path.to_path_buf(), detail)
        })?;
        result
            .get::<String>()
            .map_err(|e| ConfigError::Eval(path.to_path_buf(), e.to_string()))
    })
}

fn strip_types(path: &Path, source: &str) -> Result<String, ConfigError> {
    use oxc_allocator::Allocator;
    use oxc_codegen::Codegen;
    use oxc_parser::Parser;
    use oxc_semantic::SemanticBuilder;
    use oxc_span::SourceType;
    use oxc_transformer::{TransformOptions, Transformer};

    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::ts());
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked {
        return Err(ConfigError::Parse(
            path.to_path_buf(),
            "syntax error".into(),
        ));
    }
    let mut program = parsed.program;
    let scoping = SemanticBuilder::new()
        .with_enum_eval(true)
        .build(&program)
        .semantic
        .into_scoping();
    let ret = Transformer::new(&allocator, path, &TransformOptions::default())
        .build_with_scoping(scoping, &mut program);
    if !ret.diagnostics.is_empty() {
        return Err(ConfigError::Parse(
            path.to_path_buf(),
            ret.diagnostics
                .iter()
                .map(|d| d.to_string())
                .collect::<Vec<_>>()
                .join("; "),
        ));
    }
    Ok(Codegen::new().build(&program).code)
}

fn to_script(js: &str) -> String {
    let mut out = String::with_capacity(js.len());
    for line in js.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("import ") || trimmed.starts_with("import{") {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("export default ") {
            out.push_str("globalThis.__ojConfig = ");
            out.push_str(rest);
            out.push('\n');
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::build::environment_build_bool;
    use crate::resolve::{resolve_alias, resolve_conditions};
    use crate::server::{config_defines, environment_defines};

    /// A config may declare a TypeScript `enum`, and lowering one needs scoping
    /// built with `with_enum_eval`: without it the transform aborts the process
    /// instead of loading the config.
    #[test]
    fn a_config_that_declares_an_enum_loads() {
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("oj-config-enum-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("oj.config.ts"),
            "enum Port { Dev = 5199 }\nexport default { server: { port: Port.Dev as number } };",
        )
        .unwrap();

        let config = crate::load(&dir).expect("a config with an enum must load");
        assert_eq!(config.server.unwrap().port, Some(5199));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn eval_config_in(label: &str, src: &str) -> OjConfig {
        let dir = std::env::temp_dir().join(format!("oj-cfg-{}-{label}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("oj.config.ts"), src).unwrap();
        let cfg = load(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        cfg
    }

    #[test]
    fn no_config_is_default() {
        let cfg = load(std::path::Path::new("/nonexistent-oj-root")).unwrap();
        assert!(cfg.server.is_none());
    }

    #[test]
    fn server_hmr_false_parses_as_disabled() {
        let off: OjConfig = serde_json::from_str(r#"{"server":{"hmr":false}}"#).unwrap();
        assert!(off.server.unwrap().hmr.unwrap().is_disabled());
        let on: OjConfig = serde_json::from_str(r#"{"server":{"hmr":true}}"#).unwrap();
        assert!(!on.server.unwrap().hmr.unwrap().is_disabled());
        let obj: OjConfig =
            serde_json::from_str(r#"{"server":{"hmr":{"overlay":false}}}"#).unwrap();
        assert!(!obj.server.unwrap().hmr.unwrap().is_disabled());
        let empty: OjConfig = serde_json::from_str(r#"{"server":{"port":3000}}"#).unwrap();
        assert!(empty.server.unwrap().hmr.is_none());
    }

    #[test]
    fn evaluates_ts_config_with_types_and_define_config() {
        let cfg = eval_config_in(
            "define",
            "import { defineConfig } from \"oj\";\n\
             export default defineConfig({\n\
               server: { port: 3000, proxy: { \"/api\": \"http://localhost:8080\" } },\n\
               resolve: { alias: { \"@\": \"./src\" } as Record<string,string> },\n\
             });\n",
        );
        let server = cfg.server.unwrap();
        assert_eq!(server.port, Some(3000));
        assert_eq!(
            server.proxy.unwrap().get("/api").unwrap().target(),
            "http://localhost:8080"
        );
        assert_eq!(
            cfg.resolve.unwrap().alias.unwrap().get("@").unwrap(),
            "./src"
        );
    }

    #[test]
    fn function_config_receives_command_and_mode() {
        let src = "export default ({ command, mode }) => ({ base: command === \"build\" ? \"/prod/\" : \"/dev/\", define: { __M__: mode } });\n";
        let cfg = eval_config_in("fnform", src);
        assert_eq!(cfg.base.as_deref(), Some("/dev/"));
        let defines: std::collections::BTreeMap<_, _> = config_defines(&cfg).into_iter().collect();
        assert_eq!(defines.get("__M__").unwrap(), "development");

        let dir = std::env::temp_dir().join(format!("oj-cfg-fnbuild-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("oj.config.js"), src).unwrap();
        let cfg = load_with(&dir, "build", "production").unwrap();
        assert_eq!(cfg.base.as_deref(), Some("/prod/"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn undefined_reference_config_gives_plugins_hint() {
        let err = evaluate(
            std::path::Path::new("oj.config.mjs"),
            "export default [tailwindcss()];\n",
            "serve",
            "development",
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("tailwindcss"), "{msg}");
        assert!(msg.contains("oj.plugins.mjs"), "{msg}");
    }

    #[test]
    fn defineconfig_function_form_works() {
        let cfg = eval_config_in(
            "definefn",
            "import { defineConfig } from \"oj\";\nexport default defineConfig(({ mode }) => ({ base: mode === \"development\" ? \"/dev/\" : \"/prod/\" }));\n",
        );
        assert_eq!(cfg.base.as_deref(), Some("/dev/"));
    }

    #[test]
    fn computed_values_and_process_env_work() {
        unsafe { std::env::set_var("OJ_TEST_PORT", "4321") };
        let cfg = eval_config_in(
            "computed",
            "export default { server: { port: Number(process.env.OJ_TEST_PORT), open: 1 > 0 } };\n",
        );
        let server = cfg.server.unwrap();
        assert_eq!(server.port, Some(4321));
        assert_eq!(server.open, Some(true));
    }

    #[test]
    fn per_environment_resolution_and_precedence() {
        let cfg = eval_config_in(
            "env-resolvers",
            "export default {\n\
               define: { __FLAG__: \"true\", __COUNT__: 3 },\n\
               resolve: { conditions: [\"custom\"], alias: { \"@\": \"/src\", \"old\": \"/legacy\" } },\n\
               environments: {\n\
                 ssr: {\n\
                   build: { minify: false },\n\
                   resolve: { conditions: [\"node-only\"], alias: { \"old\": \"/ssr-legacy\" } },\n\
                   define: { __SSR__: true },\n\
                 },\n\
               },\n\
             };\n",
        );
        let defines: std::collections::BTreeMap<_, _> = config_defines(&cfg).into_iter().collect();
        assert_eq!(defines.get("__FLAG__").unwrap(), "true");
        assert_eq!(defines.get("__COUNT__").unwrap(), "3");

        // A user list replaces the defaults (no implicit module/dev-prod, like
        // Vite) but import/default are always kept so exports maps still match.
        assert_eq!(
            resolve_conditions(&cfg, "ssr"),
            vec![
                "node-only".to_string(),
                "import".to_string(),
                "default".to_string()
            ]
        );
        assert_eq!(
            resolve_conditions(&cfg, "client"),
            vec![
                "custom".to_string(),
                "import".to_string(),
                "default".to_string()
            ]
        );

        assert_eq!(
            resolve_alias(&cfg, "ssr"),
            vec![
                ("@".to_string(), "/src".to_string()),
                ("old".to_string(), "/ssr-legacy".to_string())
            ]
        );
        assert_eq!(
            resolve_alias(&cfg, "client"),
            vec![
                ("@".to_string(), "/src".to_string()),
                ("old".to_string(), "/legacy".to_string())
            ]
        );

        assert_eq!(environment_build_bool(&cfg, "ssr", "minify"), Some(false));
        assert_eq!(environment_build_bool(&cfg, "ssr", "sourcemap"), None);
        let ssr_defines: std::collections::BTreeMap<_, _> =
            environment_defines(&cfg, "ssr").into_iter().collect();
        assert_eq!(ssr_defines.get("__SSR__").unwrap(), "true");
        assert!(environment_defines(&cfg, "client").is_empty());
    }

    #[test]
    fn json_config_loads_directly() {
        let dir = std::env::temp_dir().join(format!("oj-cfg-json-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("oj.config.json"),
            r#"{"appType":"mpa","base":"/app/"}"#,
        )
        .unwrap();
        let cfg = load(&dir).unwrap();
        assert_eq!(cfg.app_type.as_deref(), Some("mpa"));
        assert_eq!(cfg.base.as_deref(), Some("/app/"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
