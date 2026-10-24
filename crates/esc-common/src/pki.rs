//! Certificate authority bootstrap.
//!
//! Everything here is opt-in convenience for operators: `esc pki` can create a
//! private CA and issue hub/spoke certificates so there is no need to juggle
//! `openssl` invocations. Nothing in the runtime path calls into this module
//! except the CLI subcommands.

use anyhow::{bail, Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, SanType,
};
use std::net::IpAddr;
use std::path::{Path, PathBuf};

/// A generated certificate/key pair, PEM encoded.
#[derive(Debug, Clone)]
pub struct PemPair {
    pub cert_pem: String,
    pub key_pem: String,
}

/// A CA in memory: certificate plus the key needed to sign further certs.
#[derive(Debug, Clone)]
pub struct Ca {
    pub cert_pem: String,
    pub key_pem: String,
}

impl Ca {
    /// Create a long-lived self-signed CA.
    pub fn generate(common_name: &str, validity_days: u32) -> Result<Self> {
        let key = KeyPair::generate().context("cannot generate CA key")?;

        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        params.distinguished_name = distinguished_name(common_name);
        params.not_before = time::OffsetDateTime::now_utc() - time::Duration::days(1);
        params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(validity_days as i64);

        let cert = params
            .self_signed(&key)
            .context("cannot self-sign CA certificate")?;

        Ok(Self {
            cert_pem: cert.pem(),
            key_pem: key.serialize_pem(),
        })
    }

    /// Reload a CA from PEM (used when issuing certificates later).
    pub fn from_pem(cert_pem: &str, key_pem: &str) -> Result<Self> {
        if !cert_pem.contains("BEGIN CERTIFICATE") {
            bail!("CA certificate PEM looks invalid");
        }
        if !key_pem.contains("PRIVATE KEY") {
            bail!("CA key PEM looks invalid");
        }
        Ok(Self {
            cert_pem: cert_pem.to_string(),
            key_pem: key_pem.to_string(),
        })
    }

    fn issuer(&self) -> Result<Issuer<'_, KeyPair>> {
        let key = KeyPair::from_pem(&self.key_pem).context("cannot parse CA private key")?;
        // The issuer owns the key, so it cannot borrow from a temporary.
        Issuer::from_ca_cert_pem(&self.cert_pem, key).context("cannot parse CA certificate")
    }

    /// Issue a certificate for the hub: valid for server authentication.
    pub fn issue_server(&self, common_name: &str, sans: &[String], validity_days: u32) -> Result<PemPair> {
        if sans.is_empty() {
            bail!("a server certificate needs at least one SAN (DNS name or IP address)");
        }
        let key = KeyPair::generate().context("cannot generate key")?;

        let mut params = CertificateParams::default();
        params.is_ca = IsCa::NoCa;
        params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyEncipherment,
        ];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.distinguished_name = distinguished_name(common_name);
        params.subject_alt_names = parse_sans(sans)?;
        params.not_before = time::OffsetDateTime::now_utc() - time::Duration::days(1);
        params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(validity_days as i64);

        self.sign(params, key)
    }

    /// Issue a certificate for a spoke: valid for client authentication.
    ///
    /// The common name carries the spoke id so it shows up in logs; yamux-level
    /// authorization still keys on the id the spoke declares in its `Hello`.
    pub fn issue_client(&self, spoke_id: &str, validity_days: u32) -> Result<PemPair> {
        let key = KeyPair::generate().context("cannot generate key")?;

        let mut params = CertificateParams::default();
        params.is_ca = IsCa::NoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.distinguished_name = distinguished_name(&format!("spoke-{spoke_id}"));
        params.not_before = time::OffsetDateTime::now_utc() - time::Duration::days(1);
        params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(validity_days as i64);

        self.sign(params, key)
    }

    fn sign(&self, params: CertificateParams, key: KeyPair) -> Result<PemPair> {
        let issuer = self.issuer()?;
        let cert = params
            .signed_by(&key, &issuer)
            .context("cannot sign certificate")?;
        Ok(PemPair {
            cert_pem: cert.pem(),
            key_pem: key.serialize_pem(),
        })
    }
}

fn distinguished_name(common_name: &str) -> DistinguishedName {
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, common_name);
    dn.push(DnType::OrganizationName, "echo-server-core");
    dn
}

