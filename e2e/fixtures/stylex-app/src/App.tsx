import { useState } from "react";
import { Card } from "./Card";

export function App() {
  const [active, setActive] = useState(false);
  return <Card active={active} onToggle={() => setActive((a) => !a)} />;
}
