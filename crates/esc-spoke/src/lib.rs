//! Spoke: the intranet agent.
//!
//! A spoke dials *out* to the hub over mutual TLS, so no inbound port needs to
//! be opened on the intranet side. All forwarded traffic travels inside that
//! single connection, multiplexed by yamux.
//!
//! The spoke only ever *offers* service names. Whether any of them is actually
//! reachable from the internet is decided by the hub's configuration.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_util::compat::{FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use tracing::{debug, info, warn};
use yamux::Mode;

use esc_common::config::SpokeConfig;
use esc_common::mux::{default_config, Mux};
use esc_common::protocol::{self, Ack, Hello, ServiceDecl, StreamHeader, PROTO_VERSION};
use esc_common::tls;

/// How often to send a keepalive comment frame on the control stream. yamux has
/// its own keepalive, but a dead TLS middlebox is caught faster this way.
const CONTROL_PING: Duration = Duration::from_secs(20);

/// Run the spoke forever, reconnecting with exponential backoff.
pub async fn run(cfg: SpokeConfig) -> Result<()> {
    let tls = tls::build_client_config(&cfg.tls)?;
    let connector = TlsConnector::from(tls);

    let services: Arc<HashMap<String, String>> = Arc::new(
        cfg.services
            .iter()
            .map(|s| (s.name.clone(), s.local.clone()))
            .collect(),
    );

    let mut backoff_ms = cfg.spoke.backoff_min_ms;
    let max_backoff = cfg.spoke.backoff_max_ms;
    let mut attempt: u64 = 0;

    loop {
        attempt += 1;
        info!(
            attempt,
            server = %cfg.spoke.server,
            spoke = %cfg.spoke.id,
            "connecting to hub"
        );

        match connect_once(&cfg, &connector, services.clone()).await {
            Ok(()) => {
                // A clean end means the hub closed us (restart, config reload).
                // Reconnect immediately-ish rather than ramping backoff.
                info!("connection closed by hub, reconnecting");
                backoff_ms = cfg.spoke.backoff_min_ms;
            }
            Err(e) => {
                warn!(error = %e, "connection failed");
            }
        }

        // Jitter avoids a thundering herd when a hub restarts with many spokes.
        let jitter = backoff_ms / 5;
        let sleep_ms = if jitter > 0 {
            backoff_ms - jitter + (rand_u64() % (2 * jitter + 1))
        } else {
            backoff_ms
        };
        debug!(sleep_ms, "backing off");
        tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
        backoff_ms = (backoff_ms.saturating_mul(2)).min(max_backoff);
    }
}

