import { useEffect, useRef, useState } from "react";

import { DEMO_FILES, INITIAL_FILE } from "../lib/demo-files";
import { buildSrcdoc, type BuildError, type BuildResult } from "../lib/preview";

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
  const iframeRef = useRef<HTMLIFrameElement>(null);
  const sessionRef = useRef<Session | null>(null);

  const [phase, setPhase] = useState<"boot" | "ready" | "failed">("boot");
  const [bootError, setBootError] = useState("");
  const [errors, setErrors] = useState<BuildError[]>([]);
  const [active, setActive] = useState(INITIAL_FILE);
  const [buildMs, setBuildMs] = useState<number | null>(null);

  useEffect(() => {
    let disposed = false;
    let debounce: ReturnType<typeof setTimeout> | undefined;

    const rebuild = (session: Session) => {
      const t0 = performance.now();
      const result: BuildResult = JSON.parse(session.project.build());
      setBuildMs(performance.now() - t0);
      setErrors(result.errors);
      // A failed build (mid-keystroke syntax error, missing import) keeps the
      // last good preview on screen; the error strip carries the diagnostics.
      if (result.ok && result.html && iframeRef.current) {
        iframeRef.current.srcdoc = buildSrcdoc(result);
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
        const [wasm, view, state, setup, langJs, langCss, langHtml] = await Promise.all([
          dynamicImport("/oj-wasm/oj_wasm.js"),
          import("@codemirror/view"),
          import("@codemirror/state"),
          import("codemirror"),
          import("@codemirror/lang-javascript"),
          import("@codemirror/lang-css"),
          import("@codemirror/lang-html"),
        ]);
        await wasm.default({ module_or_path: "/oj-wasm/oj_wasm_bg.wasm" });
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
        <iframe ref={iframeRef} className="play__frame" title="oj wasm preview" />
      </div>
    </div>
  );
}
