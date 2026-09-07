import { createRoute } from "@tanstack/react-router";

import { rootRoute } from "./__root";
import { Playground } from "../components/Playground";

const GITHUB = "https://github.com/lovablelabs/oj";

const PIPELINE = [
  {
    title: "The same pipeline",
    body: "The editor's files live in an in-memory tree inside oj_wasm, a crate that reuses oj's shared crates: oxc parses and transforms TypeScript and JSX through oj_compiler, Lightning CSS and grass compile stylesheets and scope CSS Modules through oj_css.",
  },
  {
    title: "No server anywhere",
    body: "Each compiled module becomes a Blob url, and an import map binds stable module ids to those urls, so imports (including cycles) resolve in the preview iframe without a dev server, a service worker, or a network round trip.",
  },
  {
    title: "Dependencies from the edge",
    body: "Bare imports like react and react-dom/client are left alone by the rewriter and mapped to esm.sh, pinned to one React so hooks never see two copies.",
  },
  {
    title: "Rebuilds as you type",
    body: "Every keystroke rewrites one in-memory file and re-runs the whole graph walk in WebAssembly. The full build of this little site takes a few milliseconds, so the preview simply follows your typing.",
  },
];

function Home() {
  return (
    <div id="top">
      <div className="wrap">
        <div className="masthead-title">
          <h1 className="display">oj</h1>
          <span className="badge">wasm</span>
        </div>

        <p className="lede">
          A Rust-native build tool for React apps, and this page is the proof:
          the compiler below is <em>oj itself</em>, compiled to WebAssembly,
          building a website inside your browser tab.
        </p>
        <p className="body">
          There is no dev server behind this demo. Your edits go into an
          in-memory filesystem, oxc transforms the TypeScript and JSX, Lightning
          CSS handles the stylesheets, and the preview loads the result through
          an import map of Blob urls. Everything happens on your machine.
        </p>
        <p className="intro-links">
          <a href="#how">How it works</a>
          <span className="sep">·</span>
          <a href="#start">Get oj</a>
          <span className="sep">·</span>
          <a href={GITHUB} target="_blank" rel="noreferrer">GitHub</a>
          <span className="sep">·</span>
          Built by{" "}
          <a href="https://github.com/raphamorim" target="_blank" rel="noreferrer">Raphael Amorim</a>
          {" "}and{" "}
          <a href="https://lovable.dev" target="_blank" rel="noreferrer">Lovable</a>
        </p>
      </div>

      <section id="playground" className="play-bleed">
        <Playground />
      </section>

      <div className="wrap">
        <section id="how">
          <h2 className="head">How it works</h2>
          <p className="section__sub">
            The playground is the <code>oj_wasm</code> crate: oj's compile
            pipeline behind a wasm-bindgen API.
          </p>
          <div className="rows">
            {PIPELINE.map((s) => (
              <div key={s.title} className="row">
                <div className="row__key">{s.title}</div>
                <div className="row__val">{s.body}</div>
              </div>
            ))}
          </div>
        </section>

        <section id="start">
          <h2 className="head">Get oj</h2>
          <p className="section__sub">
            The real thing is a native binary: a fast dev server with SSR and
            Fast Refresh, production builds, one-command Cloudflare deploys.
          </p>
          <pre className="code-block"><code>{`cargo install oj      # the CLI, from crates.io
oj dev .              # dev server with SSR + Fast Refresh
oj build .            # dist/ : client, SSR server, edge worker`}</code></pre>
          <p className="intro-links">
            <a href={GITHUB} target="_blank" rel="noreferrer">Read the source</a>
          </p>
        </section>
      </div>
    </div>
  );
}

export const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/",
  component: Home,
});
