export class Store<T extends object> {
  #state: T;
  static count = 0;
  listeners = new Set<(s: T) => void>();

  constructor(initial: T) {
    this.#state = initial;
    Store.count++;
  }

  get state() {
    return this.#state;
  }

  patch(next: Partial<T>) {
    this.#state = { ...this.#state, ...next };
    this.listeners.forEach((l) => l(this.#state));
  }
}

export function pick(settings: { theme?: { name?: string } }, fallback: string) {
  let name = settings.theme?.name ?? fallback;
  name ||= "plain";
  return name;
}

export async function* pages(n: number) {
  for (let i = 1; i <= n; i++) yield i;
}

export const big = 1_000_000;
