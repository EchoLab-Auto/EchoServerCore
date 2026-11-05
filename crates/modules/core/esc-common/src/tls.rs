//! Mutual-TLS plumbing.
//!
//! Both peers verify each other against a single private CA, so a leaked spoke
//! key can be revoked by re-issuing the CA bundle. TLS 1.2 is left enabled
//! purely for interoperability with older OpenSSL when someone points a
//! hand-rolled client at the control port; TLS 1.3 is what we actually expect.

use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};

use crate::config::TlsConfig;
use crate::ALPN;

/// Install the process-wide rustls crypto provider.
///
/// `rustls` 0.23 panics at first use if no default provider is installed and
/// more than one is compiled in. Calling this early in `main` keeps that
/// failure mode out of the request path.
pub fn install_crypto_provider() {
    // Already-installed is fine (tests, or a second call).
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Load a PEM certificate bundle (leaf first, then any intermediates).
pub fn load_certs(path: &std::path::Path) -> Result<Vec<CertificateDer<'static>>> {
    let file = File::open(path)
        .with_context(|| format!("cannot open certificate {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let certs = rustls_pemfile::certs(&mut reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("cannot parse certificates in {}", path.display()))?;
    if certs.is_empty() {
        bail!("no certificate found in {}", path.display());
    }
    Ok(certs)
}

/// Load a PEM private key (PKCS#8, PKCS#1 or SEC1).
pub fn load_key(path: &std::path::Path) -> Result<PrivateKeyDer<'static>> {
    let file =
        File::open(path).with_context(|| format!("cannot open private key {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let key = rustls_pemfile::private_key(&mut reader)
        .with_context(|| format!("cannot parse private key {}", path.display()))?;
    match key {
        Some(k) => Ok(k),
        None => bail!("no private key found in {}", path.display()),
    }
}

fn load_roots(path: &std::path::Path) -> Result<Arc<RootCertStore>> {
    let mut roots = RootCertStore::empty();
    for cert in load_certs(path)? {
        roots
            .add(cert)
            .with_context(|| format!("cannot add CA certificate from {}", path.display()))?;
    }
    if roots.is_empty() {
        bail!("CA bundle {} contains no usable certificate", path.display());
    }
    Ok(Arc::new(roots))
}

/// Build the hub-side TLS configuration: **client certificates are required**
/// and verified against our CA.
pub fn build_server_config(tls: &TlsConfig) -> Result<Arc<ServerConfig>> {
    let certs = load_certs(&tls.cert)?;
    let key = load_key(&tls.key)?;
    let roots = load_roots(&tls.ca_cert)?;

    let verifier = rustls::server::WebPkiClientVerifier::builder(roots)
        .build()
        .context("cannot build client certificate verifier")?;

    let mut cfg = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, key)
        .context("cannot build server TLS config")?;
    cfg.alpn_protocols = vec![ALPN.to_vec()];
    Ok(Arc::new(cfg))
}

/// Build the spoke-side TLS configuration: the hub is verified against our CA
/// and we present our own client certificate.
pub fn build_client_config(tls: &TlsConfig) -> Result<Arc<ClientConfig>> {
    let certs = load_certs(&tls.cert)?;
    let key = load_key(&tls.key)?;
    let roots = load_roots(&tls.ca_cert)?;

    let mut cfg = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(certs, key)
        .context("cannot build client TLS config")?;
    cfg.alpn_protocols = vec![ALPN.to_vec()];
    Ok(Arc::new(cfg))
}

/// Turn a `host:port` into something rustls will accept as a server name,
/// including raw IP literals.
pub fn server_name(raw: &str) -> Result<rustls::pki_types::ServerName<'static>> {
    rustls::pki_types::ServerName::try_from(raw.to_string())
        .map_err(|e| anyhow::anyhow!("{raw:?} is not a valid TLS server name: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_cert_is_a_clear_error() {
        let err = load_certs(std::path::Path::new("/nonexistent/cert.pem")).unwrap_err();
        assert!(format!("{err:#}").contains("cannot open certificate"));
    }

    #[test]
    fn empty_bundle_is_rejected() {
        let dir = std::env::temp_dir().join(format!("esc-tls-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("empty.pem");
        std::fs::write(&p, b"# nothing here\n").unwrap();
        let err = load_certs(&p).unwrap_err();
        assert!(format!("{err:#}").contains("no certificate found"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ip_literals_are_valid_server_names() {
        assert!(server_name("127.0.0.1").is_ok());
        assert!(server_name("::1").is_ok());
        assert!(server_name("esc.example.com").is_ok());
    }
}
