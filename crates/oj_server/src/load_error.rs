// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A failed load hook's error as Vite reports it: the plugin, the module it
//! failed to load and the cause. rolldown reports a plain load error as
//! "Could not load x - plugin `p` threw an error" and drops the cause, but
//! reports a diagnostic as is. The diagnostic's first line is Rollup's
//! `Could not load <id>`, so the overlay can read the module back from it
//! (Vite's load errors carry `id` and `plugin`, never a `loc`).

use std::borrow::Cow;

use rolldown_error::{BuildDiagnostic, CausedPlugin, EventKind};

const COULD_NOT_LOAD: &str = "Could not load ";

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
    let rendered = diagnostic.to_diagnostic();
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
        let text = d.to_diagnostic().to_string();
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

    #[test]
    fn a_virtual_id_round_trips() {
        let d = diagnostic(load_failed(
            "p".into(),
            "\0virtual:x",
            anyhow::anyhow!("boom"),
        ));
        let text = d.to_diagnostic().to_string();
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
