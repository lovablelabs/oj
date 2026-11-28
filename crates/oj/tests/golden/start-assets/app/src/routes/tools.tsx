import { createFileRoute } from "@tanstack/react-router";
import { useEffect, useState } from "react";

export const Route = createFileRoute("/tools")({
  component: Tools,
});

function Tools() {
  const [sum, setSum] = useState(0);
  useEffect(() => {
    const worker = new Worker(new URL("../workers/sum.ts", import.meta.url), { type: "module" });
    worker.onmessage = (e) => setSum(e.data);
    worker.postMessage([1, 2, 3]);
    return () => worker.terminate();
  }, []);
  return <p>{sum}</p>;
}
