import { createRoute, useLoaderData } from "@tanstack/react-router";
import { useEffect, useState } from "react";

import { rootRoute } from "./__root";
import { getGreeting } from "../server/data";

// tsconfig paths alias + package.json "imports" map -- both resolve to the
// same module, and both must work on the SSR side.
import { shout } from "#lib/format";
import { shout as shoutViaPaths } from "@app/lib/format";

// A CommonJS dep with `module.exports = {...}` (the loader's CJS facade).
import { badge } from "legacy-cjs";

// A CJS subpath imported extensionless (no "exports" map): Node's strict ESM
// resolver won't probe `.js`, so the SSR loader must recover it like a bundler.
import deep from "legacy-cjs/deep";

// A plugin-owned virtual module (resolved via oj's plugin-container bridge).
import { buildTag } from "virtual:build-info";

// A REAL on-disk .js file whose content a plugin's load() overrides (compiled
// in buildStart). Exercises: buildStart ran, load overrode the fs read for a
// real path, this.environment.name was visible, and the arbitrary-string
// export `m.freshMsg_cta()` survives oj's transform/bundle (the reported crash).
import * as gen from "../generated/stale.js";

// import.meta.glob in a .jsx file and in an unclaimed plain .js file: both must
// be expanded by the start SSR loader (the .tsx path already is).
import { GlobWidget } from "../widgets/glob-widget.jsx";
import { plainGlobTitles } from "../generated/glob-plain.js";
import { genericGlobTitles } from "../generated/glob-generic";
import { envProbe } from "../lib/env-probe.js";
// JSON named exports (Vite's json.namedExports default): server and client.
import { title as alphaTitle } from "../content/alpha.json";

declare const __FIXTURE_DEFINE__: string;
// Per-environment define (environments.{client,ssr}.define): a different value
// on each side, so the render tells which bundle applied which.
declare const __FIXTURE_SIDE__: string;

// svgr: a bare .svg import yields a React component (exportType "default")...
import Logo from "../logo.svg";
// ...and the explicit `?react` query yields one regardless of exportType.
import Star from "../star.svg?react";

// MDX compiled to a component by @mdx-js/rollup (a plugin `transform` hook).
import Welcome from "../content/welcome.mdx";

// Asset conventions: ?raw inlines the text, ?url yields a served URL.
import notes from "../content/notes.txt?raw";
import heroUrl from "../hero.png?url";
// A plugin's pre load owns an image's query form (vite-imagetools).
import heroVariant from "../hero.png?w=64";
// A pre plugin's resolveId owns a bare import's query form.
import sheetUrl from "fixture-sheet/styles.css?url";
// resolveId sees each import as written: a file answer with a query, a claim
// over an installed package, over an existing file, and of a /-prefixed id.
import answerRaw from "fixture-missing/notes.txt?raw";
import installedClaim from "react/index.js?raw";
import relativeClaim from "../content/claimed.txt?raw";
import slashClaim from "/virtual-probe.txt?raw";
// A claim of the bare `react/package.json` alone never takes its `?raw`.
import reactPackageRaw from "react/package.json?raw";
// A plugin's remote URL answer: stubbed by the dev render (an empty module),
// a virtual module in the client and the build; the page reads the same.
import remoteUrl from "fixture-remote/x.css?url";

// import.meta.glob (eager): a static map of the JSON content files.
const modules = import.meta.glob("../content/*.json", { eager: true }) as Record<
  string,
  { default: { title: string; slug: string } }
>;
const titles = Object.values(modules)
  .map((m) => m.default.title)
  .sort()
  .join(", ");

export const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/",
  loader: async () => await getGreeting(),
  component: Index,
});

function Index() {
  const data = useLoaderData({ from: indexRoute.id });
  // A client-only hydration probe for the browser gate: the span is rendered
  // only after mount (absent from the SSR HTML, so its appearance proves the
  // client runtime came alive), and the counter's onClick proves event handlers
  // attached post-hydration.
  const [mounted, setMounted] = useState(false);
  const [count, setCount] = useState(0);
  useEffect(() => setMounted(true), []);
  return (
    <main>
      <h1 className="fixture-heading">{shout("home")}</h1>
      {mounted ? <span data-testid="client-mounted">client-mounted-ok</span> : null}
      <button data-testid="counter" onClick={() => setCount((c) => c + 1)}>count: {count}</button>
      <p data-testid="server-fn">{data.message} / edition={data.edition}</p>
      <p data-testid="paths-alias">{shoutViaPaths("alias")}</p>
      <p data-testid="cjs">{badge("interop")}</p>
      <p data-testid="cjs-subpath">{deep("ok")}</p>
      <p data-testid="virtual">{buildTag}</p>
      {/* Deliberately environment-divergent (this.environment.name + consumer):
          "ssr-server" server-side, "client-client" in the client bundle. Like
          env-define below, this is an intentional per-environment difference, so
          suppress the (correct) hydration-mismatch warning it would otherwise
          raise. */}
      <p data-testid="fresh-module" suppressHydrationWarning>{gen.LABEL}</p>
      <p data-testid="fresh-fn" suppressHydrationWarning>{gen.freshMsg_cta()}</p>
      <p data-testid="glob">{titles}</p>
      <p data-testid="glob-jsx"><GlobWidget /></p>
      <p data-testid="glob-js">{plainGlobTitles}</p>
      <p data-testid="glob-ts-generic">{genericGlobTitles}</p>
      {/* envProbe embeds import.meta.env.SSR (true server-side, false in the
          client bundle), so it is intentionally environment-divergent too. */}
      <p data-testid="js-env" suppressHydrationWarning>{envProbe}</p>
      <p data-testid="json-named">{`json-named:${alphaTitle}`}</p>
      <p data-testid="define">{__FIXTURE_DEFINE__}</p>
      <p data-testid="env-define" suppressHydrationWarning>{__FIXTURE_SIDE__}</p>
      <p data-testid="raw">{notes.trim()}</p>
      <img data-testid="url" src={heroUrl} alt="hero" />
      <p data-testid="image-variant">{heroVariant}</p>
      <p data-testid="sheet-url">{sheetUrl}</p>
      <p data-testid="query-claims">
        {[answerRaw.trim(), installedClaim, relativeClaim, slashClaim].join(" ")}
      </p>
      <p data-testid="remote-url">
        {typeof remoteUrl === "object" || remoteUrl === "remote-client" ? "remote-answer-ok" : String(remoteUrl)}
      </p>
      <p data-testid="bare-claim">
        {reactPackageRaw.includes('"name": "react"') ? "bare-claim-kept-raw-ok" : reactPackageRaw}
      </p>
      <span data-testid="svg"><Logo /></span>
      <span data-testid="svg-react"><Star /></span>
      <section data-testid="mdx"><Welcome /></section>
    </main>
  );
}
