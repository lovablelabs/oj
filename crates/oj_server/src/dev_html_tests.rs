use std::collections::BTreeMap;
use std::path::Path;

use super::*;

/// The proxy cache and a style compile that leaves sheets as written, as
/// Vite's CSS pipeline does for the golden pages (no PostCSS config,
/// root-absolute urls); vite:css-post escapes an attribute's quotes.
#[derive(Default)]
struct TestHost {
    proxy: HtmlProxyCache,
    styles: Mutex<Vec<String>>,
}

impl DevHtmlHost for TestHost {
    async fn compile_style(&self, id: &str, code: &str) -> Result<String, String> {
        self.styles.lock().unwrap().push(id.to_string());
        Ok(if id.contains("&style-attr") {
            code.replace('"', "&quot;")
        } else {
            code.to_string()
        })
    }

    fn proxy(&self) -> &HtmlProxyCache {
        &self.proxy
    }
}

fn fixture(name: &str) -> serde_json::Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/dev-html")
        .join(name);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Every golden page, under every config, gives Vite's bytes and Vite's
/// html-proxy modules.
#[tokio::test]
async fn pages_match_vites_dev_html_transform() {
    let pages = fixture("pages.json");
    let goldens = fixture("goldens.json");
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    for page in pages.as_array().unwrap() {
        if let Some(file) = page["file"].as_str() {
            std::fs::write(root.join(file), "<html></html>").unwrap();
        }
    }
    let mut checked = 0;
    for (key, golden) in goldens.as_object().unwrap() {
        let base = golden["base"].as_str().unwrap();
        let env: BTreeMap<String, String> = golden["env"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    v.as_str().map_or_else(|| v.to_string(), str::to_string),
                )
            })
            .collect();
        let prefixes: Vec<String> = golden["envPrefix"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p.as_str().unwrap().to_string())
            .collect();
        let client = vec![posix_join(&[base, "@vite/client"])];
        let cfg = DevHtmlConfig {
            root: &root,
            base,
            env: &env,
            env_prefixes: &prefixes,
            nonce: golden["nonce"].as_str(),
            client_srcs: &client,
        };
        for page in pages.as_array().unwrap() {
            let name = page["name"].as_str().unwrap();
            let url = page["url"].as_str().unwrap();
            let host = TestHost::default();
            let filename = root.join(url.trim_start_matches('/'));
            let req = DevHtmlRequest {
                path: url,
                filename: &filename,
                original_url: page["originalUrl"].as_str().unwrap(),
            };
            let out = transform_dev_html(page["html"].as_str().unwrap(), &cfg, &req, &host)
                .await
                .unwrap();
            let want = &golden["pages"][name];
            assert_eq!(out, want["html"].as_str().unwrap(), "{key}: {name}");
            for (proxy_url, code) in want["proxies"].as_object().unwrap() {
                let stripped = if base != "/" {
                    proxy_url
                        .strip_prefix(&base[..base.len() - 1])
                        .unwrap_or(proxy_url)
                } else {
                    proxy_url
                };
                let id = match stripped.strip_prefix("/@id/") {
                    Some(rest) => rest.replacen("__x00__", "\0", 1),
                    None => stripped.to_string(),
                };
                assert_eq!(
                    host.proxy.load(&id, &root).as_deref(),
                    Some(code.as_str().unwrap()),
                    "{key}: {name}: {proxy_url}"
                );
            }
            checked += 1;
        }
    }
    assert_eq!(checked, 27, "every page under every config");
}

#[test]
fn scanning_finds_elements_the_way_parse5_does() {
    let html = "<!-- <script>no</script> --><SCRIPT Type=\"module\" src=a.js src=\"b.js\"></SCRIPT>\n<style>p{}</style><script>if (a </script) {}</script ><img alt='&quot;q&amp;' src=\"x.png\"/>";
    let nodes = scan_html(html);
    let names: Vec<&str> = nodes.iter().map(|n| n.name.as_str()).collect();
    assert_eq!(names, ["script", "style", "script", "img"]);
    let attrs: Vec<(&str, &str)> = nodes[0]
        .attrs
        .iter()
        .map(|a| (a.name.as_str(), a.value.as_str()))
        .collect();
    assert_eq!(attrs, [("type", "module"), ("src", "a.js")]);
    let src = nodes[0].attr("src").unwrap().loc;
    assert_eq!(&html[src.start..src.end], "src=a.js");
    assert_eq!(nodes[1].text.as_ref().unwrap().0, "p{}");
    assert_eq!(nodes[2].text.as_ref().unwrap().0, "if (a </script) {}");
    assert_eq!(
        &html[nodes[2].loc.start..nodes[2].loc.end],
        "<script>if (a </script) {}</script >"
    );
    assert_eq!(nodes[3].attrs[0].value, "\"q&");
}

#[test]
fn posix_join_and_normalize_follow_node() {
    assert_eq!(posix_join(&["/", "./a.png"]), "/a.png");
    assert_eq!(posix_join(&["/app/", "/x.js"]), "/app/x.js");
    assert_eq!(posix_join(&["/", ""]), "/");
    assert_eq!(posix_join(&["/app/", "../../x"]), "/x");
    assert_eq!(posix_normalize("a//b/"), "a/b/");
    assert_eq!(posix_normalize("./"), "./");
}