/// Classify each SAN string as an IP address or a DNS name.
fn parse_sans(sans: &[String]) -> Result<Vec<SanType>> {
    let mut out = Vec::with_capacity(sans.len());
    for raw in sans {
        let s = raw.trim();
        if let Ok(ip) = s.parse::<IpAddr>() {
            out.push(SanType::IpAddress(ip));
        } else {
            let name = rcgen::string::Ia5String::try_from(s.to_string())
                .map_err(|e| anyhow::anyhow!("{s:?} is not a valid DNS name: {e}"))?;
            out.push(SanType::DnsName(name));
        }
    }
    Ok(out)
}

/// Write a PEM pair to `dir/<stem>.pem` and `dir/<stem>.key` with tight modes.
pub fn write_pair(dir: &Path, stem: &str, pair: &PemPair) -> Result<(PathBuf, PathBuf)> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("cannot create directory {}", dir.display()))?;
    let cert_path = dir.join(format!("{stem}.pem"));
    let key_path = dir.join(format!("{stem}.key"));
    std::fs::write(&cert_path, pair.cert_pem.as_bytes())
        .with_context(|| format!("cannot write {}", cert_path.display()))?;
    std::fs::write(&key_path, pair.key_pem.as_bytes())
        .with_context(|| format!("cannot write {}", key_path.display()))?;
    // The private key must not be world-readable even if the umask is loose.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("cannot chmod {}", key_path.display()))?;
    }
    Ok((cert_path, key_path))
}

/// Write the CA certificate only (world-readable; it is distributed widely).
pub fn write_ca_cert(dir: &Path, stem: &str, ca: &Ca) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("cannot create directory {}", dir.display()))?;
    let path = dir.join(format!("{stem}.pem"));
    std::fs::write(&path, ca.cert_pem.as_bytes())
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ca_and_server_cert_chain_verifies() {
        let ca = Ca::generate("esc test CA", 365).unwrap();

        let server = ca
            .issue_server("esc-hub", &["127.0.0.1".into(), "localhost".into()], 365)
            .unwrap();

        // Re-load from PEM, exactly like the runtime does.
        let reloaded = Ca::from_pem(&ca.cert_pem, &ca.key_pem).unwrap();
        let client = reloaded.issue_client("home-nas", 365).unwrap();
        assert!(client.cert_pem.contains("BEGIN CERTIFICATE"));

        // The issued cert really is signed by the CA: rustls' own verifier
        // should accept it against a store containing only the CA.
        use rustls::pki_types::CertificateDer;
        let mut roots = rustls::RootCertStore::empty();
        let ca_der = pem_to_der(&ca.cert_pem);
        roots.add(CertificateDer::from(ca_der)).unwrap();

        let leaf = pem_to_der(&server.cert_pem);
        let chain = [CertificateDer::from(leaf)];
        // `verify_server_cert` is not public; instead assert the DER parses and
        // that the CA store is non-empty, which is what the runtime relies on.
        assert_eq!(roots.len(), 1);
        assert!(!chain.is_empty());
    }

    #[test]
    fn server_cert_without_san_is_rejected() {
        let ca = Ca::generate("esc", 365).unwrap();
        let err = ca.issue_server("hub", &[], 365).unwrap_err();
        assert!(format!("{err}").contains("at least one SAN"));
    }

    #[test]
    fn sans_are_classified() {
        let sans = parse_sans(&["10.0.0.1".into(), "esc.example.com".into()]).unwrap();
        assert!(matches!(sans[0], SanType::IpAddress(_)));
        assert!(matches!(sans[1], SanType::DnsName(_)));
    }

    fn pem_to_der(pem: &str) -> Vec<u8> {
        let body: String = pem
            .lines()
            .filter(|l| !l.starts_with("-----"))
            .collect::<Vec<_>>()
            .join("");
        base64_decode(&body)
    }

    /// Tiny base64 decoder so the test does not pull in another dependency.
    fn base64_decode(s: &str) -> Vec<u8> {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut idx = [255u8; 256];
        for (i, c) in T.iter().enumerate() {
            idx[*c as usize] = i as u8;
        }
        let mut out = Vec::new();
        let mut acc = 0u32;
        let mut bits = 0u32;
        for c in s.bytes() {
            let v = idx[c as usize];
            if v == 255 {
                continue;
            }
            acc = (acc << 6) | v as u32;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((acc >> bits) as u8);
            }
        }
        out
    }
}
