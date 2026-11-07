// source.go:5
pub trait Source {
    fn text(&self) -> &str;
    fn file_name(&self) -> &str;
    fn ecma_line_map(&self) -> &[u32];
}

// Go: *ast.SourceFile satisfies Source implicitly through its Text/FileName/ECMALineMap methods.
impl Source for tsrs_ast::SourceFile {
    fn text(&self) -> &str {
        tsrs_ast::SourceFile::text(self)
    }
    fn file_name(&self) -> &str {
        tsrs_ast::SourceFile::file_name(self)
    }
    fn ecma_line_map(&self) -> &[u32] {
        tsrs_ast::SourceFile::ecma_line_map(self)
    }
}
