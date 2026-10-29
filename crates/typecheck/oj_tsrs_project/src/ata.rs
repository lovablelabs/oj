// Go internal/project/ata is out of scope (docs/LSP.md): typings are never installed. Only the value type the
// project code compares is kept (ata.go:21, :27); `Session` never creates a typings installer.

use std::sync::Arc;

use tsrs_core::collections::Set;
use tsrs_core::{CompilerOptions, P, TypeAcquisition};

#[derive(Clone, Debug, Default)]
pub struct TypingsInfo {
    pub(crate) type_acquisition: Option<TypeAcquisition>,
    pub(crate) compiler_options: Option<P<CompilerOptions>>,
    pub(crate) unresolved_imports: Option<Arc<Set<String>>>,
}

impl TypingsInfo {
    // ata.go:27
    pub(crate) fn equals(&self, other: &TypingsInfo) -> bool {
        TypeAcquisition::equals(self.type_acquisition.as_ref(), other.type_acquisition.as_ref())
            && self.compiler_options.unwrap().get_allow_js()
                == other.compiler_options.unwrap().get_allow_js()
            && match (&self.unresolved_imports, &other.unresolved_imports) {
                (None, None) => true,
                (Some(a), Some(b)) => a.equals(b),
                (Some(a), None) | (None, Some(a)) => a.is_empty(),
            }
    }
}

// Go `ata.NpmExecutor`: the session keeps the hook; nothing calls it while ATA is not ported.
pub trait NpmExecutor: Send + Sync {
    fn npm_install(&self, cwd: &str, npm_install_args: &[String]) -> Result<Vec<u8>, String>;
}
