use rustc_hash::FxHashSet;
use std::sync::LazyLock;

// require('module').builtinModules.filter(x => !x.match(/^(?:_|node:)/))
static UNPREFIXED_NODE_CORE_MODULES: &[&str] = &[
    "assert",
    "assert/strict",
    "async_hooks",
    "buffer",
    "child_process",
    "cluster",
    "console",
    "constants",
    "crypto",
    "dgram",
    "diagnostics_channel",
    "dns",
    "dns/promises",
    "domain",
    "events",
    "fs",
    "fs/promises",
    "http",
    "http2",
    "https",
    "inspector",
    "inspector/promises",
    "module",
    "net",
    "os",
    "path",
    "path/posix",
    "path/win32",
    "perf_hooks",
    "process",
    "punycode",
    "querystring",
    "readline",
    "readline/promises",
    "repl",
    "stream",
    "stream/consumers",
    "stream/promises",
    "stream/web",
    "string_decoder",
    "sys",
    "timers",
    "timers/promises",
    "tls",
    "trace_events",
    "tty",
    "url",
    "util",
    "util/types",
    "v8",
    "vm",
    "wasi",
    "worker_threads",
    "zlib",
];

// require('module').builtinModules.filter(x => x.startsWith('node:'))
static EXCLUSIVELY_PREFIXED_NODE_CORE_MODULES: &[&str] =
    &["node:quic", "node:sea", "node:sqlite", "node:test", "node:test/reporters"];

static NODE_CORE_MODULES: LazyLock<FxHashSet<String>> = LazyLock::new(|| {
    let mut node_core_modules = FxHashSet::default();
    node_core_modules.extend(
        UNPREFIXED_NODE_CORE_MODULES
            .iter()
            .flat_map(|unprefixed| [unprefixed.to_string(), format!("node:{unprefixed}")]),
    );
    node_core_modules.extend(
        EXCLUSIVELY_PREFIXED_NODE_CORE_MODULES.iter().map(std::string::ToString::to_string),
    );
    node_core_modules
});

pub fn is_unprefixed_node_core_module(name: &str) -> bool {
    UNPREFIXED_NODE_CORE_MODULES.contains(&name)
}

pub fn is_exclusively_prefixed_node_core_module(name: &str) -> bool {
    EXCLUSIVELY_PREFIXED_NODE_CORE_MODULES.contains(&name)
}

pub fn node_core_modules() -> &'static FxHashSet<String> {
    &NODE_CORE_MODULES
}
