// Checker-lane cross-request identity / retained-snapshot probe for the pinned upstream sync client.
//
// Not part of the parity suite: it is run inside a tools/node-api harness work tree (which links the
// pinned client and a server binary as built/local/tsc), once against the Go oracle and once against
// tsrs, and the two observation files (CHECKER_OBS_OUT) are compared. See ../../README.md.

import { test } from "node:test";
import fs from "node:fs";
import { API } from "@typescript/typescript/sync";
import { cwd, recordingFS, syncAPI } from "../parity/utils.ts";

const A1 = `export const value: number = 1;\nexport interface Box<T> { v: T }\n`;
const A2 = `export const value: string = "x";\nexport interface Box<T> { v: T }\n`;
const B = `import { value, type Box } from "./a";\nexport const use = value;\nexport const box: Box<string> = { v: "" };\nexport class K { m = 1; n = "" }\n`;

function attempt(f: () => unknown): unknown {
    try {
        return { ok: f() };
    }
    catch (e) {
        return { error: String((e as Error).message).split("\n")[0].replace(/\d+/g, "<n>") };
    }
}

test("checker identity across requests, snapshot updates and disposal", () => {
    const { api, vfs } = syncAPI({
        "/tsconfig.json": `{ "compilerOptions": { "strict": true }, "include": ["src"] }`,
        "/src/a.ts": A1,
        "/src/b.ts": B,
    });
    using _ = api;
    const o: Record<string, unknown> = {};
    const at = (text: string, needle: string) => text.indexOf(needle);

    const snap1 = api.createSnapshot({ openProjects: ["/tsconfig.json"] });
    const c1 = snap1.getConfiguredProject("/tsconfig.json")!.checker;
    const use1 = c1.getSymbolAtPosition("/src/b.ts", at(B, "use ="))!;
    o.withinSnapshot_sameSymbolObject = use1 === c1.getSymbolAtPosition("/src/b.ts", at(B, "use ="));
    const box1 = c1.getTypeAtPosition("/src/b.ts", at(B, "box:"))!;
    const boxSym1 = c1.getSymbolAtPosition("/src/b.ts", at(B, "box:"))!;
    const boxViaSymbol = c1.getTypeOfSymbol(boxSym1)!;
    o.withinSnapshot_sameTypeObject = box1 === boxViaSymbol;
    o.withinSnapshot_sameTypeId = box1.id === boxViaSymbol.id;
    o.withinSnapshot_typeString = c1.typeToString(box1);
    const k1 = c1.getSymbolAtPosition("/src/b.ts", at(B, "K {"))!;
    o.membersOfK = [...k1.getMembers().keys()];
    o.membersOfK_sameMapTwice = k1.getMembers() === k1.getMembers();
    const val1 = c1.getSymbolAtPosition("/src/a.ts", at(A1, "value"))!;

    vfs.writeFile("/src/a.ts", A2);
    const snap2 = snap1.update({ fileNotifications: { changed: ["/src/a.ts"] }, ensurePrograms: true });
    const c2 = snap2.getConfiguredProject("/tsconfig.json")!.checker;
    const use2 = c2.getSymbolAtPosition("/src/b.ts", at(B, "use ="))!;
    o.crossSnapshot_unchangedFile_sameSymbolObject = use1 === use2;
    o.crossSnapshot_unchangedFile_sameSymbolId = use1.id === use2.id;
    const val2 = c2.getSymbolAtPosition("/src/a.ts", at(A2, "value"))!;
    o.crossSnapshot_changedFile_sameSymbolObject = val1 === val2;
    o.old_typeOfUse = c1.typeToString(c1.getTypeOfSymbol(use1)!);
    o.new_typeOfUse = c2.typeToString(c2.getTypeOfSymbol(use2)!);
    o.oldUnchangedFileSymbol_inNewChecker = attempt(() => c2.typeToString(c2.getTypeOfSymbol(use1)!));
    o.oldChangedFileSymbol_inNewChecker = attempt(() => c2.typeToString(c2.getTypeOfSymbol(val1)!));
    o.newChangedFileSymbol_inOldChecker = attempt(() => c1.typeToString(c1.getTypeOfSymbol(val2)!));
    o.oldType_inNewChecker_typeToString = attempt(() => c2.typeToString(box1));
    o.oldType_inNewChecker_assignableToString = attempt(() => c2.isTypeAssignableTo(box1, c2.getStringType()));
    // Type handles are per-checker counters and the client sends only the id: once the new checker has
    // registered a type under the same number, an old handle resolves to *that* type (no ownership check
    // on either server). Recorded, not asserted.
    const box2 = c2.getTypeAtPosition("/src/b.ts", at(B, "box:"))!;
    o.crossSnapshot_typeIdsCoincide = box1.id === box2.id;
    o.oldType_inNewChecker_afterNewRegistered = attempt(() => c2.typeToString(box1));
    o.oldType_inNewChecker_otherOldHandle = attempt(() => c2.typeToString(c1.getStringType()));

    snap2.dispose();
    o.afterNewDisposed_oldTypeOfUse = attempt(() => c1.typeToString(c1.getTypeOfSymbol(use1)!));
    o.afterNewDisposed_oldType = attempt(() => c1.typeToString(box1));
    o.afterNewDisposed_freshMembers = attempt(() => [...c1.getSymbolAtPosition("/src/b.ts", at(B, "K {"))!.getMembers().keys()]);
    o.afterNewDisposed_newChecker = attempt(() => c2.typeToString(c2.getStringType()));

    snap1.dispose();
    o.afterAllDisposed_oldChecker = attempt(() => c1.typeToString(box1));
    o.afterAllDisposed_cachedParent = attempt(() => use1.getParent()?.name);
    o.afterAllDisposed_uncachedMembers = attempt(() => [...use1.getMembers().keys()]);

    const snap3 = api.createSnapshot({ openProjects: ["/tsconfig.json"] });
    const c3 = snap3.getConfiguredProject("/tsconfig.json")!.checker;
    const use3 = c3.getSymbolAtPosition("/src/b.ts", at(B, "use ="))!;
    o.recreated_sameSymbolObjectAsDisposed = use3 === use1;
    o.recreated_typeOfUse = c3.typeToString(c3.getTypeOfSymbol(use3)!);
    o.recreated_oldSymbol = attempt(() => c3.typeToString(c3.getTypeOfSymbol(use1)!));
    snap3.dispose();

    fs.writeFileSync(process.env.CHECKER_OBS_OUT!, JSON.stringify(o, null, 2) + "\n");
});

