import { createRoute } from "@tanstack/react-router";

import { rootRoute } from "./__root";
import { BenchRace } from "../components/BenchRace";

const GITHUB = "https://github.com/lovablelabs/oj";

const STEPS = [
  { title: "Install", body: "One binary from crates.io, with Node on your PATH for the app's own toolchain.", cmd: "cargo install oj" },
  { title: "Develop", body: "A fast on-demand dev server with Fast Refresh and warm server rendering.", cmd: "oj dev ." },
  { title: "Build", body: "A minified client, a bundled SSR server, and an edge worker in one pass.", cmd: "oj build ." },
  { title: "Deploy", body: "The build emits a Cloudflare Worker and its assets. Ship it with wrangler.", cmd: "wrangler deploy" },
];

const FEATURES = [
  { title: "Rust-native compile", body: "TypeScript and JSX are transformed by oxc: types erased, the automatic JSX runtime used, Fast Refresh instrumented in dev, and every output cached content-addressed." },
  { title: "Lazy compilation", body: "Dynamic import() boundaries compile on demand, so cold start scales with the route you open, not the whole app. A 24-route app cold-starts ~2x faster than compiling everything eagerly, unbundled or bundled." },
  { title: "SSR & server functions", body: "Streaming server rendering through TanStack Start, with hydration and SPA navigation. createServerFn() runs in process during a render and becomes an HTTP RPC in the browser, behind a same-origin CSRF check." },
  { title: "Cloudflare on the edge", body: "One build emits a Worker and its static assets. Wrangler vars and .dev.vars are read in dev through a context shim, so getCloudflareContext() resolves the same way it will in production." },
  { title: "Tailwind v4, no wiring", body: "Compiled through @tailwindcss/postcss on demand in dev and emitted in prod, linked into the SSR head so the first paint is already styled." },
  { title: "The conventions you write", body: "?url, ?raw and ?inline imports, import.meta.glob and import.meta.env, svgr for .svg components, MDX, CSS Modules, and hashed bare assets." },
  { title: "Resolution & interop", body: "Extension probing, package exports and imports conditions, and tsconfig paths resolve the way a bundler does. CommonJS deps are faceted into ES modules so named imports work." },
  { title: "Your vite.config, honored", body: "oj loads your plugins and runs their hooks with Vite's gating: apply, id filters, enforce order, and per-environment application. Virtual modules and framework plugins work as written." },
];

function Home() {
  return (
    <div className="wrap" id="top">
      <div className="masthead-title">
        <h1 className="display">oj</h1>
        <span className="badge">alpha</span>
      </div>

      <p className="lede">
        A Rust-native build tool for React apps. Your build tool should be{" "}
        <em>invisible</em>. oj puts a Rust core under the app you already have.
      </p>
      <p className="body">
        A fast dev server, server rendering with TanStack Start, Tailwind, and
        one-command Cloudflare deploys. Point it at your app and run: it reads
        your <code>vite.config</code>, honors your plugins, and compiles
        TypeScript and JSX through oxc, caching every output. It optimizes for
        memory and cold start, where running many builds under Vite gets
        expensive.
      </p>
      <p className="intro-links">
        <a href="#start">Get started</a>
        <span className="sep">·</span>
        <a href={GITHUB} target="_blank" rel="noreferrer">GitHub</a>
        <span className="sep">·</span>
        Built by{" "}
        <a href="https://github.com/raphamorim" target="_blank" rel="noreferrer">Raphael Amorim</a>
        {" "}and{" "}
        <a href="https://lovable.dev" target="_blank" rel="noreferrer">Lovable</a>
      </p>

      <section id="how">
        <h2 className="head">How it works</h2>
        <p className="section__sub">Point oj at your app and run. Four commands, start to ship.</p>
        <div className="rows">
          {STEPS.map((s) => (
            <div key={s.title} className="row">
              <div className="row__key">{s.title}</div>
              <div className="row__val">
                {s.body}
                <span className="cmd"><b>$</b> {s.cmd}</span>
              </div>
            </div>
          ))}
        </div>
      </section>

      <section id="benchmark">
        <h2 className="head">Benchmark</h2>
        <p className="section__sub">
          Like for like: the race below is oj and Vite 8.2 both in their default
          (unbundled) dev mode, 10,000 components, p50, M-series Mac. The table
          adds oj's --bundle mode and Vite's experimental bundled dev.
        </p>
        <BenchRace />
        <table className="benchtable">
          <thead>
            <tr>
              <th>Tool</th><th>Cold</th><th>Warm</th><th>HMR</th><th>Reload</th><th>Server RAM</th>
            </tr>
          </thead>
          <tbody>
            <tr data-oj="true">
              <td>oj --bundle</td><td>1569ms</td><td>1408ms</td><td>69ms</td><td>231ms</td><td>122MB</td>
            </tr>
            <tr>
              <td>vite (bundled dev)</td><td>1415ms</td><td>1417ms</td><td>64ms</td><td>277ms</td><td>1738MB</td>
            </tr>
            <tr>
              <td>vite (default)</td><td>5468ms</td><td>4957ms</td><td>114ms</td><td>1604ms</td><td>1504MB</td>
            </tr>
          </tbody>
        </table>
        <p className="bench__note">
          p50 on a generated 10k-component app, M-series Mac, Vite 8.2 (default
          and experimental bundled dev). oj wins cold, warm, and reload against
          default Vite by 3–7×, holds 12–14× less server memory than either Vite
          mode, and now matches Vite's bundled dev on HMR. Reproducible:{" "}
          <code>node bench/run.mjs 10000</code>.
        </p>
      </section>

      <section id="features">
        <h2 className="head">Features</h2>
        <p className="section__sub">Everything a real app needs, none of the wiring.</p>
        <div className="rows">
          {FEATURES.map((f) => (
            <div key={f.title} className="row">
              <div className="row__key">{f.title}</div>
              <div className="row__val">{f.body}</div>
            </div>
          ))}
        </div>
      </section>

      <section id="start">
        <h2 className="head">Get started</h2>
        <p className="section__sub">Install, develop, ship.</p>
        <pre className="code-block"><code>{`cargo install oj      # the CLI, from crates.io
oj dev .              # dev server with SSR + Fast Refresh
oj build .            # dist/ : client, SSR server, edge worker`}</code></pre>
        <p className="code-note">
          For Cloudflare, add a <code>wrangler.jsonc</code> pointing at the
          emitted worker and serving the client assets from the edge:
        </p>
        <pre className="code-block"><code>{`{
  "name": "my-app",
  "main": "dist/worker.mjs",
  "compatibility_date": "2024-11-01",
  "compatibility_flags": ["nodejs_compat"],
  "assets": { "directory": "dist/client", "binding": "ASSETS" }
}`}</code></pre>
        <p className="code-note">Then build and ship in one step:</p>
        <pre className="code-block"><code>oj build . &amp;&amp; wrangler deploy</code></pre>
        <p className="intro-links">
          <a href={GITHUB} target="_blank" rel="noreferrer">Read the source</a>
        </p>
      </section>
    </div>
  );
}

export const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/",
  component: Home,
});
