const pages = import.meta.glob("./pages/**/*.js");
const texts = import.meta.glob("./texts/*.txt", { query: "?raw", import: "default", eager: true });

export async function page(name) {
  const load = pages[`./pages/${name}.js`];
  return load ? load() : null;
}

export async function locale(lang) {
  return import(`./locales/${lang}.js`);
}

export async function note(name) {
  return import(`./texts/${name}.txt?raw`);
}

export async function bare(name) {
  return import(`./pages/${name}`);
}

console.log(texts, page, locale, note, bare);
