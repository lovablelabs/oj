// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A failed load hook's error as Vite reports it: the plugin, the module it
//! failed to load and the cause. rolldown reports a plain load error as
//! "Could not load x - plugin `p` threw an error" and drops the cause, but
//! reports a diagnostic as is. The diagnostic's first line is Rollup's
//! `Could not load <id>`, so the overlay can read the module back from it
//! (Vite's load errors carry `id` and `plugin`, never a `loc`).

use std::borrow::Cow;

use rolldown_error::{BuildDiagnostic, CausedPlugin, DiagnosticOptions, EventKind};

const COULD_NOT_LOAD: &str = "Could not load ";

/// `diagnostic` rendered for the terminal or the overlay. rolldown's
/// `to_diagnostic` reads the process's current directory to shorten paths
/// and panics when that directory is gone (a plugin host boots with a real
/// chdir into its root, and a root can be deleted under a running process);
/// a diagnostic is the last thing that should fail then, so the paths stay
/// as they are instead.
pub fn render(diagnostic: &BuildDiagnostic) -> rolldown_error::Diagnostic {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
    diagnostic.to_diagnostic_with(&DiagnosticOptions { cwd })
}

/// `e`, raised by `plugin` loading `id`, as the diagnostic rolldown surfaces.
/// The cause is an inner diagnostic, so it renders as written: no source box,
/// no backtrace.
pub fn load_failed(plugin: Cow<'static, str>, id: &str, e: anyhow::Error) -> anyhow::Error {
    let shown = id.replace('\0', "\\0");
    let cause = BuildDiagnostic::oxc_error(
        arcstr::ArcStr::default(),
        id.to_string(),
        String::new(),
        format!("{COULD_NOT_LOAD}{shown}\n{e:#}"),
        Vec::new(),
        EventKind::PluginError,
    );
    BuildDiagnostic::plugin_error(CausedPlugin::new(plugin), cause.into()).into()
}

/// The plugin and module of a [`load_failed`] diagnostic, `None` for any other.
pub fn failed_load(diagnostic: &BuildDiagnostic) -> Option<(String, String)> {
    if !matches!(diagnostic.kind(), EventKind::PluginError) {
        return None;
    }
    let rendered = render(diagnostic);
    let plugin = rendered.kind();
    let text = rendered.to_string();
    // `[plugin] title`; ariadne pads a short code (`[p ]`).
    let first = text.split_once("] ")?.1.lines().next()?;
    let id = first.strip_prefix(COULD_NOT_LOAD)?.replace("\\0", "\0");
    Some((plugin, id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diagnostic(e: anyhow::Error) -> BuildDiagnostic {
        e.downcast::<BuildDiagnostic>().unwrap()
    }

    #[test]
    fn a_failed_load_names_its_plugin_and_module() {
        let d = diagnostic(load_failed(
            "oj:build".into(),
            "/app/src/a.scss",
            anyhow::anyhow!("sass error: Undefined variable."),
        ));
        let text = render(&d).to_string();
        assert!(
            text.starts_with(
                "[oj:build] Could not load /app/src/a.scss\nsass error: Undefined variable."
            ),
            "{text}"
        );
        assert!(!text.contains("╭─["), "no source box: {text}");
        assert_eq!(
            failed_load(&d),
            Some(("oj:build".to_string(), "/app/src/a.scss".to_string()))
        );
    }

    /// Rendering survives the process's current directory being gone (a
    /// plugin host's root deleted under it): the message is intact and the
    /// absolute path stands as written. The directory goes in a child
    /// process running this same test, so no other test of this binary
    /// loses its current directory meanwhile.
    #[test]
    fn a_diagnostic_renders_with_no_current_directory() {
        const CHILD: &str = "OJ_TEST_GONE_CWD_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "load_error::tests::a_diagnostic_renders_with_no_current_directory",
                ])
                .env(CHILD, "1")
                .status()
                .expect("the test binary runs itself");
            assert!(status.success(), "the child's run: {status}");
            return;
        }
        let gone = std::env::temp_dir().join(format!("oj-gone-cwd-{}", std::process::id()));
        std::fs::create_dir_all(&gone).unwrap();
        std::env::set_current_dir(&gone).unwrap();
        std::fs::remove_dir_all(&gone).unwrap();
        let d = diagnostic(load_failed(
            "p".into(),
            "/app/src/a.ts",
            anyhow::anyhow!("boom"),
        ));
        let text = render(&d).to_string();
        // ariadne pads a short plugin name (`[p ]`); the module and cause
        // follow as written.
        assert!(
            text.starts_with("[p") && text.contains("Could not load /app/src/a.ts\nboom"),
            "{text}"
        );
        assert_eq!(
            failed_load(&d),
            Some(("p".to_string(), "/app/src/a.ts".to_string()))
        );
    }

    #[test]
    fn a_virtual_id_round_trips() {
        let d = diagnostic(load_failed(
            "p".into(),
            "\0virtual:x",
            anyhow::anyhow!("boom"),
        ));
        let text = render(&d).to_string();
        assert!(text.contains("Could not load \\0virtual:x"), "{text}");
        assert_eq!(
            failed_load(&d).map(|(_, id)| id).as_deref(),
            Some("\0virtual:x")
        );
    }

    #[test]
    fn other_plugin_errors_name_no_module() {
        let d = BuildDiagnostic::plugin_error(
            CausedPlugin::new("p".into()),
            anyhow::anyhow!("transform exploded"),
        );
        assert_eq!(failed_load(&d), None);
    }
}