test("checker requests on a retained snapshot from inside a filesystem callback of an update", () => {
    const { vfs } = recordingFS({
        "/tsconfig.json": `{ "compilerOptions": { "strict": true }, "include": ["src"] }`,
        "/src/a.ts": A1,
        "/src/b.ts": B,
    });
    // The client captures the filesystem callbacks at construction: hook first, arm later.
    const readFile = vfs.readFile.bind(vfs);
    let armed = false;
    let nested: unknown = "not-called";
    let c1: any, use1: any, box1: any;
    vfs.readFile = (p: string) => {
        if (armed && p === "/src/a.ts" && nested === "not-called") {
            nested = {
                typeOfUse: attempt(() => c1.typeToString(c1.getTypeOfSymbol(use1)!)),
                typeAtPosition: attempt(() => c1.typeToString(c1.getTypeAtPosition("/src/b.ts", B.indexOf("use ="))!)),
                oldType: attempt(() => c1.typeToString(box1)),
                parent: attempt(() => use1.getParent()?.name),
            };
        }
        return readFile(p);
    };
    const api = new API({ cwd, fs: vfs });
    using _ = api;
    const o: Record<string, unknown> = {};
    const snap1 = api.createSnapshot({ openProjects: ["/tsconfig.json"] });
    c1 = snap1.getConfiguredProject("/tsconfig.json")!.checker;
    use1 = c1.getSymbolAtPosition("/src/b.ts", B.indexOf("use ="))!;
    box1 = c1.getTypeAtPosition("/src/b.ts", B.indexOf("box:"))!;
    vfs.writeFile("/src/a.ts", A2);
    armed = true;
    const snap2 = snap1.update({ fileNotifications: { changed: ["/src/a.ts"] }, ensurePrograms: true });
    o.nestedDuringUpdate = nested;
    const c2 = snap2.getConfiguredProject("/tsconfig.json")!.checker;
    o.afterUpdate_new = c2.typeToString(c2.getTypeOfSymbol(c2.getSymbolAtPosition("/src/b.ts", B.indexOf("use ="))!)!);
    o.afterUpdate_old = c1.typeToString(c1.getTypeOfSymbol(use1)!);
    snap2.dispose();
    snap1.dispose();
    fs.writeFileSync(process.env.CHECKER_REENTRY_OUT!, JSON.stringify(o, null, 2) + "\n");
});
