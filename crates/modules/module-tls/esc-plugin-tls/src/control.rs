//! 控制面：把「这一层在做什么」暴露给面板。
//!
//! # 为什么值得单开一个模块
//!
//! 这一层从前是 nginx，面板完全看不到它。于是"2222 这个端口是谁在听、转发到哪、
//! 要不要登录"这个问题只能在几个页面之间来回切才拼得出来。收进 `esc-host`
//! 之后它和隧道在同一个进程，这条链路终于能被一屏讲完：
//!
//! ```text
//! :2222  ──TLS──▶  127.0.0.1:18790  ──隧道──▶  4090 的 127.0.0.1:8080
//!  └─ TlsSite ──┘   └──── mapping.listen ────┘
//! ```
//!
//! 面板靠 `site.target == mapping.listen` 把两段接起来，所以这里给出的
//! `target` 必须是**上游地址原文**，不能是加工过的描述。
//!
//! # 只读
//!
//! 没有 upsert/delete：证书与监听端口的改动要重启宿主才生效（见
//! `docs/TLS-MODULE.md`）。加一套只能改内存、活不过重启的写接口比没有更误导。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use esc_control_api::{ControlError, TlsCertInfo, TlsControl, TlsCounters, TlsSite, TlsStatus};

use crate::config::{Action, CertConfig, Validated};

