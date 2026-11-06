use std::sync::LazyLock;

// Go defaults to `7.1.0-beta` and can override it with ldflags. tsrs can override it at build time with
// TSRS_TS_VERSION to match a tsgo binary's tsbuildinfo `version`; default builds keep Go's source default.
const VERSION: &str = match option_env!("TSRS_TS_VERSION") {
    Some(v) => v,
    None => "7.1.0-beta",
};

pub fn version() -> &'static str {
    VERSION
}

static VERSION_MAJOR_MINOR: LazyLock<&'static str> = LazyLock::new(|| {
    let mut seen_major = false;
    let i = VERSION.char_indices().find(|&(_, r)| {
        if r == '.' {
            if seen_major {
                return true;
            }
            seen_major = true;
        }
        false
    });
    match i {
        None => panic!("invalid version string: {VERSION}"),
        Some((i, _)) => &VERSION[..i],
    }
});

pub fn version_major_minor() -> &'static str {
    &VERSION_MAJOR_MINOR
}
