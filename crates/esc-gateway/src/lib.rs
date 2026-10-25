//! `esc-gateway`：在面板（或任何 HTTP 服务）**前面**加一道认证的网关。
//!
//! # 它解决什么问题
//!
//! 面板自带 Basic 认证，但那有三个不足：
//!
//! 1. 口令每次请求都在链路上（base64 = 明文），复用同一个口令访问多个端口；
//! 2. 口令以明文存在配置文件里；
//! 3. 无法按端口授权——`2223`（看映射）和 `2224`（能启停 Docker 容器）用同一套凭据。
//!
//! 网关把这些收拢到一处：**服务端会话 + argon2 哈希 + 按 route 的 ACL**。
//!
//! # 为什么是独立进程
//!
//! 它是所有未认证流量的第一站。header smuggling、路径穿越这类 bug 在这里
//! 就是安全事件，所以给它自己的沙箱、自己的用户，和隧道进程彻底分开：
//!
//! ```text
//! esc-host (User=esc)           隧道 + 端口映射面板，只听回环
//! esc-gateway (User=esc-gateway) ← 公网入口，认证后转发到回环
//! ```
//!
//! # 明确不做
//!
//! WebSocket、HTTP/2、TLS 终结（见 [`proxy`] 的文档）。需要 TLS 就在前面
//! 再放一层 nginx/Caddy，或者等以后加。

pub mod auth;
pub mod config;
pub mod proxy;
pub mod web;

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

pub use config::{GatewayConfig, RuntimeConfig};
pub use proxy::Gateway;

/// 起网关：按配置里的每个 route 各起一个监听，等 `shutdown` 后一起退出。
///
/// 任何一个端口绑不上都返回 `Err`——**不做"部分启动"**。一个认证网关只起来了
/// 一半，比完全没起来更危险：你以为 2224 被保护着，其实它在裸跑。
pub async fn run(cfg: RuntimeConfig, shutdown: CancellationToken) -> anyhow::Result<()> {
    // 先全部绑定，全部成功才开始服务。这样端口冲突会在启动时暴露，
    // 而不是等到某个用户访问那个端口才发现。
    let mut listeners = Vec::new();
    for r in cfg.routes.values() {
        let addr = r.listen;
        let l = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| anyhow::anyhow!("route {:?} 绑定 {addr} 失败: {e}", r.name))?;
        listeners.push((r.clone(), l));
    }

    let gw = Arc::new(Gateway::new(Arc::new(cfg)));

    tracing::info!(
        routes = listeners.len(),
        users = gw.cfg.users.len(),
        http_only = !gw.cfg.secure_cookie,
        "gateway starting"
    );

    let mut tasks = Vec::new();
    for (route, listener) in listeners {
        let gw = gw.clone();
        let shut = shutdown.clone();
        tasks.push(tokio::spawn(
            async move { proxy::serve_route(gw, route, listener, shut).await },
        ));
    }

    // 任一任务结束就收尾（正常情况下只有 shutdown 能让它们结束）
    shutdown.cancelled().await;
    for t in tasks {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), t).await;
    }
    tracing::info!("gateway stopped");
    Ok(())
}
