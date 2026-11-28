const lang = document.documentElement.lang || "en";

import(`./locales/${lang}.js`).then((m) => {
  document.body.textContent = m.hello;
});
