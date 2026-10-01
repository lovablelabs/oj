/// Styled output only on a terminal, and never with `NO_COLOR` set.
fn styled() -> bool {
    use std::io::IsTerminal;
    !oj_env::get().knobs.no_color && std::io::stdout().is_terminal()
}

#[inline]
pub fn cobalt(s: &str) -> String {
    if styled() {
        format!("\x1b[1;38;2;42;51;212m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

#[inline]
pub fn cell(s: &str) -> String {
    if styled() {
        format!("\x1b[48;2;255;255;255m\x1b[1;38;2;42;51;212m {s} \x1b[0m")
    } else {
        s.to_string()
    }
}

#[inline]
pub fn oj_brand() -> String {
    cell("oj")
}

pub fn link(url: &str, text: &str) -> String {
    if styled() {
        format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
    } else {
        text.to_string()
    }
}

pub(crate) fn oj_tag() -> String {
    if styled() {
        format!("{} ", oj_brand())
    } else {
        "oj:".to_string()
    }
}

pub fn boot_phase(label: &str) {
    if !oj_env::get().knobs.boot_phases {
        return;
    }
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    eprintln!("[oj-phase] {ms} {label}");
}

/// Vite's `server.open`: launch the browser once bound. `BROWSER=none` disables,
/// any other `BROWSER` value names the command (the `open` package's convention).
pub fn open_browser(url: &str) {
    let browser = oj_env::get().knobs.browser.clone();
    if browser.as_deref() == Some("none") {
        return;
    }
    let mut cmd = match browser {
        Some(b) => std::process::Command::new(b),
        None if cfg!(target_os = "macos") => std::process::Command::new("open"),
        None if cfg!(target_os = "windows") => {
            let mut c = std::process::Command::new("cmd");
            c.args(["/C", "start", ""]);
            c
        }
        None => std::process::Command::new("xdg-open"),
    };
    cmd.arg(url)
        // The launcher takes a URL, not a path, and the process cwd may be a
        // since-deleted app root (host boot chdir), which fails the spawn.
        .current_dir(std::env::temp_dir())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Err(err) = cmd.spawn() {
        eprintln!("oj: could not open the browser: {err}");
    }
}

/// A `lovable:boot-progress` custom HMR frame (editor boot narration).
/// ssrModules + clientModules must be non-negative ints or the editor drops the frame.
pub fn boot_progress_frame(
    ssr_modules: usize,
    client_modules: usize,
    client_idle_ms: Option<u64>,
) -> String {
    serde_json::json!({
        "type": "custom",
        "event": "lovable:boot-progress",
        "data": {
            "ssrModules": ssr_modules,
            "clientModules": client_modules,
            "ssrIdleMs": serde_json::Value::Null,
            "clientIdleMs": client_idle_ms,
            "buildError": serde_json::Value::Null,
        },
    })
    .to_string()
}

/// A `lovable:update-progress` custom HMR frame (editor narration).
/// batch is monotonic; trigger is one of "flush" | "watch" | "restart".
pub fn update_progress_frame(
    batch: u64,
    trigger: &str,
    ssr_modules: usize,
    client_modules: usize,
    idle_ms: Option<u64>,
    done: bool,
) -> String {
    serde_json::json!({
        "type": "custom",
        "event": "lovable:update-progress",
        "data": {
            "batch": batch,
            "trigger": trigger,
            "ssrModules": ssr_modules,
            "clientModules": client_modules,
            "idleMs": idle_ms,
            "done": done,
            "buildError": serde_json::Value::Null,
        },
    })
    .to_string()
}
