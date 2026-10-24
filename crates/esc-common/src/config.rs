//! Configuration files.
//!
//! Two shapes exist — [`HubConfig`] for the public server and [`SpokeConfig`]
//! for the intranet agent — but both share the same [`TlsConfig`] block.

use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use crate::DEFAULT_CONTROL_PORT;

/// Paths to the PEM material used for mutual TLS.
///
/// Both sides authenticate each other against the same private CA, so the same
/// `ca_cert` file is distributed everywhere while `cert`/`key` stay per-host.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct TlsConfig {
    /// PEM bundle of the CA that signed both the hub and every spoke cert.
    pub ca_cert: PathBuf,
    /// PEM certificate chain presented to the peer (leaf first).
    pub cert: PathBuf,
    /// PEM private key matching `cert` (PKCS#8, PKCS#1 or SEC1).
    pub key: PathBuf,
}

impl TlsConfig {
    /// Reject obviously-relative paths early so operators get a clear error
    /// instead of a confusing "no such file" after a `cd`.
    pub fn validate(&self) -> Result<(), String> {
        for (label, p) in [
            ("ca_cert", &self.ca_cert),
            ("cert", &self.cert),
            ("key", &self.key),
        ] {
            if !p.is_absolute() {
                return Err(format!("tls.{label} must be an absolute path, got {p:?}"));
            }
            if !p.exists() {
                return Err(format!("tls.{label} does not exist: {}", p.display()));
            }
        }
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Hub
// ─────────────────────────────────────────────────────────────────────────────

/// Configuration for the public-facing hub.
///
/// A hub owns **both** kinds of listeners: one control port for spokes to dial
/// in on, and one public port per exposed service.
///
/// ```toml
/// [hub]
/// listen = "0.0.0.0:4433"
///
/// [tls]
/// ca_cert = "/etc/esc/ca.pem"
/// cert    = "/etc/esc/hub.pem"
/// key     = "/etc/esc/hub.key"
///
/// [[service]]
/// name   = "grafana"          # must match a name the spoke declares
/// listen = "0.0.0.0:8080"
/// # spoke = "home-nas"       # optional: pin this service to one spoke
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HubConfig {
    #[serde(default)]
    pub hub: HubSection,
    pub tls: TlsConfig,
    #[serde(default, rename = "service")]
    pub services: Vec<HubService>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HubSection {
    /// Address spokes connect to. Set to `0.0.0.0:4433` behind a NAT.
    #[serde(default = "default_control_listen")]
    pub listen: String,
    /// How long a spoke may go without a control frame before we drop it.
    ///
    /// yamux itself does not time out; this is enforced by the hub with a
    /// period of quiet on the control stream.
    #[serde(default = "default_heartbeat")]
    pub heartbeat_secs: u64,
    /// Refuse a public connection when no spoke currently offers the service,
    /// instead of leaving the peer hanging. Default `true`.
    #[serde(default = "default_true")]
    pub reject_when_absent: bool,
}

impl Default for HubSection {
    fn default() -> Self {
        Self {
            listen: default_control_listen(),
            heartbeat_secs: default_heartbeat(),
            reject_when_absent: true,
        }
    }
}

/// One publicly-exposed port on the hub.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HubService {
    /// Logical service name. Matched against the spoke's declared services.
    pub name: String,
    /// Public `host:port` to accept connections on.
    pub listen: String,
    /// When set, only a spoke with this id may serve the port.
    #[serde(default)]
    pub spoke: Option<String>,
}

impl HubService {
    pub fn listen_addr(&self) -> Result<SocketAddr, String> {
        self.listen
            .parse()
            .map_err(|e| format!("service {:?}: invalid listen {:?}: {e}", self.name, self.listen))
    }
}

impl HubConfig {
    pub fn control_addr(&self) -> Result<SocketAddr, String> {
        self.hub
            .listen
            .parse()
            .map_err(|e| format!("hub.listen {:?} is not a socket address: {e}", self.hub.listen))
    }

    /// Load and sanity-check a hub configuration file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let cfg: Self = toml::from_str(&raw)
            .map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), String> {
        self.tls.validate()?;
        self.control_addr()?;

        if self.services.is_empty() {
            return Err("no [[service]] defined — a hub with nothing to expose is a typo".into());
        }

        let mut seen_name = std::collections::HashSet::new();
        let mut seen_addr = std::collections::HashSet::new();
        for svc in &self.services {
            if svc.name.trim().is_empty() {
                return Err("service.name must not be empty".into());
            }
            if !seen_name.insert(svc.name.clone()) {
                return Err(format!("duplicate service name {:?}", svc.name));
            }
            let addr = svc.listen_addr()?;
            if !seen_addr.insert(addr) {
                return Err(format!("duplicate public listen address {addr}"));
            }
            if addr == self.control_addr()? {
                return Err(format!(
                    "service {:?} listens on the control address {addr}",
                    svc.name
                ));
            }
        }
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Spoke
// ─────────────────────────────────────────────────────────────────────────────

/// Configuration for an intranet agent.
///
/// ```toml
/// [spoke]
/// id     = "home-nas"
/// server = "esc.example.com:4433"
///
/// [tls]
/// ca_cert = "/etc/esc/ca.pem"
/// cert    = "/etc/esc/home-nas.pem"
/// key     = "/etc/esc/home-nas.key"
///
/// [[service]]
/// name  = "grafana"
/// local = "127.0.0.1:3000"
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SpokeConfig {
    #[serde(default)]
    pub spoke: SpokeSection,
    pub tls: TlsConfig,
    #[serde(default, rename = "service")]
    pub services: Vec<SpokeService>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SpokeSection {
    /// Stable identifier. The hub uses it for authorization and logging.
    #[serde(default = "default_spoke_id")]
    pub id: String,
    /// `host:port` of the hub control listener.
    pub server: String,
    /// Reconnect backoff floor.
    #[serde(default = "default_backoff_min")]
    pub backoff_min_ms: u64,
    /// Reconnect backoff ceiling.
    #[serde(default = "default_backoff_max")]
    pub backoff_max_ms: u64,
    /// Optional hostname override for certificate verification. `None` means
    /// "use the host part of `server`".
    #[serde(default)]
    pub server_name: Option<String>,
    /// Maximim concurrent forwarded streams, as a local safety valve.
    #[serde(default = "default_max_streams")]
    pub max_streams: usize,
}

impl Default for SpokeSection {
    fn default() -> Self {
        Self {
            id: default_spoke_id(),
            server: String::new(),
            backoff_min_ms: default_backoff_min(),
            backoff_max_ms: default_backoff_max(),
            server_name: None,
            max_streams: default_max_streams(),
        }
    }
}

/// One intranet service this spoke is able to serve.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SpokeService {
    /// Logical name; must match a `[[service]]` block on the hub.
    pub name: String,
    /// Local `host:port` that receives the forwarded bytes.
    pub local: String,
}

impl SpokeConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let cfg: Self = toml::from_str(&raw)
            .map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), String> {
        self.tls.validate()?;

        if self.spoke.server.trim().is_empty() {
            return Err("spoke.server must be set".into());
        }
        if self.spoke.id.trim().is_empty() {
            return Err("spoke.id must not be empty".into());
        }
        if self.spoke.backoff_min_ms == 0 {
            return Err("spoke.backoff_min_ms must be > 0".into());
        }
        if self.spoke.backoff_max_ms < self.spoke.backoff_min_ms {
            return Err("spoke.backoff_max_ms must be >= backoff_min_ms".into());
        }
        if self.services.is_empty() {
            return Err("no [[service]] defined — a spoke offering nothing is a typo".into());
        }

        let mut seen = std::collections::HashSet::new();
        for svc in &self.services {
            if svc.name.trim().is_empty() {
                return Err("service.name must not be empty".into());
            }
            if !seen.insert(svc.name.clone()) {
                return Err(format!("duplicate service name {:?}", svc.name));
            }
            if svc.local.trim().is_empty() {
                return Err(format!("service {:?}: local must be set", svc.name));
            }
        }
        Ok(())
    }

    /// Hostname used for TLS verification of the hub certificate.
    pub fn tls_server_name(&self) -> String {
        if let Some(n) = &self.spoke.server_name {
            return n.clone();
        }
        crate::protocol::split_host_port(&self.spoke.server)
            .map(|(h, _)| h)
            .unwrap_or_else(|| self.spoke.server.clone())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// defaults
// ─────────────────────────────────────────────────────────────────────────────

fn default_control_listen() -> String {
    format!("0.0.0.0:{DEFAULT_CONTROL_PORT}")
}
fn default_heartbeat() -> u64 {
    30
}
fn default_true() -> bool {
    true
}
fn default_spoke_id() -> String {
    // Deliberately not auto-derived from the hostname: a stable, explicit id
    // is what authorization keys on.
    "spoke".to_string()
}
fn default_backoff_min() -> u64 {
    500
}
fn default_backoff_max() -> u64 {
    30_000
}
fn default_max_streams() -> usize {
    512
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tls() -> TlsConfig {
        TlsConfig {
            ca_cert: PathBuf::from("/dev/null"),
            cert: PathBuf::from("/dev/null"),
            key: PathBuf::from("/dev/null"),
        }
    }

    #[test]
    fn hub_rejects_duplicate_service_names() {
        let cfg = HubConfig {
            hub: HubSection::default(),
            tls: tls(),
            services: vec![
                HubService { name: "a".into(), listen: "0.0.0.0:1".into(), spoke: None },
                HubService { name: "a".into(), listen: "0.0.0.0:2".into(), spoke: None },
            ],
        };
        // /dev/null exists and is absolute, so only the name clash can fail.
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("duplicate service name"), "{err}");
    }

    #[test]
    fn hub_rejects_service_on_control_port() {
        let cfg = HubConfig {
            hub: HubSection::default(),
            tls: tls(),
            services: vec![HubService {
                name: "a".into(),
                listen: format!("0.0.0.0:{DEFAULT_CONTROL_PORT}"),
                spoke: None,
            }],
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("control address"), "{err}");
    }

    #[test]
    fn spoke_derives_server_name_from_server() {
        let mut cfg = SpokeConfig {
            spoke: SpokeSection { server: "esc.example.com:4433".into(), ..Default::default() },
            tls: tls(),
            services: vec![SpokeService { name: "a".into(), local: "127.0.0.1:1".into() }],
        };
        assert_eq!(cfg.tls_server_name(), "esc.example.com");
        cfg.spoke.server_name = Some("override.example".into());
        assert_eq!(cfg.tls_server_name(), "override.example");
    }
}
