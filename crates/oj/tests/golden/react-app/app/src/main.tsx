import { lazy, StrictMode, Suspense, useState } from "react";
import { createRoot } from "react-dom/client";

import Logo from "./logo.svg";
import starUrl from "./star.svg?url";
import starRaw from "./star.svg?raw";
import notes from "./content/notes.txt?raw";
import data from "./content/data.json";
import Welcome from "./content/welcome.mdx";
import "./app.css";

const Settings = lazy(() => import("./Settings"));

declare module "*.mdx" {
  const Component: (props: Record<string, unknown>) => JSX.Element;
  export default Component;
}

function App() {
  const [open, setOpen] = useState(false);
  return (
    <StrictMode>
      <header className="header">
        <Logo width={32} height={32} />
        <img src={starUrl} alt="" />
        <span dangerouslySetInnerHTML={{ __html: starRaw }} />
        <h1>{data.title}</h1>
      </header>
      <Welcome />
      <pre>{notes}</pre>
      <button onClick={() => setOpen((o) => !o)}>{open ? "close" : "open"}</button>
      {open && (
        <Suspense fallback={<p>loading</p>}>
          <Settings items={data.items} />
        </Suspense>
      )}
    </StrictMode>
  );
}

createRoot(document.getElementById("root")!).render(<App />);
