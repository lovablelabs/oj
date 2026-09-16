import { useEffect, useRef, useState } from "react";

import { DEMO_FILES, INITIAL_FILE } from "../lib/demo-files";
import { buildSrcdoc, revokeRetired, type BuildError, type BuildResult } from "../lib/preview";

// Everything heavy (the wasm module, CodeMirror) loads client-side in the
// boot effect: the route is server-rendered and this component must render as
// an empty shell on the server.

type Session = {
  project: { writeFile(path: string, contents: string): void; build(): string };
  view: any;
  states: Map<string, any>;
  makeState: (path: string, doc: string) => any;
  contents: Record<string, string>;
  active: string;
};

export function Playground() {
  const editorHostRef = useRef<HTMLDivElement>(null);
  const frameARef = useRef<HTMLIFrameElement>(null);
  const frameBRef = useRef<HTMLIFrameElement>(null);
  const frontRef = useRef<0 | 1>(0);
  const lastDocRef = useRef("");
  const sessionRef = useRef<Session | null>(null);

  const [phase, setPhase] = useState<"boot" | "ready" | "failed">("boot");
  const [bootError, setBootError] = useState("");
  const [errors, setErrors] = useState<BuildError[]>([]);
  const [active, setActive] = useState(INITIAL_FILE);
  const [buildMs, setBuildMs] = useState<number | null>(null);

  useEffect(() => {
    let disposed = false;
    let debounce: ReturnType<typeof setTimeout> | undefined;

    // Double-buffered preview: a fresh srcdoc tears the document down before
    // the new one has parsed, so typing showed a blank flash per rebuild. The
    // new build loads into the hidden iframe and the panes swap on its `load`
    // event; the old page stays visible the whole time. `onload` is assigned
    // as a property so a rebuild that lands while the back frame is still
    // loading simply supersedes the pending swap.
    const present = (doc: string) => {
      const frames = [frameARef.current, frameBRef.current];
      const front = frames[frontRef.current];
      const back = frames[frontRef.current === 0 ? 1 : 0];
      if (!front || !back) return;
      back.onload = () => {
        back.onload = null;
        back.dataset.front = "true";
        back.removeAttribute("aria-hidden");
        front.dataset.front = "false";
        front.setAttribute("aria-hidden", "true");
        frontRef.current = frontRef.current === 0 ? 1 : 0;
        // The displaced document is off screen now; its blob urls (and any
        // batches superseded before ever showing) can finally go.
        revokeRetired();
      };
      back.srcdoc = doc;
    };

    const rebuild = (session: Session) => {
      // Compile errors come back as data; the catch is for a wasm panic, which
      // would otherwise throw inside the debounce timer and silently stop all
      // future rebuilds.
      try {
        const t0 = performance.now();
        const json = session.project.build();
        const result: BuildResult = JSON.parse(json);
        setBuildMs(performance.now() - t0);
        setErrors(result.errors);
        // A failed build (mid-keystroke syntax error, missing import) keeps the
        // last good preview on screen; the error strip carries the diagnostics.
        // The dedupe compares the raw build output, BEFORE blob urls are
        // minted: an edit that compiles to identical output (comment tweaks)
        // must not reload the preview or churn blobs.
        if (result.ok && result.html && json !== lastDocRef.current) {
          lastDocRef.current = json;
          present(buildSrcdoc(result));
        }
      } catch (err) {
        setErrors([{ path: "oj_wasm", message: `build crashed: ${err instanceof Error ? err.message : String(err)}` }]);
      }
    };

    (async () => {
      try {
        // The wasm-bindgen module is a public asset resolved in the browser at
        // runtime; the import must stay opaque to every bundler that sees this
        // file (oj's rolldown, wrangler's esbuild for the SSR worker), so it
        // goes through Vite's own dynamicImport trick. Built here, not at
        // module scope: Workers disallow Function construction at runtime.
        const dynamicImport = new Function("u", "return import(u)") as (u: string) => Promise<any>;
        // The wasm binary is by far the largest download; kick its fetch and
        // instantiation off inside the same Promise.all instead of serializing
        // it behind the CodeMirror chunks.
        const [wasm, view, state, setup, langJs, langCss, langHtml, dark] = await Promise.all([
          dynamicImport("/oj-wasm/oj_wasm.js").then(async (m: any) => {
            await m.default({ module_or_path: "/oj-wasm/oj_wasm_bg.wasm" });
            return m;
          }),
          import("@codemirror/view"),
          import("@codemirror/state"),
          import("codemirror"),
          import("@codemirror/lang-javascript"),
          import("@codemirror/lang-css"),
          import("@codemirror/lang-html"),
          import("@codemirror/theme-one-dark"),
        ]);
        if (disposed || !editorHostRef.current) return;

        const project = new wasm.OjProject();
        const contents: Record<string, string> = { ...DEMO_FILES };
        for (const [path, text] of Object.entries(contents)) project.writeFile(path, text);

        const language = (path: string) => {
          if (path.endsWith(".css")) return langCss.css();
          if (path.endsWith(".html")) return langHtml.html();
          return langJs.javascript({ jsx: true, typescript: true });
        };

        const session: Session = {
          project,
          view: null,
          states: new Map(),
          makeState: (path: string, doc: string) =>
            state.EditorState.create({
              doc,
              extensions: [
                setup.basicSetup,
                dark.oneDark,
                language(path),
                view.EditorView.updateListener.of((update: any) => {
                  if (!update.docChanged) return;
                  const s = sessionRef.current;
                  if (!s) return;
                  const text = update.state.doc.toString();
                  s.contents[s.active] = text;
                  s.project.writeFile(s.active, text);
                  clearTimeout(debounce);
                  debounce = setTimeout(() => rebuild(s), 250);
                }),
              ],
            }),
          contents,
          active: INITIAL_FILE,
        };
        session.view = new view.EditorView({
          state: session.makeState(INITIAL_FILE, contents[INITIAL_FILE]),
          parent: editorHostRef.current,
        });
        sessionRef.current = session;
        setPhase("ready");
        rebuild(session);
      } catch (err) {
        if (!disposed) {
          setPhase("failed");
          setBootError(err instanceof Error ? err.message : String(err));
        }
      }
    })();

    return () => {
      disposed = true;
      clearTimeout(debounce);
      sessionRef.current?.view?.destroy();
      // wasm-bindgen objects hold linear memory until freed; React StrictMode
      // remounts would otherwise leak a project per mount.
      (sessionRef.current?.project as any)?.free?.();
      sessionRef.current = null;
    };
  }, []);

  const openFile = (path: string) => {
    const session = sessionRef.current;
    if (!session || path === session.active) return;
    session.states.set(session.active, session.view.state);
    session.active = path;
    const restored = session.states.get(path) ?? session.makeState(path, session.contents[path]);
    session.view.setState(restored);
    setActive(path);
  };

  return (
    <div className="play" data-phase={phase}>
      <div className="play__pane play__editor">
        <div className="play__tabs" role="tablist">
          {Object.keys(DEMO_FILES).map((path) => (
            <button
              key={path}
              role="tab"
              aria-selected={active === path}
              className="play__tab"
              data-active={active === path}
              onClick={() => openFile(path)}
            >
              {path.replace(/^\/(src\/)?/, "")}
            </button>
          ))}
        </div>
        <div className="play__cm" ref={editorHostRef}>
          {phase === "boot" && <p className="play__status">fetching oj_wasm…</p>}
          {phase === "failed" && (
            <p className="play__status play__status--error">could not start the wasm build: {bootError}</p>
          )}
        </div>
        {errors.length > 0 && (
          <div className="play__errors">
            {errors.map((e, i) => (
              <div key={i} className="play__error">
                <b>{e.path}</b> {e.message}
              </div>
            ))}
          </div>
        )}
      </div>
      <div className="play__pane play__preview">
        <div className="play__previewbar">
          <span>preview</span>
          {buildMs !== null && <span className="play__ms">rebuilt in {buildMs.toFixed(1)}ms</span>}
        </div>
        <div className="play__framewrap">
          <iframe ref={frameARef} className="play__frame" data-front="true" title="oj wasm preview" />
          <iframe ref={frameBRef} className="play__frame" data-front="false" aria-hidden="true" title="oj wasm preview" />
        </div>
      </div>
    </div>
  );
}
