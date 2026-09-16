// Turn a wasm build result into an iframe srcdoc: every compiled module
// becomes a Blob url, an import map binds the stable module ids (and the bare
// react imports, served from esm.sh) to those urls, and the transformed
// index.html loads its entries through inline `import` statements, which do
// consult the import map. Cycles are free because ids never change.

export type BuildError = { path: string; message: string };

export type BuildResult = {
  ok: boolean;
  html: string;
  modules: { id: string; code: string }[];
  bare: string[];
  errors: BuildError[];
};

const REACT_VERSION = "19.2.0";
const REACT_DEPS = `deps=react@${REACT_VERSION},react-dom@${REACT_VERSION}`;

export function cdnUrl(spec: string): string {
  if (spec === "react") return `https://esm.sh/react@${REACT_VERSION}`;
  if (spec.startsWith("react/")) {
    return `https://esm.sh/react@${REACT_VERSION}/${spec.slice("react/".length)}`;
  }
  if (spec === "react-dom") {
    return `https://esm.sh/react-dom@${REACT_VERSION}?${REACT_DEPS}`;
  }
  if (spec.startsWith("react-dom/")) {
    return `https://esm.sh/react-dom@${REACT_VERSION}/${spec.slice("react-dom/".length)}?${REACT_DEPS}`;
  }
  // Every other package gets the same pin: a React-dependent library must
  // resolve its internal react import to the copy the playground loads, or
  // hooks crash with two React instances.
  return `https://esm.sh/${spec}?${REACT_DEPS}`;
}

// A build's blob urls stay alive until the swap that displaces their document
// actually commits: batches superseded before ever being shown, and the
// previously displayed batch, collect in `retired` and are revoked together by
// revokeRetired() from the swap's load handler. A flat timer would race a slow
// back-frame load and yank modules from the still-visible document.
let liveUrls: string[] = [];
let retired: string[] = [];

export function revokeRetired(): void {
  for (const url of retired) URL.revokeObjectURL(url);
  retired = [];
}

/** Everything, retired and live: for component teardown. */
export function revokeAll(): void {
  revokeRetired();
  for (const url of liveUrls) URL.revokeObjectURL(url);
  liveUrls = [];
}

export function buildSrcdoc(result: BuildResult): string {
  retired.push(...liveUrls);
  liveUrls = [];

  const imports: Record<string, string> = {};
  for (const m of result.modules) {
    const url = URL.createObjectURL(new Blob([m.code], { type: "text/javascript" }));
    imports[m.id] = url;
    liveUrls.push(url);
  }
  // Module ids win: a literal `import "@app/..."` in user code must never
  // overwrite a real module's blob url with a garbage cdn url.
  for (const bare of result.bare) {
    if (!(bare in imports)) imports[bare] = cdnUrl(bare);
  }

  const mapTag = `<script type="importmap">${JSON.stringify({ imports })}</script>`;
  const html = result.html;
  // `[\s>]` so <header> can't match; without a <head>, the map must still land
  // before the first script tag (a late import map is rejected by the browser).
  const head = /<head[\s>]/i.exec(html);
  if (head) {
    const at = html.indexOf(">", head.index) + 1;
    return html.slice(0, at) + mapTag + html.slice(at);
  }
  const firstScript = /<script\b/i.exec(html);
  if (firstScript) {
    return html.slice(0, firstScript.index) + mapTag + html.slice(firstScript.index);
  }
  return mapTag + html;
}
