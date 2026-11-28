import debug from "debug";
import { boot } from "./boot.js";
import { format } from "./shared.js";
import { today } from "./vendor/dates/index.js";
import "./main.css";

const log = debug("app");
log(boot(), format(today()), import.meta.env.VITE_APP_ENV, import.meta.env.VITE_API_URL, import.meta.env.MODE);
if (globalThis.__APP_SSR__) log("never on the client");
if (globalThis.__LOCAL_PRICING__) log("dropped from the build");

const routes = {
  "/dashboard": () => import("./routes/dashboard.js"),
  "/settings": () => import("./routes/settings.js"),
  "/report": () => import("./routes/report.js"),
};
const locales = { en: () => import("./i18n/en.js"), fr: () => import("./i18n/fr.js") };

async function go(path, locale) {
  const [route, messages] = await Promise.all([routes[path](), locales[locale]()]);
  log(route.render(messages.default));
}

go(location.pathname in routes ? location.pathname : "/dashboard", navigator.language.startsWith("fr") ? "fr" : "en");
