import { createFileRoute } from "@tanstack/react-router";
import { useEffect, useState } from "react";

import { shout } from "@app/lib/format";
import "../about.css";

export const Route = createFileRoute("/about")({
  component: About,
});

function About() {
  const [extra, setExtra] = useState("");
  useEffect(() => {
    import("../lib/heavy").then((m) => setExtra(m.heavy()));
  }, []);
  return (
    <section className="about">
      <h1>{shout("about")}</h1>
      <p>{extra}</p>
    </section>
  );
}
