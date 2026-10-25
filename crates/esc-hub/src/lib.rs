//! Hub: the public-facing side of echo-server-core.
//!
//! 这个 crate 现在只是 [`esc_engine::HubEngine`] 的一层薄封装，负责
//! 「读静态配置 → 起引擎 → 等信号 → 优雅退出」。真正的实现（动态端口绑定、
//! 注册表、转发）都在 `esc-engine` 里，因为它要被 Web 面板在运行时驱动。
//!
//! 保留这个 crate 是为了两条兼容性：
//!
//! 1. `esc hub --config hub.toml` 的行为和命令行参数完全不变；
//! 2. `esc_hub::registry::Registry` 等公开路径继续可用（re-export）。
//!
//! 想用同一套引擎但可运行时增删映射，请看 `esc-host` + `esc-plugin-tunnel`。

pub use esc_engine::hub::registry;

use anyhow::Result;
use esc_common::config::HubConfig;
use esc_engine::{Engine, EngineConfig};
use tracing::info;

/// Run the hub until the process receives a shutdown signal.
///
/// 静态模式：`[[service]]` 只在启动时读一次，不写任何映射表。
pub async fn run(cfg: HubConfig) -> Result<()> {
    cfg.validate().map_err(|e| anyhow::anyhow!(e))?;
    let engine = Engine::start(EngineConfig::from_hub_config(&cfg))
        .await
        .map_err(|e| anyhow::anyhow!("cannot start hub: {}", e.message))?;

    info!(
        control = %engine_control_addr(&engine),
        services = cfg.services.len(),
        "hub ready"
    );

    shutdown_signal().await;
    info!("shutdown signal received");
    engine.shutdown().await;
    info!("hub stopped");
    Ok(())
}

fn engine_control_addr(engine: &Engine) -> String {
    engine
        .control_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|| "-".to_string())
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
