//! Hub: the public-facing side of echo-server-core.
//!
//! Responsibilities:
//!
//! 1. Accept mutual-TLS connections from spokes on the control port.
//! 2. Validate each spoke's `Hello` against the hub's own service list.
//! 3. Bind one public TCP listener per exposed service and forward accepted
//!    connections over the spoke's muxed connection.
//!
//! Note the asymmetry that makes this safe to put on the internet: the spoke
//! **never** chooses what gets exposed. It can only offer names; the hub decides
//! which of them get a public listener.

pub mod registry;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_util::compat::{FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use tracing::{debug, error, info, warn};
use yamux::Mode;

use esc_common::config::{HubConfig, HubService};
use esc_common::mux::{default_config, Mux};
use esc_common::protocol::{self, Ack, DeniedService, Hello, StreamHeader, PROTO_VERSION};
use esc_common::tls;
use registry::{Entry, Registry};

/// Seconds a fresh spoke has to send its `Hello` before we hang up.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// How long a public connection waits for a provider when
/// `hub.reject_when_absent = false`.
const GRACE_PERIOD: Duration = Duration::from_secs(5);

/// Run the hub until the process receives a shutdown signal.
pub async fn run(cfg: HubConfig) -> Result<()> {
    let registry = Registry::new();
    let acceptor = TlsAcceptor::from(tls::build_server_config(&cfg.tls)?);

    let control_addr = cfg
        .control_addr()
        .map_err(|e| anyhow::anyhow!("invalid hub.listen: {e}"))?;

    // Bind every public listener *before* accepting spokes, so a port clash is a
    // startup failure rather than a service that silently never appears.
    let mut public = Vec::new();
    for svc in &cfg.services {
        let addr = svc
            .listen_addr()
            .map_err(|e| anyhow::anyhow!("invalid service listen: {e}"))?;
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("cannot bind public listener for service {:?} on {addr}", svc.name))?;
        info!(
            service = %svc.name,
            addr = %addr,
            pinned_spoke = svc.spoke.as_deref().unwrap_or("-"),
            "public listener bound"
        );
        public.push((svc.clone(), listener));
    }

    let control = TcpListener::bind(control_addr)
        .await
        .with_context(|| format!("cannot bind control listener on {control_addr}"))?;
    info!(addr = %control_addr, "control listener bound, waiting for spokes");

    // One task per public service.
    let mut handles = Vec::new();
    for (svc, listener) in public {
        let reg = registry.clone();
        let reject_when_absent = cfg.hub.reject_when_absent;
        handles.push(tokio::spawn(async move {
            if let Err(e) = serve_public(svc, listener, reg, reject_when_absent).await {
                error!(error = %e, "public listener stopped");
            }
        }));
    }

    // Control-plane accept loop, interruptible.
    let respawn = Arc::new(AtomicU64::new(0));
    let accept_loop = {
        let registry = registry.clone();
        let acceptor = acceptor.clone();
        let heartbeat = cfg.hub.heartbeat_secs;
        let allowed: Arc<Vec<String>> = Arc::new(cfg.services.iter().map(|s| s.name.clone()).collect());
        let pinned: Arc<std::collections::HashMap<String, String>> = Arc::new(
            cfg.services
                .iter()
                .filter_map(|s| s.spoke.clone().map(|sp| (s.name.clone(), sp)))
                .collect(),
        );
        let respawn = respawn.clone();
        async move {
            loop {
                let (sock, peer) = match control.accept().await {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(error = %e, "control accept failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                let n = respawn.fetch_add(1, Ordering::Relaxed);
                let conn_key = format!("c{n}-{peer}");
                let acceptor = acceptor.clone();
                let registry = registry.clone();
                let allowed = allowed.clone();
                let pinned = pinned.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_spoke(
                        sock, peer, acceptor, registry, allowed, pinned, heartbeat, conn_key,
                    )
                    .await
                    {
                        debug!(%peer, error = %e, "spoke connection ended");
                    }
                });
            }
        }
    };

    tokio::select! {
        _ = accept_loop => {},
        _ = shutdown_signal() => info!("shutdown signal received"),
    }

