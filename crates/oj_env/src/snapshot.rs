//! The process environment, captured once.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::sync::OnceLock;

use crate::Knobs;

/// An immutable copy of the process environment and the oj settings parsed
/// from it. Production code reaches the process-wide one through [`get`];
/// tests build their own with [`Env::from_vars`].
#[derive(Debug, Clone)]
pub struct Env {
    vars: BTreeMap<OsString, OsString>,
    pub knobs: Knobs,
}

impl Env {
    pub fn from_vars_os(vars: impl IntoIterator<Item = (OsString, OsString)>) -> Self {
        let vars: BTreeMap<OsString, OsString> = vars.into_iter().collect();
        let knobs = Knobs::parse(|key| vars.get(OsStr::new(key)).map(OsString::as_os_str));
        Env { vars, knobs }
    }

    pub fn from_vars<K: Into<String>, V: Into<String>>(
        vars: impl IntoIterator<Item = (K, V)>,
    ) -> Self {
        Self::from_vars_os(
            vars.into_iter()
                .map(|(k, v)| (OsString::from(k.into()), OsString::from(v.into()))),
        )
    }

    /// `key` when set and valid UTF-8 (`std::env::var` semantics).
    pub fn var(&self, key: &str) -> Option<&str> {
        self.var_os(key)?.to_str()
    }

    pub fn var_os(&self, key: &str) -> Option<&OsStr> {
        self.vars.get(OsStr::new(key)).map(OsString::as_os_str)
    }

    /// Every UTF-8 variable, sorted by name.
    pub fn vars(&self) -> impl Iterator<Item = (&str, &str)> {
        self.vars
            .iter()
            .filter_map(|(k, v)| Some((k.to_str()?, v.to_str()?)))
    }
}

static ENV: OnceLock<Env> = OnceLock::new();

/// Captures the process environment. The first call wins and later calls
/// return that same snapshot, so it can never be replaced. `oj`'s `main` calls
/// it once its own startup writes are done, before any thread starts.
pub fn init() -> &'static Env {
    ENV.get_or_init(|| Env::from_vars_os(std::env::vars_os()))
}

/// The process snapshot ([`init`] on first use).
pub fn get() -> &'static Env {
    init()
}
