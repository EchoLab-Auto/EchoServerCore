//! Spoke: the intranet agent.
//!
//! A spoke dials *out* to the hub over mutual TLS, so no inbound port needs to
//! be opened on the intranet side. All forwarded traffic travels inside that
//! single connection, multiplexed by yamux.
//!
//! 与 hub 一样，这个 crate 现在只是 [`esc_engine::SpokeEngine`] 的薄封装：
//! `esc spoke --config spoke.toml` 的行为不变，而引擎可以在运行时被
//! Web 面板改配置（见 `esc-host` + `esc-plugin-tunnel`）。

use anyhow::Result;
use esc_common::config::SpokeConfig;
use esc_engine::{Engine, EngineConfig};
use tracing::info;

/// Run the spoke forever (reconnecting with backoff) until a shutdown signal.
pub async fn run(cfg: SpokeConfig) -> Result<()> {
    cfg.validate().map_err(|e| anyhow::anyhow!(e))?;
    let engine = Engine::start(EngineConfig::from_spoke_config(&cfg))
        .await
        .map_err(|e| anyhow::anyhow!("cannot start spoke: {}", e.message))?;

    info!(
        spoke = %cfg.spoke.id,
        server = %cfg.spoke.server,
        services = cfg.services.len(),
        "spoke ready"
    );

    shutdown_signal().await;
    info!("shutdown signal received");
    engine.shutdown().await;
    info!("spoke stopped");
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
