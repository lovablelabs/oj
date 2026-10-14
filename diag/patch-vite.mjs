// diag only: timestamps around the steps of Vite's loadConfigFromFile.
import fs from "node:fs";
const f = process.argv[2];
let s = fs.readFileSync(f, "utf8");
const P = (label) => `process.stderr.write("[oj-phase] " + Date.now() + " vite: ${label}\\n");`;
const rep = (a, b) => {
  if (!s.includes(a)) throw new Error("anchor missing: " + a);
  s = s.replace(a, b);
};
rep("\tconst bundled = await bundleConfigFile(resolvedPath, isESM);", `\t${P("bundle start")}\n\tconst bundled = await bundleConfigFile(resolvedPath, isESM);\n\t${P("bundle done")}`);
rep("\t\tawait fsp.writeFile(tempFileName, bundledCode);", `\t\tawait fsp.writeFile(tempFileName, bundledCode);\n\t\t${P("temp written")}`);
rep("\t\t\treturn (await import(pathToFileURL(tempFileName).href)).default;", `\t\t\tconst __m = await import(pathToFileURL(tempFileName).href);\n\t\t\t${P("temp imported")}\n\t\t\treturn __m.default;`);
rep("\tconst bundle = await rolldown({\n\t\tinput: fileName,", `\t${P("rolldown() start")}\n\tconst bundle = await rolldown({\n\t\tinput: fileName,`);
fs.writeFileSync(f, s);
console.log("patched", f);
