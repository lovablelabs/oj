const pages = import.meta.glob("./pages/*.js");
const eager = import.meta.glob("./pages/*.js", { eager: true, import: "title" });

console.log(Object.keys(pages), eager, import.meta.env.MODE, import.meta.env.VITE_GREETING, import.meta.env.PROD);
if (import.meta.env.DEV) console.log("dev only");
console.log(typeof import.meta.env.SECRET, import.meta.env.BASE_URL);
for (const load of Object.values(pages)) load().then((m) => console.log(m.title));
