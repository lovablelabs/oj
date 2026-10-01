// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

pub fn svg_to_component(svg: &str) -> String {
    let cleaned = strip_noise(svg);
    let jsx = transform_tags(&cleaned);
    format!(
        "const SvgComponent = (props) => (\n{}\n);\nexport default SvgComponent;\n",
        jsx.trim()
    )
}

fn strip_noise(svg: &str) -> String {
    let mut out = String::with_capacity(svg.len());
    let mut rest = svg;
    loop {
        let cut = rest.find("<?").map(|i| (i, "?>")).into_iter();
        let cut2 = rest.find("<!--").map(|i| (i, "-->"));
        let doctype = rest
            .find("<!DOCTYPE")
            .or_else(|| rest.find("<!doctype"))
            .map(|i| (i, ">"));
        let next = cut.chain(cut2).chain(doctype).min_by_key(|(i, _)| *i);
        let Some((start, end_tok)) = next else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..start]);
        match rest[start..].find(end_tok) {
            Some(rel) => rest = &rest[start + rel + end_tok.len()..],
            None => break,
        }
    }
    out
}

fn transform_tags(svg: &str) -> String {
    let mut out = String::with_capacity(svg.len() + 64);
    let mut rest = svg;
    let mut root_done = false;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        rest = &rest[lt..];
        let Some(gt) = rest.find('>') else {
            out.push_str(rest);
            return out;
        };
        let tag = &rest[..=gt];
        let (rewritten, was_root) = rewrite_tag(tag, !root_done);
        if was_root {
            root_done = true;
        }
        out.push_str(&rewritten);
        rest = &rest[gt + 1..];
    }
    out.push_str(rest);
    out
}

fn rewrite_tag(tag: &str, root_candidate: bool) -> (String, bool) {
    let inner = &tag[1..tag.len() - 1];
    if inner.starts_with('/') {
        return (tag.to_string(), false);
    }
    let self_closing = inner.ends_with('/');
    let inner = inner.strip_suffix('/').unwrap_or(inner);

    let name_end = inner
        .find(|c: char| c.is_whitespace())
        .unwrap_or(inner.len());
    let name = &inner[..name_end];
    let attrs_src = inner[name_end..].trim();
    let is_root = root_candidate && name == "svg";

    let mut out = String::new();
    out.push('<');
    out.push_str(name);
    for (key, value) in parse_attrs(attrs_src) {
        out.push(' ');
        out.push_str(&rewrite_attr(&key, value.as_deref()));
    }
    if is_root {
        out.push_str(" {...props}");
    }
    if self_closing {
        out.push_str(" />");
    } else {
        out.push('>');
    }
    (out, is_root)
}

fn parse_attrs(src: &str) -> Vec<(String, Option<String>)> {
    let mut attrs = Vec::new();
    let bytes = src.as_bytes();
    let mut i = 0;
    while i < src.len() {
        while i < src.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let start = i;
        while i < src.len() && bytes[i] != b'=' && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i == start {
            break;
        }
        let key = src[start..i].to_string();
        while i < src.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < src.len() && bytes[i] == b'=' {
            i += 1;
            while i < src.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < src.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                let quote = bytes[i];
                i += 1;
                let vstart = i;
                while i < src.len() && bytes[i] != quote {
                    i += 1;
                }
                let value = src[vstart..i].to_string();
                i += 1;
                attrs.push((key, Some(value)));
            } else {
                let vstart = i;
                while i < src.len() && !bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                attrs.push((key, Some(src[vstart..i].to_string())));
            }
        } else {
            attrs.push((key, None));
        }
    }
    attrs
}

fn rewrite_attr(key: &str, value: Option<&str>) -> String {
    if key.eq_ignore_ascii_case("style") {
        if let Some(v) = value {
            return format!("style={{{{{}}}}}", style_to_object(v));
        }
    }
    let name = jsx_attr_name(key);
    match value {
        Some(v) => format!("{name}={}", serde_json::Value::String(v.to_string())),
        None => name,
    }
}

fn jsx_attr_name(key: &str) -> String {
    match key {
        "class" => return "className".to_string(),
        "for" => return "htmlFor".to_string(),
        _ => {}
    }
    if key.starts_with("data-") || key.starts_with("aria-") {
        return key.to_string();
    }
    camel_case(key, &['-', ':'])
}

/// `stroke-width` -> `strokeWidth`, splitting on any of `seps`.
fn camel_case(s: &str, seps: &[char]) -> String {
    let mut out = String::with_capacity(s.len());
    let mut upper = false;
    for c in s.chars() {
        if seps.contains(&c) {
            upper = true;
        } else if upper {
            out.extend(c.to_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}

fn style_to_object(style: &str) -> String {
    let mut parts = Vec::new();
    for decl in style.split(';') {
        let decl = decl.trim();
        if decl.is_empty() {
            continue;
        }
        let Some((prop, val)) = decl.split_once(':') else {
            continue;
        };
        let key = css_prop_to_camel(prop.trim());
        parts.push(format!(
            "{}: {}",
            key,
            serde_json::Value::String(val.trim().to_string())
        ));
    }
    parts.join(", ")
}

fn css_prop_to_camel(prop: &str) -> String {
    if prop.starts_with("--") {
        return serde_json::Value::String(prop.to_string()).to_string();
    }
    camel_case(prop, &['-'])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_and_exports_default_component() {
        let out = svg_to_component(r#"<svg width="24"><path d="M0 0"/></svg>"#);
        assert!(out.contains("const SvgComponent = (props) =>"), "{out}");
        assert!(out.contains("export default SvgComponent;"), "{out}");
    }

    #[test]
    fn injects_props_on_root_svg_only() {
        let out = svg_to_component(r#"<svg><g><svg></svg></g></svg>"#);
        assert_eq!(out.matches("{...props}").count(), 1, "{out}");
        assert!(out.contains("<svg {...props}>"), "{out}");
    }

    #[test]
    fn rewrites_kebab_and_class_and_namespaced_attrs() {
        let out = svg_to_component(
            r##"<svg class="a" stroke-width="2" clip-path="url(#c)" xlink:href="#x"><path/></svg>"##,
        );
        assert!(out.contains("className=\"a\""), "{out}");
        assert!(out.contains("strokeWidth=\"2\""), "{out}");
        assert!(out.contains("clipPath="), "{out}");
        assert!(out.contains("xlinkHref="), "{out}");
    }

    #[test]
    fn converts_inline_style_to_object() {
        let out = svg_to_component(r#"<svg style="fill:red;stroke-width:2"><path/></svg>"#);
        assert!(out.contains("style={{"), "{out}");
        assert!(out.contains("fill: \"red\""), "{out}");
        assert!(out.contains("strokeWidth: \"2\""), "{out}");
    }

    #[test]
    fn keeps_data_and_aria_attrs() {
        let out = svg_to_component(r#"<svg data-x="1" aria-hidden="true"><path/></svg>"#);
        assert!(out.contains("data-x=\"1\""), "{out}");
        assert!(out.contains("aria-hidden=\"true\""), "{out}");
    }

    #[test]
    fn strips_xml_prolog_doctype_comments() {
        let out =
            svg_to_component("<?xml version=\"1.0\"?><!DOCTYPE svg><!-- c --><svg><path/></svg>");
        assert!(!out.contains("<?xml"), "{out}");
        assert!(!out.contains("DOCTYPE"), "{out}");
        assert!(!out.contains("<!--"), "{out}");
        assert!(out.contains("<svg {...props}>"), "{out}");
    }
}
