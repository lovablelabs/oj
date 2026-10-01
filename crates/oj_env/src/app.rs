//! The app's env as Vite resolves it, for one root and mode.

use std::path::Path;

use crate::defines::has_prefix;
use crate::{import_meta_env_defines_with, load_with, resolve_node_env, with_process_env, Env};

/// The `.env` files of one root and mode, the NODE_ENV they resolve to, and
/// the prefixes that decide which vars reach client code.
#[derive(Debug, Clone)]
pub struct AppEnv {
    mode: String,
    loaded: Vec<(String, String)>,
    prefixes: Vec<String>,
    node_env: String,
}

impl AppEnv {
    /// `default_node_env` is the command's: `production` for build,
    /// `development` for serve.
    pub fn resolve(
        env: &Env,
        dir: &Path,
        mode: &str,
        prefixes: Vec<String>,
        default_node_env: &str,
    ) -> Self {
        let loaded = load_with(env, dir, mode);
        let node_env = resolve_node_env(env.knobs.node_env.as_deref(), &loaded, default_node_env);
        AppEnv {
            mode: mode.to_string(),
            loaded,
            prefixes,
            node_env,
        }
    }

    pub fn mode(&self) -> &str {
        &self.mode
    }

    pub fn node_env(&self) -> &str {
        &self.node_env
    }

    pub fn is_production(&self) -> bool {
        self.node_env == "production"
    }

    /// The vars the `.env` files define.
    pub fn loaded(&self) -> &[(String, String)] {
        &self.loaded
    }

    pub fn prefixes(&self) -> &[String] {
        &self.prefixes
    }

    pub fn has_prefix(&self, key: &str) -> bool {
        has_prefix(key, &self.prefix_refs())
    }

    fn prefix_refs(&self) -> Vec<&str> {
        self.prefixes.iter().map(String::as_str).collect()
    }

    /// The prefixed vars client code sees: the `.env` files, then the process
    /// env, then `delta` (env changes made by plugin `config()` hooks).
    pub fn exposed_vars(&self, env: &Env, delta: &[(String, String)]) -> Vec<(String, String)> {
        let process = env
            .vars()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .chain(delta.iter().cloned());
        with_process_env(self.loaded.clone(), process, &self.prefix_refs())
    }

    /// The `import.meta.env.*` defines (DEV/PROD follow NODE_ENV).
    pub fn import_meta_env_defines(
        &self,
        env: &Env,
        delta: &[(String, String)],
        base_url: &str,
        ssr: bool,
    ) -> Vec<(String, String)> {
        import_meta_env_defines_with(
            &self.exposed_vars(env, delta),
            &self.mode,
            !self.is_production(),
            base_url,
            &self.prefix_refs(),
            ssr,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in files {
            std::fs::write(dir.path().join(name), body).unwrap();
        }
        dir
    }

    #[test]
    fn files_then_process_then_plugin_delta() {
        let dir = dir_with(&[(".env", "VITE_A=file\nVITE_B=file\nSECRET=x\n")]);
        let env = Env::from_vars([("VITE_B", "shell"), ("VITE_C", "shell"), ("OTHER", "y")]);
        let app = AppEnv::resolve(
            &env,
            dir.path(),
            "development",
            vec!["VITE_".into()],
            "development",
        );
        let delta = [("VITE_C".to_string(), "plugin".to_string())];
        let vars = app.exposed_vars(&env, &delta);
        assert_eq!(
            vars,
            [
                ("SECRET", "x"),
                ("VITE_A", "file"),
                ("VITE_B", "shell"),
                ("VITE_C", "plugin")
            ]
            .map(|(k, v)| (k.to_string(), v.to_string()))
        );
        assert!(!app.is_production());
    }

    #[test]
    fn node_env_comes_from_the_snapshot_then_the_files() {
        let dir = dir_with(&[(".env", "NODE_ENV=development\n")]);
        let shell = Env::from_vars([("NODE_ENV", "production")]);
        let app = AppEnv::resolve(&shell, dir.path(), "production", vec![], "production");
        assert_eq!(app.node_env(), "production", "the shell wins");
        let none = Env::from_vars(Vec::<(String, String)>::new());
        let app = AppEnv::resolve(&none, dir.path(), "production", vec![], "production");
        assert_eq!(app.node_env(), "development", ".env NODE_ENV=development");
    }

    #[test]
    fn the_process_snapshot_is_taken_once() {
        let first = crate::init() as *const Env;
        assert_eq!(first, crate::get() as *const Env);
        assert_eq!(
            first,
            crate::init() as *const Env,
            "a second init never replaces it"
        );
    }
}
