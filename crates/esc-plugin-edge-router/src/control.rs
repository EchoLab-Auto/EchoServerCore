//! 控制面：宿主 / 其他插件按契约查询与增删路由。
//!
//! 这里是**契约**（trait + 数据类型），不是实现。和隧道插件一个套路：
//! 插件把 `Arc<dyn EdgeRouterControl>` 放进 `Ctx`，宿主或子进程插件通过
//! `ServiceKey` 取用。好处是将来面板想显示路由表、或者想让用户点一下加
//! 路由，都不需要动这个 crate —— 它已经是一个服务了。
//!
//! 错误一律用 [`ControlError`]：`ErrorKind` 自带 `http_status()`，面板可以
//! 直接把 `not_found` 映射成 404、`conflict` 映射成 409，不会出现两套码表。

use std::sync::atomic::{AtomicU64, Ordering};

use esc_control_api::ControlError;
use serde::{Deserialize, Serialize};

/// 一条路由。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteView {
    /// 域名前缀（`cad`）或完整域名（`cad.example.com`）。小写。
    pub prefix: String,
    /// 本机端口。
    pub port: u16,
}

/// 指标快照。
///
/// 只做全局聚合：按前缀计数需要在无锁热路径上加一次锁，而日志里已经有
/// `prefix=` 字段，`grep`/`awk` 就能统计，不值得。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CounterSnapshot {
    pub requests: u64,
    pub proxied: u64,
    pub unrouted: u64,
    pub upstream_errors: u64,
    pub upgrade_rejected: u64,
}

/// 运行时状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouterStatus {
    /// 实际在听的地址（`127.0.0.1:2229` 这种形态，端口 0 已被解析成真实端口）。
    pub listens: Vec<String>,
    pub routes: Vec<RouteView>,
    pub default_port: Option<u16>,
    pub counters: CounterSnapshot,
}

/// 原子计数器。`Relaxed` 足够：这些数字只用于观察，不需要与任何写入建立
/// happens-before 关系（不会有人根据它们做决策）。
#[derive(Debug, Default)]
pub struct Counters(InnerCounters);

#[derive(Debug, Default)]
struct InnerCounters {
    requests: AtomicU64,
    proxied: AtomicU64,
    unrouted: AtomicU64,
    upstream_errors: AtomicU64,
    upgrade_rejected: AtomicU64,
}

impl Counters {
    pub fn snapshot(&self) -> CounterSnapshot {
        CounterSnapshot {
            requests: self.0.requests.load(Ordering::Relaxed),
            proxied: self.0.proxied.load(Ordering::Relaxed),
            unrouted: self.0.unrouted.load(Ordering::Relaxed),
            upstream_errors: self.0.upstream_errors.load(Ordering::Relaxed),
            upgrade_rejected: self.0.upgrade_rejected.load(Ordering::Relaxed),
        }
    }
    pub(crate) fn record_request(&self) {
        self.0.requests.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn record_proxied(&self) {
        self.0.proxied.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn record_unrouted(&self) {
        self.0.unrouted.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn record_upstream_error(&self) {
        self.0.upstream_errors.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn record_upgrade_rejected(&self) {
        self.0.upgrade_rejected.fetch_add(1, Ordering::Relaxed);
    }
}

/// 边缘路由的控制面。
#[async_trait::async_trait]
pub trait EdgeRouterControl: Send + Sync {
    /// 监听地址、路由表、计数器。
    async fn status(&self) -> Result<RouterStatus, ControlError>;

    /// 新增或覆盖一条路由。
    ///
    /// 只在内存里生效 —— 持久化是宿主的职责（改 TOML 再重启）。这一点在
    /// 文档里写明，避免重启后"路由莫名消失"被当成 bug。
    async fn upsert_route(&self, prefix: &str, port: u16) -> Result<RouteView, ControlError>;

    /// 删除一条路由。
    async fn delete_route(&self, prefix: &str) -> Result<(), ControlError>;
}

/// 把 [`crate::EdgeRouter`] 适配成契约。
pub struct RouterHandle {
    router: std::sync::Arc<crate::EdgeRouter>,
}

impl RouterHandle {
    pub fn new(router: std::sync::Arc<crate::EdgeRouter>) -> Self {
        Self { router }
    }

    pub fn router(&self) -> std::sync::Arc<crate::EdgeRouter> {
        std::sync::Arc::clone(&self.router)
    }
}

#[async_trait::async_trait]
impl EdgeRouterControl for RouterHandle {
    async fn status(&self) -> Result<RouterStatus, ControlError> {
        Ok(self.router.status())
    }
    async fn upsert_route(&self, prefix: &str, port: u16) -> Result<RouteView, ControlError> {
        self.router.upsert_route(prefix, port)
    }
    async fn delete_route(&self, prefix: &str) -> Result<(), ControlError> {
        self.router.delete_route(prefix)
    }
}
