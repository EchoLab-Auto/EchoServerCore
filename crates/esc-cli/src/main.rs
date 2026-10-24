//! `esc` — single binary for both ends of echo-server-core.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

use esc_common::config::{HubConfig, SpokeConfig};
use esc_common::pki::Ca;
use esc_common::{tls, ALPN};

#[derive(Parser, Debug)]
#[command(
    name = "esc",
    version,
    about = "echo-server-core: reverse-tunnel port forwarding over mutual TLS",
    long_about = None
)]
struct Cli {
    /// Log filter, e.g. `info`, `esc_hub=debug`, or `warn`.
    #[arg(long, env = "ESC_LOG", global = true, default_value = "info")]
    log: String,

    /// Log in JSON (handy when shipping to a collector).
    #[arg(long, global = true)]
    log_json: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run the public hub (accepts spokes, exposes service ports).
    Hub {
        #[arg(short, long, env = "ESC_CONFIG", default_value = "/etc/esc/hub.toml")]
        config: PathBuf,
    },
    /// Run the intranet spoke agent (dials out to the hub).
    Spoke {
        #[arg(short, long, env = "ESC_CONFIG", default_value = "/etc/esc/spoke.toml")]
        config: PathBuf,
    },
    /// Validate a config file and print what it would do. Binds nothing.
    Check {
        #[arg(short, long, env = "ESC_CONFIG")]
        config: PathBuf,
        #[arg(long, value_enum, default_value = "hub")]
        role: Role,
    },
    /// Certificate authority helpers.
    Pki {
        #[command(subcommand)]
        cmd: PkiCmd,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum Role {
    Hub,
    Spoke,
}

#[derive(Subcommand, Debug)]
enum PkiCmd {
    /// Create a CA and issue a hub certificate plus one spoke certificate.
    ///
    /// Everything is written to --out as <stem>.pem / <stem>.key.
    Init {
        #[arg(short, long, default_value = "/etc/esc")]
        out: PathBuf,
        /// Common name for the CA.
        #[arg(long, default_value = "echo-server-core CA")]
        ca_name: String,
        /// DNS name or IP the hub certificate must be valid for. Repeatable.
        #[arg(long = "host", required = true)]
        hosts: Vec<String>,
        /// Spoke id to issue a client certificate for. Repeatable.
        #[arg(long = "spoke", required = true)]
        spokes: Vec<String>,
        /// Certificate lifetime in days.
        #[arg(long, default_value_t = 3650)]
        ca_days: u32,
        #[arg(long, default_value_t = 3650)]
        cert_days: u32,
        /// Overwrite existing files.
        #[arg(long)]
        force: bool,
    },
    /// Issue one more spoke certificate from an existing CA.
    Spoke {
        #[arg(long, default_value = "/etc/esc")]
        ca_dir: PathBuf,
        #[arg(short, long)]
        id: String,
        /// Where to write the certificate. Defaults to --ca-dir.
        #[arg(short, long)]
        out: Option<PathBuf>,
        #[arg(long, default_value_t = 3650)]
        days: u32,
        #[arg(long)]
        force: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(&cli.log, cli.log_json);

    // rustls needs a process-wide crypto provider before any TLS object exists.
    tls::install_crypto_provider();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("cannot build the tokio runtime")?;

    rt.block_on(async move {
        match cli.cmd {
            Cmd::Hub { config } => {
                let cfg = HubConfig::load(&config).map_err(|e| anyhow::anyhow!("{e}"))?;
                tracing::info!(
                    version = env!("CARGO_PKG_VERSION"),
                    alpn = %String::from_utf8_lossy(ALPN),
                    "starting hub"
                );
                esc_hub::run(cfg).await
            }
            Cmd::Spoke { config } => {
                let cfg = SpokeConfig::load(&config).map_err(|e| anyhow::anyhow!("{e}"))?;
                tracing::info!(
                    version = env!("CARGO_PKG_VERSION"),
                    alpn = %String::from_utf8_lossy(ALPN),
                    "starting spoke"
                );
                esc_spoke::run(cfg).await
            }
            Cmd::Check { config, role } => check(&config, role),
            Cmd::Pki { cmd } => pki(cmd),
        }
    })
}

fn init_logging(filter: &str, json: bool) {
    use tracing_subscriber::EnvFilter;
    let env = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(env);
    if json {
        builder.json().init();
    } else {
        builder.init();
    }
}

/// Validate a config and describe its effect without touching the network.
fn check(path: &Path, role: Role) -> Result<()> {
    match role {
        Role::Hub => {
            let cfg = HubConfig::load(path).map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("OK  hub config: {}", path.display());
            println!("    control listen : {}", cfg.hub.listen);
            println!("    tls ca         : {}", cfg.tls.ca_cert.display());
            println!("    tls cert       : {}", cfg.tls.cert.display());
            println!("    reject absent  : {}", cfg.hub.reject_when_absent);
            println!("    services       : {}", cfg.services.len());
            for svc in &cfg.services {
                println!(
                    "      - {:<16} public {:<22} pinned_spoke={}",
                    svc.name,
                    svc.listen,
                    svc.spoke.as_deref().unwrap_or("-")
                );
            }
            // Prove the PEM material actually parses, which `validate()` cannot.
            tls::build_server_config(&cfg.tls)
                .context("TLS material does not load (the hub would fail at startup)")?;
            println!("    tls material   : loads OK (server, client-auth required)");
        }
        Role::Spoke => {
            let cfg = SpokeConfig::load(path).map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("OK  spoke config: {}", path.display());
            println!("    spoke id       : {}", cfg.spoke.id);
            println!("    server         : {}", cfg.spoke.server);
            println!("    tls server name: {}", cfg.tls_server_name());
            println!("    tls ca         : {}", cfg.tls.ca_cert.display());
            println!("    tls cert       : {}", cfg.tls.cert.display());
            println!("    backoff        : {}..{} ms", cfg.spoke.backoff_min_ms, cfg.spoke.backoff_max_ms);
            println!("    max streams    : {}", cfg.spoke.max_streams);
            println!("    services       : {}", cfg.services.len());
            for svc in &cfg.services {
                println!("      - {:<16} local {}", svc.name, svc.local);
            }
            tls::build_client_config(&cfg.tls)
                .context("TLS material does not load (the spoke would fail at startup)")?;
            println!("    tls material   : loads OK (client cert presented)");
        }
    }
    Ok(())
}

fn pki(cmd: PkiCmd) -> Result<()> {
    match cmd {
        PkiCmd::Init {
            out,
            ca_name,
            hosts,
            spokes,
            ca_days,
            cert_days,
            force,
        } => {
            guard(&out, &["ca.pem", "ca.key"], force)?;
            let ca = Ca::generate(&ca_name, ca_days)?;
            let ca_cert = esc_common::pki::write_ca_cert(&out, "ca", &ca)?;
            // The CA key is written by hand so we can keep it off the spoke hosts.
            let ca_key_path = out.join("ca.key");
            std::fs::write(&ca_key_path, ca.key_pem.as_bytes())
                .with_context(|| format!("cannot write {}", ca_key_path.display()))?;
            restrict(&ca_key_path)?;
            println!("CA       cert {}", ca_cert.display());
            println!("CA       key  {}  (keep this only on the machine that issues certs)", ca_key_path.display());

            let hub = ca.issue_server("esc-hub", &hosts, cert_days)?;
            let (hc, hk) = esc_common::pki::write_pair(&out, "hub", &hub)?;
            println!("hub      cert {}  (SANs: {})", hc.display(), hosts.join(", "));
            println!("hub      key  {}", hk.display());

            for id in &spokes {
                guard(&out, &[&format!("{id}.pem"), &format!("{id}.key")], force)?;
                let pair = ca.issue_client(id, cert_days)?;
                let (c, k) = esc_common::pki::write_pair(&out, id, &pair)?;
                println!("spoke    cert {}  (id: {id})", c.display());
                println!("spoke    key  {}", k.display());
            }
            println!();
            println!("Next: copy ca.pem + hub.{{pem,key}} to the hub, and ca.pem + <id>.{{pem,key}}");
            println!("      to each spoke. Never copy ca.key off this machine.");
        }
        PkiCmd::Spoke {
            ca_dir,
            id,
            out,
            days,
            force,
        } => {
            let out = out.unwrap_or_else(|| ca_dir.clone());
            guard(&out, &[&format!("{id}.pem"), &format!("{id}.key")], force)?;
            let ca_cert_pem = std::fs::read_to_string(ca_dir.join("ca.pem"))
                .with_context(|| format!("cannot read {}", ca_dir.join("ca.pem").display()))?;
            let ca_key_pem = std::fs::read_to_string(ca_dir.join("ca.key"))
                .with_context(|| format!("cannot read {}", ca_dir.join("ca.key").display()))?;
            let ca = Ca::from_pem(&ca_cert_pem, &ca_key_pem)?;
            let pair = ca.issue_client(&id, days)?;
            let (c, k) = esc_common::pki::write_pair(&out, &id, &pair)?;
            println!("spoke    cert {}  (id: {id})", c.display());
            println!("spoke    key  {}", k.display());
        }
    }
    Ok(())
}

fn guard(dir: &std::path::Path, names: &[&str], force: bool) -> Result<()> {
    for n in names {
        let p = dir.join(n);
        if p.exists() && !force {
            bail!(
                "{} already exists — refusing to overwrite (pass --force to replace)",
                p.display()
            );
        }
    }
    Ok(())
}

#[cfg(unix)]
fn restrict(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("cannot chmod {}", path.display()))
}

#[cfg(not(unix))]
fn restrict(_path: &std::path::Path) -> Result<()> {
    Ok(())
}
