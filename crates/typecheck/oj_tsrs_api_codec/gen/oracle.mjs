// Regenerates tests/golden/*.bin with the Go `tsc --api` server built from the pinned source, the reference
// implementation this crate must match byte for byte.
//
// Usage:
//   # server: built from ts-ref at the workspace pin (Cargo.toml [workspace.metadata.typescript].commit)
//   (cd ts-ref/tsc && GOWORK=off go build -o /tmp/tsgo/tsc ./cmd/tsc)
//   # client: the JavaScript API client of the same source (the pinned npm package)
//   pnpm --dir /tmp/tsoracle add typescript@7.1.0-beta
//   TSRS_CODEC_TSC=/tmp/tsgo/tsc TSRS_CODEC_ORACLE=/tmp/tsoracle/node_modules/typescript \
//     node crates/tsrs_api_codec/gen/oracle.mjs
//
// The script refuses to run unless ts-ref is checked out at the pin (or TSRS_CODEC_COMMIT during an in-progress
// source upgrade) and TSRS_CODEC_TSC is set, and records the
// source SHA, the Go version and the server executable's sha256 in tests/golden/manifest.json.
//
// For every fixture it asks the server to `createSourceFile` (parse + bind through the server's parse cache,
// then `encoder.EncodeSourceFile`) and stores the response with the session-specific source file ID and lease
// fields (header bytes 44..60) zeroed. It also stores, for the decoder tests, the bytes the JavaScript client's
// own encoder (`encodeNode`/`encodeSourceFile`, used for printNode and friends) produces for each tree.
import * as fs from "node:fs";
import * as path from "node:path";
import { pathToFileURL } from "node:url";
import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";

const crateRoot = path.resolve(import.meta.dirname, "..");
const oracle = process.env.TSRS_CODEC_ORACLE;
if (!oracle) {
    console.error("set TSRS_CODEC_ORACLE to an installed typescript@7.1.0-beta package directory");
    process.exit(2);
}
const tsc = process.env.TSRS_CODEC_TSC;
if (!tsc) {
    console.error("set TSRS_CODEC_TSC to a tsc executable built from ts-ref at the pinned commit");
    process.exit(2);
}
const repoRoot = path.resolve(crateRoot, "../..");
const pin = process.env.TSRS_CODEC_COMMIT ?? fs.readFileSync(path.join(repoRoot, "Cargo.toml"), "utf8").match(/^commit = "([0-9a-f]{40})"/m)[1];
const refHead = execFileSync("git", ["-C", path.join(repoRoot, "ts-ref"), "rev-parse", "HEAD"], { encoding: "utf8" }).trim();
const refDirty = execFileSync("git", ["-C", path.join(repoRoot, "ts-ref"), "status", "--porcelain", "--", "tsc"], { encoding: "utf8" }).trim();
if (refHead !== pin || refDirty) {
    console.error(`ts-ref is at ${refHead}${refDirty ? " (modified)" : ""}, expected the pin ${pin}`);
    process.exit(2);
}
const tscSha256 = createHash("sha256").update(fs.readFileSync(tsc)).digest("hex");
const tscVersion = execFileSync(tsc, ["--version"], { encoding: "utf8" }).trim();
const pkg = JSON.parse(fs.readFileSync(path.join(oracle, "package.json"), "utf8"));
const imp = rel => import(pathToFileURL(path.join(oracle, rel)).href);
const { API } = await imp("dist/api/sync/api.js");
const { encodeSourceFile, encodeNode } = await imp("dist/api/node/encoder.js");

const fixturesDir = path.join(crateRoot, "tests/fixtures");
const goldenDir = path.join(crateRoot, "tests/golden");
fs.mkdirSync(goldenDir, { recursive: true });

// Source text with unpaired surrogates cannot be sent: the pinned server rejects it while unmarshalling the
// request ("jsontext: invalid surrogate pair"). Lone surrogates still reach the binary format through escapes in
// string literals (unicode_escapes.ts), whose cooked text is WTF-8.
const inline = {};

const fixtures = fs.readdirSync(fixturesDir).filter(f => !f.startsWith(".")).sort()
    .map(name => [name, fs.readFileSync(path.join(fixturesDir, name), "utf8")]);
fixtures.push(...Object.entries(inline));

