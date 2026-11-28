import "./main.css";

export async function theme() {
  await import("./theme.css");
  return import("./styles-only.js");
}

export async function widget() {
  const m = await import("./widget.js");
  return m.widget();
}

theme().then(() => widget());
