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

pub mod admin;
pub mod auth;
pub mod config;
pub mod proxy;
pub mod routes;
pub mod store;
pub mod web;

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

pub use config::{GatewayConfig, RuntimeConfig};
pub use proxy::Gateway;
pub use routes::RouteStore;
pub use store::AccountStore;

/// 起网关：按配置里的每个 route 各起一个监听，等 `shutdown` 后一起退出。
///
/// 任何一个端口绑不上都返回 `Err`——**不做"部分启动"**。一个认证网关只起来了
/// 一半，比完全没起来更危险：你以为 2224 被保护着，其实它在裸跑。
pub async fn run(cfg: RuntimeConfig, shutdown: CancellationToken) -> anyhow::Result<()> {
    let gw = Arc::new(Gateway::new(Arc::new(cfg)));

    // 先全部绑定，全部成功才开始服务。这样端口冲突会在启动时暴露，
    // 而不是等到某个用户访问那个端口才发现——一个认证网关只起来了一半，
    // 比完全没起来更危险：你以为 2224 被保护着，其实它在裸跑。
    let wanted: Vec<config::RouteRuntime> = gw.routes.all().iter().map(|r| (**r).clone()).collect();
    let prepared = gw
        .listeners
        .prepare(&wanted)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // 管理端口单独绑、单独管：它**不登记进 ListenerSet**，否则面板做热应用
    // 时会把它当成"多余的路由端口"顺手停掉——那等于改个上游就把管理面弄没了。
    // 改 admin_listen 需要重启，和 title 一类。
    let admin_listener = match gw.cfg.admin_listen {
        Some(addr) => Some(
            tokio::net::TcpListener::bind(addr)
                .await
                .map_err(|e| anyhow::anyhow!("管理端口 {addr} 绑定失败: {e}"))?,
        ),
        None => None,
    };

    tracing::info!(
        routes = gw.route_count(),
        users = gw.cfg.users.len(),
        admin_listen = ?gw.cfg.admin_listen,
        http_only = !gw.cfg.secure_cookie,
        "gateway starting"
    );

    gw.listeners.commit(&gw, prepared);

    let admin_task = match (gw.cfg.admin_listen, admin_listener) {
        (Some(addr), Some(l)) => {
            let gw = gw.clone();
            let shut = shutdown.clone();
            Some(tokio::spawn(async move {
                proxy::serve_admin(gw, addr, l, shut).await
            }))
        }
        _ => None,
    };

    shutdown.cancelled().await;
    if let Some(t) = admin_task {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), t).await;
    }
    // 关掉所有监听（cancel 会让 accept loop 退出、连接进入收尾）
    for addr in gw.listeners.addrs() {
        gw.listeners.cancel(addr);
    }
    // 给在途请求一点时间收尾，但别无限等
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    tracing::info!("gateway stopped");
    Ok(())
}
