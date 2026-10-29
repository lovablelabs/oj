use bitflags::bitflags;

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
    pub struct EmitFlags: u32 {
        const SingleLine = 1 << 0; // The contents of this node should be emitted on a single line.
        const MultiLine = 1 << 1; // The contents of this node should be emitted on multiple lines.
        const NoLeadingSourceMap = 1 << 2; // Do not emit a leading source map location for this node.
        const NoTrailingSourceMap = 1 << 3; // Do not emit a trailing source map location for this node.
        const NoNestedSourceMaps = 1 << 4; // Do not emit source map locations for children of this node.
        const NoTokenLeadingSourceMaps = 1 << 5; // Do not emit leading source map location for token nodes.
        const NoTokenTrailingSourceMaps = 1 << 6; // Do not emit trailing source map location for token nodes.
        const NoLeadingComments = 1 << 7; // Do not emit leading comments for this node.
        const NoTrailingComments = 1 << 8; // Do not emit trailing comments for this node.
        const NoNestedComments = 1 << 9; // Do not emit nested comments for children of this node.
        const HelperName = 1 << 10; // The Identifier refers to an *unscoped* emit helper (one that is emitted at the top of the file)
        const ExportName = 1 << 11; // Ensure an export prefix is added for an identifier that points to an exported declaration with a local name (see SymbolFlags.ExportHasLocal).
        const LocalName = 1 << 12; // Ensure an export prefix is not added for an identifier that points to an exported declaration.
        const Indented = 1 << 13; // Adds an explicit extra indentation level for class and function bodies when printing (used to match old emitter).
        const NoIndentation = 1 << 14; // Do not indent the node.
        const ReuseTempVariableScope = 1 << 15; // Reuse the existing temp variable scope during emit.
        const CustomPrologue = 1 << 16; // Treat the statement as if it were a prologue directive (NOTE: Prologue directives are *not* transformed).
        const NoAsciiEscaping = 1 << 17; // When synthesizing nodes that lack an original node or textSourceNode, we want to write the text on the node with ASCII escaping substitutions.
        const ExternalHelpers = 1 << 18; // This source file has external helpers
        const StartOnNewLine = 1 << 19; // Start this node on a new line
        const IndirectCall = 1 << 20; // Emit CallExpression as an indirect call: `(0, f)()`
        const AsyncFunctionBody = 1 << 21; // The node was originally an async function body.
        const NoLexicalArguments = 1 << 22; // Do not capture `arguments` for this arrow function. Set on arrows lowered from class static blocks, where `arguments` is an error; preserves Strada's emit behavior.
        const TransformPrivateStaticElements = 1 << 23; // Indicates static private elements in a file or class should be transformed regardless of --target (used by esDecorators transform).
        const NoLexicalThis = 1 << 24; // Do not capture `this` for this node's subtree. Set on relocated static initializers, where `this` is handled by the class fields transform.
        const InternalSymbolName = 1 << 25; // Port-only: this string literal's text is a symbol name whose leading U+007F is the port's stand-in for Go's internal-name prefix byte 0xFE.

        const None = 0;
        const NoSourceMap = Self::NoLeadingSourceMap.bits() | Self::NoTrailingSourceMap.bits(); // Do not emit a source map location for this node.
        const NoTokenSourceMaps = Self::NoTokenLeadingSourceMaps.bits() | Self::NoTokenTrailingSourceMaps.bits(); // Do not emit source map locations for tokens of this node.
        const NoComments = Self::NoLeadingComments.bits() | Self::NoTrailingComments.bits(); // Do not emit comments for this node.
    }
}
