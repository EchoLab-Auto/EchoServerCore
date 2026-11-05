//! 计数器（实现细节）。
//!
//! 契约类型（`EdgeRoute` / `EdgeRouterStatus` / `EdgeRouteControl`）在
//! `esc-control-api` 里 —— 面板要能编译期读到同一份形状，所以不能定义在
//! 插件里。这个文件只剩"插件私有"的东西：原子计数器。
//!
//! 为什么不做按前缀计数：那需要在无锁热路径上加一次锁，而日志里已经有
//! `prefix=` 字段，`grep`/`awk` 就能统计，收益不抵成本。

use std::sync::atomic::{AtomicU64, Ordering};

use esc_control_api::EdgeRouterCounters;

/// 原子计数器。`Relaxed` 足够：这些数字只用于观察，不需要与任何写入建立
/// happens-before 关系（不会有人根据它们做决策）。
#[derive(Debug, Default)]
pub struct Counters(Inner);

#[derive(Debug, Default)]
struct Inner {
    requests: AtomicU64,
    proxied: AtomicU64,
    unrouted: AtomicU64,
    upstream_errors: AtomicU64,
    upgraded: AtomicU64,
}

impl Counters {
    pub fn snapshot(&self) -> EdgeRouterCounters {
        EdgeRouterCounters {
            requests: self.0.requests.load(Ordering::Relaxed),
            proxied: self.0.proxied.load(Ordering::Relaxed),
            unrouted: self.0.unrouted.load(Ordering::Relaxed),
            upstream_errors: self.0.upstream_errors.load(Ordering::Relaxed),
            upgraded: self.0.upgraded.load(Ordering::Relaxed),
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
    pub(crate) fn record_upgraded(&self) {
        self.0.upgraded.fetch_add(1, Ordering::Relaxed);
    }
}
