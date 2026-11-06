// Checker-lane numeric handle probe over raw wire payloads (no client library): spawns an `--api` server
// (pinned Go oracle or tsrs), speaks the sync MessagePack tuple protocol or async JSON-RPC directly, and
// sends checker requests whose integer fields carry exact literals (2^53±1, uint32/uint64 bounds ±1,
// negative, exponent/fraction syntax, escaped member names, nested symbol references, array elements,
// batchRequests). Prints one JSON line per case: the response result or the first line of the error.
//
//   node numeric_ids_raw.mjs <server-exe> sync|async > out.jsonl
//
// Compare the outputs of the oracle and tsrs line by line.
import { spawn } from "node:child_process";
import { mkdtempSync, writeFileSync, mkdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const [exe, mode] = process.argv.slice(2);
if (!exe || !["sync", "async"].includes(mode)) throw new Error("usage: numeric_ids_raw.mjs <exe> sync|async");

const root = mkdtempSync(join(tmpdir(), "tsrs-numeric-"));
mkdirSync(join(root, "p"));
const B = `export const box: Array<number> = [];\nexport function f(x: string): number { return 1; }\nexport const y = f("a");\n`;
writeFileSync(join(root, "p", "tsconfig.json"), JSON.stringify({ compilerOptions: { strict: true, noEmit: true, types: [] }, files: ["b.ts"] }));
writeFileSync(join(root, "p", "b.ts"), B);

const args = ["--api", ...(mode === "async" ? ["--async"] : []), "--cwd", root, "--useCaseSensitiveFileNames=true"];
const child = spawn(exe, args, { stdio: ["pipe", "pipe", "inherit"] });
let buf = Buffer.alloc(0);
const waiters = [];
child.stdout.on("data", d => {
    buf = Buffer.concat([buf, d]);
    pump();
});

// ── framing ──
function binHeader(n) {
    if (n < 256) return Buffer.from([0xc4, n]);
    if (n < 65536) return Buffer.from([0xc5, n >> 8, n & 255]);
    const b = Buffer.alloc(5);
    b[0] = 0xc6;
    b.writeUInt32BE(n, 1);
    return b;
}
function readBin(b, i) {
    const t = b[i];
    if (t === 0xc4) return i + 2 <= b.length && i + 2 + b[i + 1] <= b.length ? [b.subarray(i + 2, i + 2 + b[i + 1]), i + 2 + b[i + 1]] : null;
    if (t === 0xc5) {
        if (i + 3 > b.length) return null;
        const n = b.readUInt16BE(i + 1);
        return i + 3 + n <= b.length ? [b.subarray(i + 3, i + 3 + n), i + 3 + n] : null;
    }
    if (t === 0xc6) {
        if (i + 5 > b.length) return null;
        const n = b.readUInt32BE(i + 1);
        return i + 5 + n <= b.length ? [b.subarray(i + 5, i + 5 + n), i + 5 + n] : null;
    }
    throw new Error("unexpected msgpack byte " + t);
}
function pump() {
    for (;;) {
        let msg = null;
        if (mode === "sync") {
            if (buf.length < 2) return;
            if (buf[0] !== 0x93) throw new Error("bad tuple");
            const type = buf[1];
            const m = readBin(buf, 2);
            if (!m) return;
            const p = readBin(buf, m[1]);
            if (!p) return;
            msg = { type, payload: p[0].toString("utf8") };
            buf = buf.subarray(p[1]);
        }
        else {
            const h = buf.indexOf("\r\n\r\n");
            if (h < 0) return;
            const len = Number(/Content-Length: (\d+)/i.exec(buf.subarray(0, h).toString())[1]);
            if (buf.length < h + 4 + len) return;
            msg = JSON.parse(buf.subarray(h + 4, h + 4 + len).toString("utf8"));
            buf = buf.subarray(h + 4 + len);
        }
        waiters.shift()(msg);
    }
}
let nextId = 0;
/** Sends `method` with the raw JSON text `params`; resolves to { result } (parsed) or { error } (first line). */
function send(method, params) {
    return new Promise(resolve => {
        waiters.push(msg => {
            if (mode === "sync") {
                if (msg.type === 4) resolve({ result: msg.payload === "" ? null : JSON.parse(msg.payload) });
                else resolve({ error: msg.payload.split("\n")[0] });
            }
            else if (msg.error) resolve({ error: String(msg.error.message).split("\n")[0] });
            else resolve({ result: msg.result });
        });
        if (mode === "sync") {
            const m = Buffer.from(method), p = Buffer.from(params ?? "");
            child.stdin.write(Buffer.concat([Buffer.from([0x93, 1]), binHeader(m.length), m, binHeader(p.length), p]));
        }
        else {
            const body = Buffer.from(`{"jsonrpc":"2.0","id":${++nextId},"method":${JSON.stringify(method)}${params === undefined ? "" : `,"params":${params}`}}`);
            child.stdin.write(Buffer.concat([Buffer.from(`Content-Length: ${body.length}\r\n\r\n`), body]));
        }
    });
}

const out = [];
const record = (label, r) => out.push(JSON.stringify({ q: label, r }));

await send("initialize", "null");
const snap = (await send("createSnapshot", JSON.stringify({ openProjects: [join(root, "p", "tsconfig.json")] }))).result;
const S = snap.snapshot, P = JSON.stringify(snap.projects[0].id);
const file = JSON.stringify(join(root, "p", "b.ts"));
const sp = `"snapshot":${S},"project":${P}`;
const box = (await send("getTypeAtPosition", `{${sp},"file":${file},"position":${B.indexOf("box")}}`)).result;
const fSym = (await send("getSymbolAtPosition", `{${sp},"file":${file},"position":${B.indexOf("f(x")}}`)).result;
const fType = (await send("getTypeOfSymbol", `{${sp},"symbol":${JSON.stringify(fSym.reference)}}`)).result;
const sig = (await send("getSignaturesOfType", `{${sp},"type":${fType.id},"kind":0}`)).result[0];
const arr = (await send("resolveName", `{${sp},"name":"Array","meaning":788968}`)).result;
// Controls: small, valid handles still work.
record("control typeToString(box)", await send("typeToString", `{${sp},"type":${box.id}}`));
record("control returnType(sig)", await send("getReturnTypeOfSignature", `{${sp},"objectId":${sig.id}}`).then(r => r.result ? { result: r.result.flags } : r));
record("control symbol kind", { result: [fSym.reference.kind, arr.reference.kind] });

const U64 = ["9007199254740991", "9007199254740992", "9007199254740993", "18446744073709551615", "18446744073709551616", "-1", "1e3", "1.0", "0"];
const U32 = ["4294967295", "4294967296", "9007199254740993", "-1", "1e1"];
const I32 = ["2147483647", "2147483648", "-2147483648", "-2147483649"];

for (const n of U64) {
    record(`snapshot=${n} getTypeAtPosition`, await send("getTypeAtPosition", `{"snapshot":${n},"project":${P},"file":${file},"position":0}`));
    record(`escaped snap\\u0073hot=${n}`, await send("getTypeAtPosition", `{"snap\\u0073hot":${n},"project":${P},"file":${file},"position":0}`));
    record(`signature objectId=${n} getReturnTypeOfSignature`, await send("getReturnTypeOfSignature", `{${sp},"objectId":${n}}`));
    record(`signature=${n} getRestTypeOfSignature`, await send("getRestTypeOfSignature", `{${sp},"signature":${n}}`));
    record(`symbol.id=${n} snapshot-owned getTypeOfSymbol`, await send("getTypeOfSymbol", `{${sp},"symbol":{"kind":1,"snapshot":${S},"project":${P},"id":${n}}}`));
    record(`symbol.snapshot=${n} getTypeOfSymbol`, await send("getTypeOfSymbol", `{${sp},"symbol":{"kind":1,"snapshot":${n},"project":${P},"id":${arr.reference.id}}}`));
    record(`symbol.snapshot=${n} getParentOfSymbol`, await send("getParentOfSymbol", `{"symbol":{"kind":1,"snapshot":${n},"project":${P},"id":${arr.reference.id}}}`));
    record(`symbols[0].id=${n} getTypesOfSymbols`, await send("getTypesOfSymbols", `{${sp},"symbols":[{"kind":1,"snapshot":${S},"project":${P},"id":${n}}]}`));
    record(`batch snapshot=${n}`, await send("batchRequests", `{"requests":[{"method":"getTypeAtPosition","params":{"snapshot":${n},"project":${P},"file":${file},"position":0}},{"method":"typeToString","params":{${sp},"type":${box.id}}}]}`));
}
for (const n of U32) {
    record(`type=${n} typeToString`, await send("typeToString", `{${sp},"type":${n}}`));
    record(`objectId(type)=${n} getTargetOfType`, await send("getTargetOfType", `{${sp},"objectId":${n}}`));
    record(`source=${n} isTypeAssignableTo`, await send("isTypeAssignableTo", `{${sp},"source":${n},"target":${box.id}}`));
    record(`position=${n} getTypeAtPosition`, await send("getTypeAtPosition", `{${sp},"file":${file},"position":${n}}`));
    record(`positions[1]=${n} getSymbolsAtPositions`, await send("getSymbolsAtPositions", `{${sp},"file":${file},"positions":[0,${n}]}`).then(r => r.result ? { result: r.result.map(s => s && s.name) } : r));
    record(`meaning=${n} resolveName`, await send("resolveName", `{${sp},"name":"Array","meaning":${n}}`).then(r => r.result !== undefined ? { result: r.result && r.result.name } : r));
    record(`symbol.kind=${n} getTypeOfSymbol`, await send("getTypeOfSymbol", `{${sp},"symbol":{"kind":${n},"snapshot":${S},"project":${P},"id":${arr.reference.id}}}`));
}
for (const n of I32) {
    record(`index=${n} getParameterType`, await send("getParameterType", `{${sp},"signature":${sig.id},"index":${n}}`));
    record(`kind=${n} getSignaturesOfType`, await send("getSignaturesOfType", `{${sp},"type":${fType.id},"kind":${n}}`).then(r => r.result ? { result: r.result.length } : r));
    record(`flags=${n} typeToString`, await send("typeToString", `{${sp},"type":${box.id},"flags":${n}}`));
}
child.stdin.end();
process.stdout.write(out.join("\n") + "\n");
