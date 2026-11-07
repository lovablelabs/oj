// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The @lingui macro entrypoints (`@lingui/macro`, `@lingui/core/macro`,
//! `@lingui/react/macro`) are normally compiled away by @lingui/swc-plugin,
//! which oj cannot run. oj rewrites all three to a runtime identity shim at
//! `/@oj/lingui-macro-shim.js`, by name and with no @lingui package
//! installed, so an app using them loads and renders its source strings.
//! Every shim export runs in a real page.
//! Not covered here: the one-time stderr warning about the degraded shim
//! (a process-wide `Once` written straight to stderr).

use std::time::Duration;

use crate::browser::Page;
use crate::App;

const WAIT: Duration = Duration::from_secs(20);

const MAIN: &str = r##"import { t, msg, defineMessage, plural, selectOrdinal, select } from "@lingui/core/macro";
import { Trans, Plural, Select, SelectOrdinal, useLingui } from "@lingui/react/macro";
import { t as ut } from "@lingui/macro";
const name = "World";
const bound = useLingui();
const out = {
  t: t`Hello ${name}`,
  umbrellaT: ut`Umbrella ${name}`,
  msg: msg`Bye ${name}`,
  defineMessage: defineMessage`Def ${name}`,
  plural2: plural(2, { one: "# item", other: "# items" }),
  plural1: plural(1, { one: "# item", other: "# items" }),
  ordinal: selectOrdinal(1, { one: "#st", other: "#th" }),
  select: select("a", { a: "Apple", other: "Other" }),
  hookT: bound.t`Hi ${name}`,
  hookI18n: bound.i18n._("passthrough"),
  trans: Trans({ message: "TransMessage" }),
  transChildren: Trans({ children: "TransChild" }),
  pluralC: Plural({ value: 2, one: "# file", other: "# files" }),
  selectOrdinalC: SelectOrdinal({ value: 1, one: "#st", other: "#th" }),
  selectC: Select({ value: "a", a: "AA", other: "OO" }),
};
window.__OJ_LINGUI = out;
document.getElementById("root").textContent = JSON.stringify(out);
"##;

#[tokio::test(flavor = "multi_thread")]
async fn lingui_macros_are_served_by_the_identity_shim() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"lingui-shim-app","version":"1.0.0"}"#,
        ),
        (
            "index.html",
            r#"<!doctype html><html><head><title>lingui</title></head><body><div id="root"></div><script type="module" src="/main.js"></script></body></html>"#,
        ),
        ("main.js", MAIN),
    ]);
    let server = app.serve().await;

    let shim = server.get("/@oj/lingui-macro-shim.js").await;
    assert_eq!(shim.status, 200, "shim route serves:\n{}", shim.body);
    let ctype = shim
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ctype.contains("javascript"),
        "shim served as JS module: {ctype}"
    );
    let exports_t = regex::Regex::new(r"export function t\b").unwrap();
    assert!(
        exports_t.is_match(&shim.body),
        "shim exports t:\n{}",
        shim.body
    );
    let exports_hook = regex::Regex::new(r"export function useLingui\b").unwrap();
    assert!(
        exports_hook.is_match(&shim.body),
        "shim exports useLingui:\n{}",
        shim.body
    );

    let main = server.text("/main.js").await;
    assert!(
        !main.contains(r#""@lingui/core/macro""#),
        "the raw @lingui/core/macro specifier must be rewritten away:\n{main}"
    );
    let rewrites = main.matches("/@oj/lingui-macro-shim.js").count();
    assert!(
        rewrites >= 3,
        "all three macro imports rewritten to the shim (got {rewrites}):\n{main}"
    );

    if crate::browser::skip() {
        return;
    }
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for("window.__OJ_LINGUI !== undefined", WAIT)
        .await;
    let out: serde_json::Value = page.eval("window.__OJ_LINGUI").await;
    assert_eq!(page.errors(), Vec::<String>::new(), "no page errors");
    let expected = [
        ("t", "Hello World"),
        ("umbrellaT", "Umbrella World"),
        ("msg", "Bye World"),
        ("defineMessage", "Def World"),
        ("plural2", "2 items"),
        ("plural1", "1 item"),
        ("ordinal", "1st"),
        ("select", "Apple"),
        ("hookT", "Hi World"),
        ("hookI18n", "passthrough"),
        ("trans", "TransMessage"),
        ("transChildren", "TransChild"),
        ("pluralC", "2 files"),
        ("selectOrdinalC", "1st"),
        ("selectC", "AA"),
    ];
    for (key, want) in expected {
        assert_eq!(out[key], want, "{key} in {out}");
    }
    let root_text: String = page
        .eval(r#"document.getElementById("root").textContent"#)
        .await;
    assert!(
        root_text.contains("Hello World"),
        "shim output rendered into the DOM: {root_text}"
    );
    page.close().await;
}
