export class Store {
  static count = 0;
  #secret = 42;
  value;
  config = { label: "store" };

  read() {
    return this.#secret + (this.value ?? 0);
  }

  static {
    Store.count = 1;
  }
}
