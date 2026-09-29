import type { ReactNode } from "react";
import type { RouteData } from "@/ui";

// The root layout has its own loader; its data is available on every route.
export function loader() {
  return { app: "oj" };
}

// Default head for every route; deeper routes override the title.
export function meta() {
  return [
    { title: "oj app" },
    { name: "generator", content: "oj" },
    { property: "og:site_name", content: "oj" },
    { property: "og:type", content: "website" },
  ];
}

export default function RootLayout({ children, data }: { children: ReactNode; data: RouteData }) {
  return (
    <div data-layout="root">
      <header data-app-header data-app-name={String(data?.app ?? "")}>
        oj app
      </header>
      {children}
    </div>
  );
}
