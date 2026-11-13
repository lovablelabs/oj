// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `@vitejs/plugin-basic-ssl`, run natively: the plugin's `configResolved`
//! reads `<certDir>/_cert.pem` (default `<cacheDir>/basic-ssl`), makes a
//! self-signed certificate there when the file is missing or past its
//! `notAfter`, and sets `server.https` and `preview.https` to it, key and
//! certificate in one PEM. oj's config evaluation hands the plugin's module a
//! stand-in that only records its options (oj_js `stand_ins`), the literal
//! reader reads the call itself, and this module does the plugin's work with
//! rcgen: the same file, so an app that alternates `vite` and `oj` keeps one
//! certificate the browser trusted once, and the same subject and names
//! (`name`, default `example.org`, as the CN; localhost, 127.0.0.1, fe80::1
//! and the `domains` option as alternative names; `ttlDays`, default 30). The
//! key is ECDSA P-256 where the plugin's is RSA 2048: node reads either from
//! the cache file, and no RSA key generation runs in a config evaluation.

use std::path::{Path, PathBuf};

/// The plugin's cache file under its directory.
const CERT_FILE: &str = "_cert.pem";

/// The `name`, `domains`, `certDir` and `ttlDays` of `basicSsl(options)`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Options {
    pub name: String,
    pub domains: Vec<String>,
    pub cert_dir: Option<PathBuf>,
    pub ttl_days: i64,
}

impl Options {
    /// The options object extraction recorded; a value outside the plugin's
    /// types takes the plugin's default, as its `??` fallbacks do for a
    /// missing one.
    pub(crate) fn from_json(options: &serde_json::Value) -> Self {
        Self {
            name: options
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| "example.org".to_string()),
            domains: options
                .get("domains")
                .and_then(|v| v.as_array())
                .map(|list| {
                    list.iter()
                        .filter_map(|d| d.as_str())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            cert_dir: options
                .get("certDir")
                .and_then(|v| v.as_str())
                .map(PathBuf::from),
            ttl_days: options
                .get("ttlDays")
                .and_then(|v| v.as_f64())
                .filter(|d| d.is_finite() && *d >= 1.0)
                .map(|d| d as i64)
                .unwrap_or(30),
        }
    }
}

/// The certificate for `options`: the kept one under the plugin's directory
/// when it still parses and has not expired, else a fresh one written there.
/// `root` resolves a relative `certDir` (the plugin resolves it from the
/// process's working directory, which under `vite` is the app); `cache_dir`
/// is Vite's resolved `cacheDir`. A write failure keeps the pair for this run
/// and says so, as the plugin's unawaited write does silently.
pub(crate) fn certificate(
    options: &Options,
    root: &Path,
    cache_dir: &Path,
) -> anyhow::Result<oj_config::HttpsConfig> {
    let dir = match &options.cert_dir {
        Some(dir) if dir.is_absolute() => dir.clone(),
        Some(dir) => root.join(dir),
        None => cache_dir.join("basic-ssl"),
    };
    let path = dir.join(CERT_FILE);
    if let Some(kept) = kept(&path) {
        return Ok(kept);
    }
    let pem = generate(options)?;
    if let Err(e) = write_private(&dir, &path, &pem) {
        eprintln!(
            "oj: basic-ssl: the certificate could not be kept at {} ({e}); a fresh one is made on every start",
            path.display()
        );
    }
    Ok(pair(pem))
}

fn pair(pem: String) -> oj_config::HttpsConfig {
    oj_config::HttpsConfig {
        key: pem.clone(),
        cert: pem,
    }
}

/// The file's PEM when its certificate has a readable `notAfter` still ahead
/// (the plugin's `isCertificateExpired`) and rustls accepts the pair.
fn kept(path: &Path) -> Option<oj_config::HttpsConfig> {
    let pem = std::fs::read_to_string(path).ok()?;
    let not_after = x509_parser::pem::Pem::iter_from_buffer(pem.as_bytes())
        .filter_map(Result::ok)
        .find(|block| block.label == "CERTIFICATE")?
        .parse_x509()
        .ok()?
        .validity()
        .not_after
        .timestamp();
    if time::OffsetDateTime::now_utc().unix_timestamp() > not_after {
        return None;
    }
    let pair = pair(pem);
    crate::tls::accepts(&pair).then_some(pair)
}

/// A self-signed certificate shaped as the plugin's `createCertificate`
/// makes it: its subject, key usages and alternative names, the `domains`
/// appended, valid from now for `ttlDays`. Key then certificate, as the
/// plugin concatenates them.
fn generate(options: &Options) -> anyhow::Result<String> {
    use rcgen::{DnType, ExtendedKeyUsagePurpose, KeyUsagePurpose};
    let mut names = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "fe80::1".to_string(),
    ];
    for domain in &options.domains {
        if !names.contains(domain) {
            names.push(domain.clone());
        }
    }
    let mut params = rcgen::CertificateParams::new(names)?;
    let dn = &mut params.distinguished_name;
    dn.push(DnType::CommonName, options.name.as_str());
    dn.push(DnType::CountryName, "US");
    dn.push(DnType::StateOrProvinceName, "Virginia");
    dn.push(DnType::LocalityName, "Blacksburg");
    dn.push(DnType::OrganizationName, "Test");
    dn.push(DnType::OrganizationalUnitName, "Test");
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::ContentCommitment,
        KeyUsagePurpose::KeyEncipherment,
        KeyUsagePurpose::DataEncipherment,
    ];
    params.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ServerAuth,
        ExtendedKeyUsagePurpose::ClientAuth,
        ExtendedKeyUsagePurpose::CodeSigning,
        ExtendedKeyUsagePurpose::TimeStamping,
    ];
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now;
    params.not_after = now + time::Duration::days(options.ttl_days);
    let key = rcgen::KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    Ok(format!("{}{}", key.serialize_pem(), cert.pem()))
}

