#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum ScriptKind {
    #[default]
    Unknown = 0,
    JS = 1,
    #[expect(
        clippy::upper_case_acronyms,
        reason = "The name matches the upstream Go or protocol vocabulary."
    )]
    JSX = 2,
    TS = 3,
    #[expect(
        clippy::upper_case_acronyms,
        reason = "The name matches the upstream Go or protocol vocabulary."
    )]
    TSX = 4,

    // Value 5 is reserved (formerly ScriptKindExternal).
    #[expect(
        clippy::upper_case_acronyms,
        reason = "The name matches the upstream Go or protocol vocabulary."
    )]
    JSON = 6,
    // Value 7 is reserved (formerly ScriptKindDeferred).
}

impl ScriptKind {
    fn string(self) -> &'static str {
        match self {
            ScriptKind::Unknown => "ScriptKindUnknown",
            ScriptKind::JS => "ScriptKindJS",
            ScriptKind::JSX => "ScriptKindJSX",
            ScriptKind::TS => "ScriptKindTS",
            ScriptKind::TSX => "ScriptKindTSX",
            ScriptKind::JSON => "ScriptKindJSON",
        }
    }
}

impl std::fmt::Display for ScriptKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.string())
    }
}
