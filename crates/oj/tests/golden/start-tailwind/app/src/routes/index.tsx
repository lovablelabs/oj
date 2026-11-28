import { createFileRoute } from "@tanstack/react-router";

import styles from "./index.module.css";

export const Route = createFileRoute("/")({
  component: () => (
    <main className="flex flex-col gap-4 p-4">
      <h1 className={styles.title}>tailwind</h1>
      <p className={`card ${styles.muted}`}>a card</p>
      <a className="text-brand hover:underline" href="/about">
        about
      </a>
    </main>
  ),
});