const api = new API({ cwd: "/", tsserverPath: tsc });
const manifest = {
    server: { source: "microsoft/TypeScript", commit: refHead, build: "cd ts-ref/tsc && GOWORK=off go build ./cmd/tsc", go: process.env.TSRS_CODEC_GO_VERSION ?? "unknown", version: tscVersion, sha256: tscSha256 },
    client: { package: `${pkg.name}@${pkg.version}`, gitHead: pkg.gitHead },
    files: {},
};
for (const [name, text] of fixtures) {
    using retained = api.createSourceFile(`/fixtures/${name}`, text);
    const sf = retained.sourceFile;
    const raw = api.client.apiRequestBinary("createSourceFile", { fileName: `/fixtures/${name}`, sourceText: text, options: {} });
    const lease = new DataView(raw.buffer, raw.byteOffset).getUint32(52, true);
    api.client.apiRequest("releaseSourceFile", { lease });
    const data = Uint8Array.from(raw);
    data.fill(0, 44, 60);
    fs.writeFileSync(path.join(goldenDir, `${name}.bin`), data);
    const client = Uint8Array.from(encodeSourceFile(sf));
    fs.writeFileSync(path.join(goldenDir, `${name}.client.bin`), client);
    manifest.files[name] = { bytes: data.length, clientBytes: client.length, inline: name in inline };
    console.log(`${name}: ${data.length} bytes (client encoder ${client.length})`);
}
// printNode parity: the server's DecodeNodes + printer on (a) each Go SourceFile encoding (printed with the
// decoded file as Go's handlePrintNode does) and (b) synthesized trees built with the client factory (every
// position -1 / 0xFFFFFFFF on the wire) and encoded by the client's encodeNode, as api.printer.printNode does.
const printDir = path.join(goldenDir, "print");
fs.rmSync(printDir, { recursive: true, force: true });
fs.mkdirSync(printDir, { recursive: true });
// A request the server fails (e.g. a recovered decoder panic) is recorded as `<case>.go.err` (first line).
const printTo = (base, bytes) => {
    try {
        fs.writeFileSync(path.join(printDir, `${base}.go.txt`), api.client.apiRequest("printNode", { data: Buffer.from(bytes).toString("base64") }));
        return "printed";
    }
    catch (e) {
        fs.writeFileSync(path.join(printDir, `${base}.go.err`), e.message.split("\n")[0] + "\n");
        return "error";
    }
};
manifest.print = {};
for (const [name] of fixtures) {
    manifest.print[`${name}`] = printTo(name, fs.readFileSync(path.join(goldenDir, `${name}.bin`)));
    manifest.print[`${name}.client`] = printTo(`${name}.client`, fs.readFileSync(path.join(goldenDir, `${name}.client.bin`)));
}
const f = await imp("dist/ast/factory.generated.js");
const { SyntaxKind: K } = await imp("dist/enums/syntaxKind.enum.js");
const { TokenFlags } = await imp("dist/enums/tokenFlags.enum.js");
const id = t => f.createIdentifier(t);
const kw = k => f.createKeywordTypeNode(k);
const synthetic = {
    "synthetic_union": () => f.createUnionTypeNode([
        f.createTypeReferenceNode(id("Map"), [kw(K.StringKeyword), f.createLiteralTypeNode(f.createStringLiteral("a\uD800b", TokenFlags.None))]),
        f.createFunctionTypeNode(undefined, [f.createParameterDeclaration(undefined, undefined, id("x"), f.createToken(K.QuestionToken), kw(K.NumberKeyword))], f.createTypeOperatorNode(K.ReadonlyKeyword, f.createArrayTypeNode(kw(K.BooleanKeyword)))),
        f.createLiteralTypeNode(f.createNumericLiteral("42", TokenFlags.None)),
    ]),
    "synthetic_function": () => f.createFunctionDeclaration(
        [f.createToken(K.ExportKeyword), f.createToken(K.AsyncKeyword)], undefined, id("run"), [f.createTypeParameterDeclaration(undefined, id("T"), kw(K.ObjectKeyword))],
        [f.createParameterDeclaration(undefined, undefined, id("a"), f.createToken(K.QuestionToken), kw(K.StringKeyword)),
         f.createParameterDeclaration(undefined, f.createToken(K.DotDotDotToken), id("rest"), undefined, f.createArrayTypeNode(kw(K.NumberKeyword)))],
        f.createTypeReferenceNode(id("Promise"), [kw(K.VoidKeyword)]),
        f.createBlock([
            f.createVariableStatement(undefined, f.createVariableDeclarationList([f.createVariableDeclaration(id("s"), undefined, undefined, f.createNoSubstitutionTemplateLiteral("t\u00e9`x", TokenFlags.None))], 2)),
            f.createIfStatement(f.createPrefixUnaryExpression(K.ExclamationToken, id("a")), f.createReturnStatement(undefined), undefined),
            f.createExpressionStatement(f.createAwaitExpression(f.createCallExpression(f.createPropertyAccessExpression(id("console"), undefined, id("log"), 0), undefined, undefined, [id("a"), f.createSpreadElement(id("rest"))], 0))),
        ], true),
    ),
};
// SourceFile roots mixing synthesized (0xFFFFFFFF) and parsed positions: the server prints them with the
// decoded file as the current source file, so synthesized-position handling against real text is exercised.
const syntheticStatements = () => [
    synthetic.synthetic_function(),
    f.createVariableStatement(undefined, f.createVariableDeclarationList([f.createVariableDeclaration(id("caf\u00e9"), undefined, undefined, f.createStringLiteral("\u{1F600}", TokenFlags.None))], 1)),
];
synthetic.synthetic_source_file_empty_text = () => f.createSourceFile(syntheticStatements(), f.createToken(K.EndOfFile), "", "/synthetic.ts", "/synthetic.ts");
synthetic.synthetic_source_file_with_text = () => f.createSourceFile(syntheticStatements(), f.createToken(K.EndOfFile), "const original = \"\u00e9t\u00e9\";\n", "/synthetic.ts", "/synthetic.ts");
for (const fixture of ["unicode_text.ts", "complex.ts"]) {
    synthetic[`mixed_${fixture.replace(/\W/g, "_")}`] = () => {
        const retained = api.createSourceFile(`/fixtures/${fixture}`, fs.readFileSync(path.join(fixturesDir, fixture), "utf8"));
        const sf = retained.sourceFile;
        return f.updateSourceFile(sf, [...sf.statements, ...syntheticStatements()], sf.endOfFileToken);
    };
}
for (const [name, build] of Object.entries(synthetic)) {
    const bytes = Uint8Array.from(encodeNode(build()));
    fs.writeFileSync(path.join(printDir, `${name}.client.bin`), bytes);
    manifest.print[name] = printTo(name, bytes);
}
api.close();
fs.writeFileSync(path.join(goldenDir, "manifest.json"), JSON.stringify(manifest, null, 2) + "\n");