async fn connect_once(
    cfg: &SpokeConfig,
    connector: &TlsConnector,
    services: Arc<HashMap<String, String>>,
) -> Result<()> {
    let tcp = TcpStream::connect(&cfg.spoke.server)
        .await
        .with_context(|| format!("cannot connect to {}", cfg.spoke.server))?;
    let _ = tcp.set_nodelay(true);

    let server_name = tls::server_name(&cfg.tls_server_name())?;
    let stream = connector
        .connect(server_name, tcp)
        .await
        .context("TLS handshake with hub failed")?;

    let (mux, mut inbound) = Mux::spawn(
        stream.compat(),
        Mode::Client,
        default_config(cfg.spoke.max_streams),
        format!("hub:{}", cfg.spoke.server),
    );

    // The control stream is the first one we open, and it must stay alive for
    // the connection's lifetime: the hub treats its EOF as "spoke is gone".
    let ctrl = mux
        .open()
        .await
        .map_err(|e| anyhow::anyhow!("cannot open control stream: {e}"))?;
    let mut ctrl = ctrl.compat();

    let hello = Hello {
        version: PROTO_VERSION,
        spoke_id: cfg.spoke.id.clone(),
        services: cfg
            .services
            .iter()
            .map(|s| ServiceDecl {
                name: s.name.clone(),
                local: s.local.clone(),
            })
            .collect(),
        agent: format!("esc/{}", env!("CARGO_PKG_VERSION")),
    };
    protocol::write_msg(&mut ctrl, &hello)
        .await
        .context("cannot send Hello")?;

    let ack: Ack = protocol::read_msg(&mut ctrl)
        .await
        .context("cannot read Ack (is the hub speaking a compatible version?)")?;

    if ack.version != PROTO_VERSION {
        anyhow::bail!(
            "protocol version mismatch: hub speaks {}, we speak {}",
            ack.version,
            PROTO_VERSION
        );
    }

    for d in &ack.denied {
        warn!(service = %d.name, reason = %d.reason, "hub refused service");
    }
    if ack.granted.is_empty() {
        anyhow::bail!(
            "hub granted none of our {} service(s); check the hub's [[service]] list",
            cfg.services.len()
        );
    }
    info!(granted = ?ack.granted, "connected, services published");

    // Keepalive writer: a tiny frame the hub drains and ignores. If the write
    // starts failing we treat the connection as dead.
    let mut ping = tokio::time::interval(CONTROL_PING);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping.tick().await;

    // Split the control stream: the read half detects "hub went away", the
    // write half carries keepalive pings. They must not share `&mut`, or the
    // select! below would need two mutable borrows of the same stream.
    let (mut ctrl_r, mut ctrl_w) = tokio::io::split(ctrl);

    // The hub sends nothing after the Ack, so any successful read is garbage and
    // EOF (or an error) means the connection is gone.
    let ctrl_lost = async move {
        let mut sink = [0u8; 128];
        loop {
            match ctrl_r.read(&mut sink).await {
                Ok(0) | Err(_) => return,
                Ok(_) => continue,
            }
        }
    };
    tokio::pin!(ctrl_lost);

    loop {
        tokio::select! {
            stream = inbound.recv() => {
                let Some(stream) = stream else {
                    debug!("mux closed");
                    return Ok(());
                };
                let services = services.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_data_stream(stream, services).await {
                        debug!(error = %e, "forwarded stream failed");
                    }
                });
            }
            _ = &mut ctrl_lost => {
                info!("control stream closed by hub");
                return Ok(());
            }
            _ = ping.tick() => {
                // Any write on the mux flushes; sending an empty JSON object is
                // cheap and lets us notice a half-dead socket.
                if let Err(e) = protocol::write_msg(&mut ctrl_w, &serde_json::json!({})).await {
                    warn!(error = %e, "control ping failed");
                    return Ok(());
                }
            }
        }
    }
}

/// Handle one data stream opened by the hub.
async fn handle_data_stream(
    stream: yamux::Stream,
    services: Arc<HashMap<String, String>>,
) -> Result<()> {
    let mut stream = stream.compat();
    let header: StreamHeader = protocol::read_msg(&mut stream)
        .await
        .context("cannot read stream header")?;

    let local = services.get(&header.service).ok_or_else(|| {
        anyhow::anyhow!(
            "hub asked for service {:?} which is not configured here",
            header.service
        )
    })?;

    let mut upstream = TcpStream::connect(local)
        .await
        .with_context(|| format!("cannot connect to local target {local} for {:?}", header.service))?;
    let _ = upstream.set_nodelay(true);

    let (up, down) = tokio::io::copy_bidirectional(&mut upstream, &mut stream)
        .await
        .context("copy failed")?;
    debug!(service = %header.service, up, down, "stream finished");
    Ok(())
}

/// A tiny PRNG so we do not pull in `rand` just for backoff jitter.
///
/// Quality is irrelevant here; it only needs to decorrelate retry storms.
fn rand_u64() -> u64 {
    use std::cell::Cell;
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    thread_local! {
        static STATE: Cell<u64> = const { Cell::new(0) };
    }

    STATE.with(|s| {
        let mut v = s.get();
        if v == 0 {
            let mut h = RandomState::new().build_hasher();
            h.write_u128(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0),
            );
            v = h.finish() | 1;
        }
        // xorshift64*
        v ^= v >> 12;
        v ^= v << 25;
        v ^= v >> 27;
        s.set(v);
        v.wrapping_mul(0x2545_F491_4F6C_DD1D)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_stays_in_range() {
        for _ in 0..1000 {
            let backoff = 1000u64;
            let jitter = backoff / 5; // 200
            let v = rand_u64() % (2 * jitter + 1);
            let sleep = backoff - jitter + v;
            assert!((800..=1200).contains(&sleep), "sleep {sleep} out of range");
        }
    }
}