/// The file holds the private key, so it is the owner's alone.
fn write_private(dir: &Path, path: &Path, pem: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(path, pem)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names_of(pem: &str) -> (String, Vec<String>, i64) {
        let block = x509_parser::pem::Pem::iter_from_buffer(pem.as_bytes())
            .filter_map(Result::ok)
            .find(|b| b.label == "CERTIFICATE")
            .unwrap();
        let cert = block.parse_x509().unwrap();
        let cn = cert
            .subject()
            .iter_common_name()
            .next()
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();
        let sans = cert
            .subject_alternative_name()
            .unwrap()
            .unwrap()
            .value
            .general_names
            .iter()
            .map(|n| match n {
                x509_parser::extensions::GeneralName::DNSName(d) => d.to_string(),
                x509_parser::extensions::GeneralName::IPAddress(ip) => format!("ip:{}", ip.len()),
                other => format!("{other:?}"),
            })
            .collect();
        let days = (cert.validity().not_after.timestamp() - cert.validity().not_before.timestamp())
            / 86_400;
        (cn, sans, days)
    }

    #[test]
    fn options_take_the_plugins_defaults() {
        let o = Options::from_json(&serde_json::json!({}));
        assert_eq!(
            o,
            Options {
                name: "example.org".into(),
                domains: vec![],
                cert_dir: None,
                ttl_days: 30,
            }
        );
        let o = Options::from_json(&serde_json::json!({
            "name": "dev.local", "domains": ["dev.local", 3], "certDir": "certs", "ttlDays": 365.0
        }));
        assert_eq!(o.name, "dev.local");
        assert_eq!(o.domains, vec!["dev.local".to_string()]);
        assert_eq!(o.cert_dir.as_deref(), Some(Path::new("certs")));
        assert_eq!(o.ttl_days, 365);
        assert_eq!(
            Options::from_json(&serde_json::json!({ "ttlDays": 0, "name": 5 })).ttl_days,
            30
        );
    }

    #[test]
    fn the_certificate_is_made_once_under_the_cache_dir_as_the_plugin_shapes_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let cache = root.join("node_modules/.vite");
        let options = Options::from_json(&serde_json::json!({ "domains": ["dev.local"] }));
        let first = certificate(&options, root, &cache).unwrap();
        assert_eq!(first.key, first.cert, "one PEM carries key and certificate");
        assert!(first.key.starts_with("-----BEGIN PRIVATE KEY-----"));
        assert!(first.key.contains("-----BEGIN CERTIFICATE-----"));
        let file = cache.join("basic-ssl/_cert.pem");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), first.cert);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(crate::tls::accepts(&first));
        let (cn, sans, days) = names_of(&first.cert);
        assert_eq!(cn, "example.org");
        assert_eq!(sans, vec!["localhost", "ip:4", "ip:16", "dev.local"]);
        assert_eq!(days, 30);

        let again = certificate(&options, root, &cache).unwrap();
        assert_eq!(again.cert, first.cert, "the kept file is reused");
    }

    #[test]
    fn a_relative_cert_dir_is_under_the_root_and_an_absolute_one_is_itself() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let relative = Options::from_json(&serde_json::json!({ "certDir": "certs", "ttlDays": 2 }));
        certificate(&relative, root, &root.join("cache")).unwrap();
        assert!(root.join("certs/_cert.pem").is_file());
        let elsewhere = tempfile::tempdir().unwrap();
        let absolute = Options::from_json(
            &serde_json::json!({ "certDir": elsewhere.path().to_str().unwrap(), "name": "oj" }),
        );
        let made = certificate(&absolute, root, &root.join("cache")).unwrap();
        assert!(elsewhere.path().join("_cert.pem").is_file());
        assert_eq!(names_of(&made.cert).0, "oj");
    }

    #[test]
    fn an_expired_or_damaged_file_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let cache = root.join("cache");
        let file = cache.join("basic-ssl/_cert.pem");
        // The plugin's own check is on `notAfter` alone: a certificate past
        // it is made anew whatever its file's age.
        let mut expired = Options::from_json(&serde_json::json!({}));
        expired.ttl_days = 1;
        let mut params = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now - time::Duration::days(3);
        params.not_after = now - time::Duration::days(1);
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let old = format!("{}{}", key.serialize_pem(), cert.pem());
        std::fs::write(&file, &old).unwrap();
        let fresh = certificate(&expired, root, &cache).unwrap();
        assert_ne!(fresh.cert, old);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), fresh.cert);

        std::fs::write(
            &file,
            "-----BEGIN CERTIFICATE-----\nnope\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        let replaced = certificate(&expired, root, &cache).unwrap();
        assert!(crate::tls::accepts(&replaced));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), replaced.cert);

        // A certificate without its key does not load, so it is replaced too.
        let cert_only = fresh
            .cert
            .split_once("-----BEGIN CERTIFICATE-----")
            .map(|(_, rest)| format!("-----BEGIN CERTIFICATE-----{rest}"))
            .unwrap();
        std::fs::write(&file, &cert_only).unwrap();
        assert_ne!(certificate(&expired, root, &cache).unwrap().cert, cert_only);
    }
}
