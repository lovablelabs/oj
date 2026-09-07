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

export function cdnUrl(spec: string): string {
  if (spec === "react") return `https://esm.sh/react@${REACT_VERSION}`;
  if (spec.startsWith("react/")) {
    return `https://esm.sh/react@${REACT_VERSION}/${spec.slice("react/".length)}`;
  }
  if (spec === "react-dom") {
    return `https://esm.sh/react-dom@${REACT_VERSION}?deps=react@${REACT_VERSION}`;
  }
  if (spec.startsWith("react-dom/")) {
    return `https://esm.sh/react-dom@${REACT_VERSION}/${spec.slice("react-dom/".length)}?deps=react@${REACT_VERSION}`;
  }
  return `https://esm.sh/${spec}`;
}

let liveUrls: string[] = [];

export function buildSrcdoc(result: BuildResult): string {
  const stale = liveUrls;
  liveUrls = [];

  const imports: Record<string, string> = {};
  for (const m of result.modules) {
    const url = URL.createObjectURL(new Blob([m.code], { type: "text/javascript" }));
    imports[m.id] = url;
    liveUrls.push(url);
  }
  for (const bare of result.bare) imports[bare] = cdnUrl(bare);

  // The previous build's blobs are only unreachable once the new srcdoc is
  // committed; revoking on the next tick avoids yanking a module out from
  // under the outgoing document.
  setTimeout(() => {
    for (const url of stale) URL.revokeObjectURL(url);
  }, 1000);

  const mapTag = `<script type="importmap">${JSON.stringify({ imports })}</script>`;
  const html = result.html;
  const head = /<head[^>]*>/i.exec(html);
  if (head) {
    const at = head.index + head[0].length;
    return html.slice(0, at) + mapTag + html.slice(at);
  }
  return mapTag + html;
}