/// 累计计数。
///
/// 全是单调递增的：面板拿两次采样就能算速率，不需要服务端存时间窗口。
#[derive(Debug, Default)]
pub struct Metrics {
    requests_total: AtomicU64,
    handshake_failures: AtomicU64,
    upstream_errors: AtomicU64,
    body_too_large: AtomicU64,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 一次请求完成了（握手成功、进入 HTTP）。
    ///
    /// 计在这里而不是"响应写完"时：面板关心的是"有没有流量进来"，而不是
    /// 每条响应是否完整 —— 客户端中途断开不该让计数看起来像没收到请求。
    pub fn record_request(&self) {
        self.requests_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_handshake_failure(&self) {
        self.handshake_failures.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_upstream_error(&self) {
        self.upstream_errors.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_body_too_large(&self) {
        self.body_too_large.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> TlsCounters {
        TlsCounters {
            requests_total: self.requests_total.load(Ordering::Relaxed),
            handshake_failures: self.handshake_failures.load(Ordering::Relaxed),
            upstream_errors: self.upstream_errors.load(Ordering::Relaxed),
            body_too_large: self.body_too_large.load(Ordering::Relaxed),
        }
    }
}

/// 挂载时冻结的配置快照 + 活的计数器。
///
/// 配置部分不做 RwLock：它整个生命周期都不变（改配置 = 重启），拿一把锁只为
/// 返回一份永远不会变的数据没有意义。
pub struct TlsControlImpl {
    sites: Vec<TlsSite>,
    certs: Vec<TlsCertInfo>,
    default_cert: Option<String>,
    tls12: bool,
    metrics: Arc<Metrics>,
}

impl TlsControlImpl {
    pub fn new(
        validated: &Validated,
        certs: &[CertConfig],
        metrics: Arc<Metrics>,
    ) -> Self {
        let sites = validated
            .sites
            .iter()
            .map(|s| {
                let (kind, target) = match &s.action {
                    Action::Proxy(addr) => ("proxy".to_string(), addr.to_string()),
                    Action::Redirect(t) => ("redirect".to_string(), t.clone()),
                };
                TlsSite {
                    name: s.name.clone(),
                    listen: s.addr.to_string(),
                    kind,
                    target,
                    websocket: s.websocket,
                    log_query: s.log_query,
                    read_timeout_secs: s.read_timeout.as_secs(),
                    max_body_bytes: s.max_body_bytes,
                    hsts: s.hsts,
                }
            })
            .collect();

        let certs = certs
            .iter()
            .map(|c| TlsCertInfo {
                name: c.name.clone(),
                cert: c.cert.display().to_string(),
                sni: c
                    .sni
                    .iter()
                    .map(|n| n.trim().to_ascii_lowercase())
                    .filter(|n| !n.is_empty())
                    .collect(),
                default: validated.default_cert.as_deref() == Some(c.name.as_str()),
            })
            .collect();

        Self {
            sites,
            certs,
            default_cert: validated.default_cert.clone(),
            tls12: validated.tls12,
            metrics,
        }
    }
}

#[async_trait::async_trait]
impl TlsControl for TlsControlImpl {
    async fn status(&self) -> Result<TlsStatus, ControlError> {
        Ok(TlsStatus {
            sites: self.sites.clone(),
            certs: self.certs.clone(),
            default_cert: self.default_cert.clone(),
            tls12: self.tls12,
            counters: self.metrics.snapshot(),
        })
    }
}

/// Ctx 里的服务键。宿主按这个键取控制面。
pub static TLS_CONTROL: echo_context::ServiceKey<Arc<dyn TlsControl>> =
    echo_context::ServiceKey::new("esc.tls.control");

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Defaults, Site, TlsModuleConfig};
    use std::net::SocketAddr;
    use std::time::Duration;

    fn site(name: &str, addr: &str, action: Action) -> Site {
        Site {
            name: name.into(),
            addr: addr.parse::<SocketAddr>().unwrap(),
            action,
            websocket: true,
            log_query: false,
            read_timeout: Duration::from_secs(3600),
            max_body_bytes: 1024,
            hsts: false,
        }
    }

    fn validated() -> Validated {
        Validated {
            default_cert: Some("self".into()),
            tls12: true,
            handshake_timeout: Duration::from_secs(10),
            sites: vec![
                site(
                    "panel",
                    "0.0.0.0:2222",
                    Action::Proxy("127.0.0.1:18790".parse().unwrap()),
                ),
                site("old", "0.0.0.0:2223", Action::Redirect("https://{hostname}:2224{uri}".into())),
            ],
        }
    }

    fn certs() -> Vec<CertConfig> {
        vec![
            CertConfig {
                name: "self".into(),
                cert: "/etc/esc/pki/self.crt".into(),
                key: "/etc/esc/pki/self.key".into(),
                sni: vec![],
            },
            CertConfig {
                name: "cloud-tx".into(),
                cert: "/etc/esc/pki/cloud.crt".into(),
                key: "/etc/esc/pki/cloud.key".into(),
                // 故意大写，验证归一化
                sni: vec!["Cloud.TX.evencewu.asia".into()],
            },
        ]
    }

    #[tokio::test]
    async fn status_mirrors_the_config() {
        let m = Metrics::new();
        let ctl = TlsControlImpl::new(&validated(), &certs(), m);
        let st = ctl.status().await.expect("应能取到状态");

        assert_eq!(st.sites.len(), 2);
        assert_eq!(st.sites[0].name, "panel");
        assert_eq!(st.sites[0].kind, "proxy");
        // 这一条是面板把 TLS 与隧道接起来的关键：target 必须是上游地址原文
        assert_eq!(st.sites[0].target, "127.0.0.1:18790");
        assert!(st.sites[0].websocket);
        assert_eq!(st.sites[0].read_timeout_secs, 3600);

        assert_eq!(st.sites[1].kind, "redirect");
        assert_eq!(st.sites[1].target, "https://{hostname}:2224{uri}");

        assert_eq!(st.certs.len(), 2);
        assert_eq!(st.certs[1].sni, vec!["cloud.tx.evencewu.asia"], "SNI 要归一化成小写");
        assert!(st.certs[0].default, "self 是默认证书");
        assert!(!st.certs[1].default);

        assert_eq!(st.default_cert.as_deref(), Some("self"));
        assert!(st.tls12);
    }

    #[tokio::test]
    async fn counters_are_monotonic_and_reflected() {
        let m = Metrics::new();
        let ctl = TlsControlImpl::new(&validated(), &certs(), Arc::clone(&m));
        assert_eq!(ctl.status().await.unwrap().counters.requests_total, 0);

        m.record_request();
        m.record_request();
        m.record_handshake_failure();
        m.record_upstream_error();
        m.record_body_too_large();

        let c = ctl.status().await.unwrap().counters;
        assert_eq!(c.requests_total, 2);
        assert_eq!(c.handshake_failures, 1);
        assert_eq!(c.upstream_errors, 1);
        assert_eq!(c.body_too_large, 1);
    }

    /// 面板拿两次采样算速率，所以计数只能增不能减。
    #[tokio::test]
    async fn snapshot_never_goes_backwards() {
        let m = Metrics::new();
        m.record_request();
        let first = m.snapshot().requests_total;
        m.record_request();
        let second = m.snapshot().requests_total;
        assert!(second > first);
    }

    /// 默认值（空配置）也要能构造出状态 —— 面板在插件没注册时走的是 501，
    /// 但一旦注册了就不该因为某个字段缺失而整页崩掉。
    #[tokio::test]
    async fn empty_defaults_do_not_break_serialization() {
        let cfg = TlsModuleConfig {
            defaults: Defaults::default(),
            cert: vec![],
            site: vec![],
        };
        assert!(cfg.validate().is_err(), "空配置应当校验失败");

        // 但 TlsStatus 本身要能序列化成面板认识的 JSON
        let st = TlsStatus::default();
        let json = serde_json::to_string(&st).unwrap();
        assert!(json.contains("\"sites\":[]"), "{json}");
        assert!(json.contains("\"certs\":[]"), "{json}");
    }
}

/// 从 Ctx 取控制面。宿主与测试都走这个入口，不各自 `get` 一次键。
///
/// 与 `esc_plugin_tunnel::resolve` / `esc_plugin_edge_router::resolve` 同一个
/// 形状：拿不到时给一句能直接贴进 issue 的话，而不是一个裸的 None。
pub fn resolve(ctx: &echo_context::Ctx) -> Result<Arc<dyn TlsControl>, ControlError> {
    ctx.require(&TLS_CONTROL).map_err(|e| {
        ControlError::unavailable(format!(
            "TLS 终结服务未注册（host.toml 里 [tls] enabled 了吗）: {e}"
        ))
    })
}