    info!("hub stopping");
    for h in handles {
        h.abort();
    }
    Ok(())
}

/// Accept loop for one publicly-exposed port.
async fn serve_public(
    svc: HubService,
    listener: TcpListener,
    registry: Registry,
    reject_when_absent: bool,
) -> Result<()> {
    loop {
        let (sock, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!(service = %svc.name, error = %e, "public accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };

        let entry = registry.pick(&svc.name, svc.spoke.as_deref()).await;
        let entry = match entry {
            Some(e) => e,
            None if !reject_when_absent => {
                // Operator opted into "grace period": a spoke may be
                // reconnecting right now, so give it a moment before refusing.
                match wait_for_provider(&registry, &svc, peer).await {
                    Some(e) => e,
                    None => {
                        warn!(
                            service = %svc.name,
                            %peer,
                            "no spoke appeared within the grace period, closing"
                        );
                        drop(sock);
                        continue;
                    }
                }
            }
            None => {
                warn!(
                    service = %svc.name,
                    %peer,
                    "no spoke currently serves this service, closing"
                );
                drop(sock);
                continue;
            }
        };

        let svc_name = svc.name.clone();
        tokio::spawn(async move {
            match forward(sock, entry, svc_name.clone()).await {
                Ok((up, down)) => {
                    debug!(service = %svc_name, %peer, up, down, "connection finished");
                }
                Err(e) => {
                    debug!(service = %svc_name, %peer, error = %e, "connection failed");
                }
            }
        });
    }
}

/// Poll for a provider for up to [`GRACE_PERIOD`], used when
/// `hub.reject_when_absent = false` so a briefly-reconnecting spoke does not
/// turn into a user-visible failure.
async fn wait_for_provider(registry: &Registry, svc: &HubService, peer: SocketAddr) -> Option<Entry> {
    let deadline = tokio::time::Instant::now() + GRACE_PERIOD;
    loop {
        if let Some(e) = registry.pick(&svc.name, svc.spoke.as_deref()).await {
            debug!(service = %svc.name, %peer, "provider appeared during grace period");
            return Some(e);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Bridge one accepted public socket to a new mux stream towards the spoke.
async fn forward(mut sock: TcpStream, entry: Entry, service: String) -> Result<(u64, u64)> {
    // Small writes travel better without Nagle when we are proxying.
    let _ = sock.set_nodelay(true);

    let stream = entry
        .mux
        .open()
        .await
        .map_err(|e| anyhow::anyhow!("cannot open stream to spoke {}: {e}", entry.spoke_id))?;
    let mut stream = stream.compat();

    protocol::write_msg(&mut stream, &StreamHeader { service: service.clone() })
        .await
        .context("cannot send stream header")?;

    let (up, down) = tokio::io::copy_bidirectional(&mut sock, &mut stream)
        .await
        .context("copy failed")?;
    Ok((up, down))
}

/// Handle one authenticated spoke connection for its whole lifetime.
#[allow(clippy::too_many_arguments)]
async fn handle_spoke(
    sock: TcpStream,
    peer: SocketAddr,
    acceptor: TlsAcceptor,
    registry: Registry,
    allowed: Arc<Vec<String>>,
    pinned: Arc<std::collections::HashMap<String, String>>,
    heartbeat_secs: u64,
    conn_key: String,
) -> Result<()> {
    let _ = sock.set_nodelay(true);

    let tls_stream = tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(sock))
        .await
        .map_err(|_| anyhow::anyhow!("TLS handshake timed out"))?
        .context("TLS handshake failed")?;

    // mTLS already guarantees a client certificate is present; we only note its
    // size for logs rather than parsing the DN (that would pull in an X.509
    // parser for a purely cosmetic field). Authorization keys on `Hello.spoke_id`.
    let cert_bytes = tls_stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certs| certs.first().map(|c| c.as_ref().len()))
        .unwrap_or(0);

    let (mux, mut inbound) = Mux::spawn(
        tls_stream.compat(),
        Mode::Server,
        default_config(512),
        format!("spoke@{peer}"),
    );

    // The first stream a spoke opens is the control stream.
    let ctrl = tokio::time::timeout(HANDSHAKE_TIMEOUT, inbound.recv())
        .await
        .map_err(|_| anyhow::anyhow!("spoke opened no control stream"))?
        .ok_or_else(|| anyhow::anyhow!("connection closed before control stream"))?;
    let mut ctrl = ctrl.compat();

    let hello: Hello = tokio::time::timeout(HANDSHAKE_TIMEOUT, protocol::read_msg(&mut ctrl))
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for Hello"))?
        .context("cannot read Hello")?;

    if hello.version != PROTO_VERSION {
        let msg = format!(
            "protocol version mismatch: spoke sent {}, hub speaks {}",
            hello.version, PROTO_VERSION
        );
        warn!(%peer, spoke = %hello.spoke_id, "{msg}");
        // Best effort: tell the peer why, then drop.
        let _ = protocol::write_msg(
            &mut ctrl,
            &Ack {
                version: PROTO_VERSION,
                granted: vec![],
                denied: vec![DeniedService { name: "*".into(), reason: msg.clone() }],
            },
        )
        .await;
        anyhow::bail!(msg);
    }

    // Decide what this spoke is allowed to serve.
    let mut granted = Vec::new();
    let mut denied = Vec::new();
    for decl in &hello.services {
        if !allowed.iter().any(|a| a == &decl.name) {
            denied.push(DeniedService {
                name: decl.name.clone(),
                reason: "not exposed by this hub".into(),
            });
            continue;
        }
        if let Some(want) = pinned.get(&decl.name) {
            if want != &hello.spoke_id {
                denied.push(DeniedService {
                    name: decl.name.clone(),
                    reason: format!("hub pins this service to spoke {want:?}"),
                });
                continue;
            }
        }
        granted.push(decl.name.clone());
    }

    protocol::write_msg(
        &mut ctrl,
        &Ack {
            version: PROTO_VERSION,
            granted: granted.clone(),
            denied: denied.clone(),
        },
    )
    .await
    .context("cannot send Ack")?;

    for d in &denied {
        warn!(%peer, spoke = %hello.spoke_id, service = %d.name, reason = %d.reason, "service denied");
    }

    if granted.is_empty() {
        warn!(
            %peer,
            spoke = %hello.spoke_id,
            declared = hello.services.len(),
            "spoke offers nothing this hub exposes, dropping"
        );
        anyhow::bail!("spoke granted no services");
    }

    info!(
        %peer,
        spoke = %hello.spoke_id,
        agent = %hello.agent,
        client_cert_bytes = cert_bytes,
        granted = ?granted,
        "spoke connected"
    );

    registry
        .register(
            Entry {
                conn_key: conn_key.clone(),
                spoke_id: hello.spoke_id.clone(),
                mux: mux.clone(),
            },
            &granted,
        )
        .await;

    // Keep the control stream alive for the connection's lifetime. Any frame
    // here is unexpected, so we simply drain; EOF means the spoke is gone.
    let mut sink = [0u8; 256];
    let mut idle = tokio::time::interval(Duration::from_secs(heartbeat_secs.max(5)));
    idle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    idle.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            r = ctrl.read(&mut sink) => match r {
                Ok(0) => {
                    debug!(spoke = %hello.spoke_id, "control stream closed by peer");
                    break;
                }
                Ok(n) => {
                    debug!(spoke = %hello.spoke_id, bytes = n, "unexpected control frame (ignored)");
                }
                Err(e) => {
                    debug!(spoke = %hello.spoke_id, error = %e, "control stream read error");
                    break;
                }
            },
            _ = idle.tick() => {
                // yamux keeps the transport warm via its own keepalive frames;
                // this tick only refreshes liveness bookkeeping in the registry.
                if registry.providers(&granted[0]).await == 0 {
                    debug!(spoke = %hello.spoke_id, "spoke vanished from registry");
                    break;
                }
            }
        }
    }

    registry.unregister(&conn_key).await;
    info!(spoke = %hello.spoke_id, "spoke disconnected");
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => return std::future::pending().await,
        };
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(_) => return std::future::pending().await,
        };
        tokio::select! {
            _ = sigterm.recv() => {},
            _ = sigint.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
