/** @jsxRuntime classic */
/** @jsx h */
export function h(tag: string, props: Record<string, string> | null, ...children: (string | Node)[]): HTMLElement {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(props ?? {})) el.setAttribute(k, v);
  el.append(...children);
  return el;
}

export const render = (text: string, times: number) => (
  <ul class="list">
    {Array.from({ length: times }, (_, i) => (
      <li key={String(i)}>{text}</li>
    ))}
  </ul>
);
