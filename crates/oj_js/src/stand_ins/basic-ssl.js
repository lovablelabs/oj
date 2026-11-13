// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// @vitejs/plugin-basic-ssl as a vite.config sees it under oj, which makes
// the plugin's certificate natively and never runs the real plugin (whose
// configResolved would generate an RSA key in the config evaluation). The
// plugin returned carries the options it was given; config extraction reads
// them, and oj does the rest of the plugin's work in Rust. The loader appends
// the exports, as ESM or CommonJS.

function viteBasicSslPlugin(options) {
  return { name: "vite:basic-ssl", __ojBasicSslOptions: options ?? {} };
}

// The package's other export, its generator: oj writes the certificate file
// before it serves, so a config has nothing to call this for.
function getCertificate() {
  throw new Error(
    "@vitejs/plugin-basic-ssl's getCertificate is not available under oj, which makes the plugin's certificate itself: list basicSsl() in plugins",
  );
}
