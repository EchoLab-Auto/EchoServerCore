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
//! # 从只读变成可写
//!
//! 第一版是只读的，理由是「改动要重启宿主才生效，给一套只能改内存的写接口比
//! 没有更误导」。那个理由只在**没有热切换**时成立。现在有了：
//!
//! * 改上游 / 301 / 超时 / websocket / log_query —— 只换 [`serve::Shared`] 里
//!   的站点表，**监听 socket 不动**，在途连接不受影响；
//! * 改监听端口 —— 先绑新增、再换配置、最后停移除的（见
//!   [`serve::ServerHandle::sync`]）。新端口绑不上就整体放弃；
//! * 校验或绑定失败 —— 一个字节都不改，旧配置继续跑。
//!
//! 落盘走独立的 `tls.toml`（[`crate::store`]）：`host.toml` 是只读基线，
//! 理由见那个模块的注释。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use esc_control_api::{
    ControlError, TlsApplyOutcome, TlsCertInfo, TlsConfigInput, TlsControl, TlsCounters, TlsSite,
    TlsStatus,
};
use tracing::info;

use crate::certs::{self, CertStore};
use crate::config::{Action, CertConfig, TlsModuleConfig, Validated};
use crate::serve::{self, ServerHandle};
use crate::store::ConfigStore;

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

// ─────────────────────────────────────────────────────────────────────────────
// 运行期：配置 + 由它派生的一切，整体替换
// ─────────────────────────────────────────────────────────────────────────────

/// 一份「配置 + 由它派生出来的一切」。
///
/// 整体替换而不是字段级更新：`sites` 是 `validated.sites` 的投影，`certs` 是
/// `cfg.cert` 的投影，任何一次改动都会同时影响它们。分成几把锁去改会出现
/// 「面板看到新站点、serve 还在用旧的」这种窗口 —— 那种不一致最费时间。
pub struct State {
    cfg: TlsModuleConfig,
    validated: Validated,
    sites: Vec<TlsSite>,
    certs: Vec<TlsCertInfo>,
    tls_config: Arc<rustls::ServerConfig>,
}

/// 校验 + 读证书 + 构建 TLS 配置。**不碰端口**，所以可以在任何地方先跑一遍。
fn build(cfg: TlsModuleConfig) -> Result<(State, Arc<CertStore>), String> {
    let validated = cfg.validate().map_err(|errs| {
        // 逐条列出来。配置错通常是一批（复制粘贴改了一半），只报第一条会让
        // 人改一轮跑一轮。
        let mut msg = String::from("TLS 配置有误：");
        for e in errs {
            msg.push_str("\n  · ");
            msg.push_str(&e);
        }
        msg
    })?;

    // 证书读盘失败即失败（fail-closed）：一个"大部分端口能用、某个域名握手失败"
    // 的部署，排查成本远高于保存时报错。
    let cert_store = Arc::new(
        CertStore::load(&cfg.cert, &validated.default_cert).map_err(|e| format!("{e:#}"))?,
    );
    let tls_config = certs::server_config(Arc::clone(&cert_store), validated.tls12)
        .map_err(|e| format!("TLS 配置构建失败: {e:#}"))?;

    let sites = project_sites(&validated);
    let certs_view = project_certs(&cfg.cert, &validated);
    Ok((
        State {
            cfg,
            validated,
            sites,
            certs: certs_view,
            tls_config,
        },
        cert_store,
    ))
}

/// `Validated` → 面板看到的那份站点表。
///
/// 关键约定：`target` 必须是**上游地址原文**（不是"转发到 18790"这类描述），
/// 面板靠它与 `mapping.listen` 做字符串相等比较来接链路。
fn project_sites(validated: &Validated) -> Vec<TlsSite> {
    validated
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
        .collect()
}

fn project_certs(certs: &[CertConfig], validated: &Validated) -> Vec<TlsCertInfo> {
    certs
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
        .collect()
}

/// 配置结构 → 面板编辑用的输入结构（字段名刻意保持一致）。
///
/// 走 serde 而不是手写字段搬运：两边字段一一对应，手写会漏，而漏掉的字段
/// 表现是「面板保存后某个设置悄悄回到默认」—— 最难查的那类 bug。
fn to_input(cfg: &TlsModuleConfig) -> TlsConfigInput {
    serde_json::to_value(cfg)
        .ok()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

fn from_input(input: TlsConfigInput) -> Result<TlsModuleConfig, String> {
    let v = serde_json::to_value(&input).map_err(|e| format!("配置无法序列化: {e}"))?;
    serde_json::from_value(v).map_err(|e| format!("配置形状不对: {e}"))
}

impl State {
    fn status(&self, metrics: &Metrics) -> TlsStatus {
        TlsStatus {
            sites: self.sites.clone(),
            certs: self.certs.clone(),
            default_cert: self.validated.default_cert.clone(),
            tls12: self.validated.tls12,
            counters: metrics.snapshot(),
            config: to_input(&self.cfg),
        }
    }
}

/// TLS 终结的运行期。
///
/// 一个进程里只有一个（插件挂一次）。它同时是控制面（实现 [`TlsControl`]）
/// 和监听的持有者 —— 这两件事绑在一起是刻意的：**改配置与换监听必须是同一把
/// 锁下的同一件事**，分开就会有人先改了配置再想监听怎么换。
pub struct Runtime {
    state: RwLock<Arc<State>>,
    server: Mutex<Option<ServerHandle>>,
    metrics: Arc<Metrics>,
    store: Option<ConfigStore>,
}

impl Runtime {
    /// 首次启动：校验 → 读证书 → 绑端口 → 起监听。
    ///
    /// 绑端口在这里同步做，失败即返回 `Err`（挂载失败，宿主看到明确原因）。
    /// 与 `esc.edge-router` 同一个取舍：端口被占用、没有权限绑 1024 以下这类
    /// 错误要在启动阶段暴露，而不是变成「起来了但每个连接都失败」。
    pub fn start(
        cfg: TlsModuleConfig,
        metrics: Arc<Metrics>,
        store: Option<ConfigStore>,
    ) -> Result<Arc<Self>, String> {
        let (state, cert_store) = build(cfg)?;
        let bound = serve::bind(&state.validated.sites)?;
        let handle = serve::spawn(
            bound,
            Arc::clone(&state.tls_config),
            state.validated.handshake_timeout,
            Arc::clone(&metrics),
        )?;

        info!(
            sites = state.validated.sites.len(),
            certs = ?cert_store.names(),
            sni = ?cert_store.sni_names(),
            default_cert = ?state.validated.default_cert,
            tls12 = state.validated.tls12,
            addrs = ?handle.addrs(),
            store = store.as_ref().map(|s| s.path().display().to_string()),
            "TLS 终结已挂载"
        );

        Ok(Arc::new(Self {
            state: RwLock::new(Arc::new(state)),
            server: Mutex::new(Some(handle)),
            metrics,
            store,
        }))
    }

    fn current(&self) -> Arc<State> {
        match self.state.read() {
            Ok(g) => Arc::clone(&g),
            Err(p) => Arc::clone(&p.into_inner()),
        }
    }

    /// 当前实际在监听的端口（升序）。
    ///
    /// 与 `status().sites[].listen` 的区别：配置里写 `:0`（测试与临时部署）时，
    /// 这里给的是系统分配的真实端口。
    pub fn addrs(&self) -> Vec<std::net::SocketAddr> {
        let guard = match self.server.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard.as_ref().map(|h| h.addrs()).unwrap_or_default()
    }

    /// 状态文件路径（`None` = 只改内存，重启回到基线）。
    pub fn store_path(&self) -> Option<&std::path::Path> {
        self.store.as_ref().map(|s| s.path())
    }

    pub fn shutdown(&self) {
        let mut g = match self.server.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Some(h) = g.take() {
            h.shutdown();
            info!("TLS 终结已停止");
        }
    }
}

#[async_trait::async_trait]
impl TlsControl for Runtime {
    async fn status(&self) -> Result<TlsStatus, ControlError> {
        Ok(self.current().status(&self.metrics))
    }

    async fn apply_config(
        &self,
        input: TlsConfigInput,
    ) -> Result<TlsApplyOutcome, ControlError> {
        // ① 形状 + 语义校验 + 读证书。任何一步失败都在动端口**之前**返回，
        //    所以「保存失败」在线上是零影响 —— 旧监听、旧证书原样继续跑。
        let cfg = from_input(input).map_err(ControlError::invalid)?;
        let (next, _) = build(cfg).map_err(ControlError::invalid)?;

        // ② 同步监听：先绑新增、再换活配置、最后停移除的。
        //    在这一步之前 `next` 只活在栈上，失败就是 drop，什么都不留。
        let report = {
            let guard = match self.server.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            let handle = guard.as_ref().ok_or_else(|| {
                ControlError::unavailable(
                    "TLS 终结的监听不在运行（插件未挂载或已停止）".to_string(),
                )
            })?;
            handle
                .sync(
                    next.validated.sites.clone(),
                    Arc::clone(&next.tls_config),
                    next.validated.handshake_timeout,
                )
                .map_err(ControlError::bind)?
        };

        // ③ 换控制面视图。放这一步而不是 sync 之前：sync 成功才意味着
        //    「这套配置真的在跑」，先换视图会出现面板显示的东西还没生效的窗口。
        let status = next.status(&self.metrics);
        {
            let prev = Arc::new(next);
            let mut g = match self.state.write() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            *g = prev;
        }

        // ④ 落盘。失败**不**回滚运行期：配置已经在生效了，回滚只会让用户
        //    更困惑（"它明明在跑"）。改成把这句话带回去让面板显示。
        let (store, warn) = match &self.store {
            None => (None, None),
            Some(store) => match store.save(&self.current().cfg) {
                Ok(()) => (Some(store.path().display().to_string()), None),
                Err(e) => (
                    None,
                    Some(format!("配置已在运行期生效，但写盘失败，重启后会回到基线：{e}")),
                ),
            },
        };

        info!(
            rebound = ?report.added,
            released = ?report.removed,
            unchanged = report.unchanged,
            store = ?store,
            "TLS 配置已应用"
        );

        Ok(TlsApplyOutcome {
            status,
            rebound: report.added.iter().map(|a| a.to_string()).collect(),
            released: report.removed.iter().map(|a| a.to_string()).collect(),
            unchanged: report.unchanged,
            store,
            warn,
        })
    }
}

/// Ctx 里的服务键。宿主按这个键取控制面。
pub static TLS_CONTROL: echo_context::ServiceKey<Arc<dyn TlsControl>> =
    echo_context::ServiceKey::new("esc.tls.control");

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Defaults, Site};
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
                site(
                    "old",
                    "0.0.0.0:2223",
                    Action::Redirect("https://{hostname}:2224{uri}".into()),
                ),
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

    /// 站点投影：`target` 必须是上游地址原文。
    ///
    /// 这条是面板「把 TLS 与隧道接成一条链」的全部依据 —— 它拿
    /// `site.target == mapping.listen` 做字符串比较。写成别的形状链路就断了，
    /// 而且不会有任何报错，只是面板上少一行箭头。
    #[test]
    fn site_projection_keeps_raw_target() {
        let sites = project_sites(&validated());
        assert_eq!(sites.len(), 2);
        assert_eq!(sites[0].name, "panel");
        assert_eq!(sites[0].kind, "proxy");
        assert_eq!(sites[0].target, "127.0.0.1:18790");
        assert_eq!(sites[0].listen, "0.0.0.0:2222");
        assert_eq!(sites[0].read_timeout_secs, 3600);
        assert_eq!(sites[1].kind, "redirect");
        assert_eq!(sites[1].target, "https://{hostname}:2224{uri}");
    }

    /// 证书投影：SNI 归一化成小写，`default` 标在正确的那张上。
    #[test]
    fn cert_projection_normalizes_sni() {
        let cs = project_certs(&certs(), &validated());
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[1].sni, vec!["cloud.tx.evencewu.asia"]);
        assert!(cs[0].default, "self 是默认证书");
        assert!(!cs[1].default);
    }

    /// 配置 ↔ 输入往返必须**语义无损**。
    ///
    /// 面板的编辑流是：读 `status.config` → 改 → POST 回来。这条链上任何
    /// 一个字段在往返里丢了，"面板保存后某个设置悄悄变成默认值"就是这么来的。
    /// 特别注意 `Option` 的 `None`（= 继承默认）不能变成 `Some(默认值)`。
    #[test]
    fn config_roundtrips_through_input() {
        let cfg = TlsModuleConfig {
            defaults: Defaults {
                cert: Some("self".into()),
                tls12: false,
                max_body_bytes: 2048,
                handshake_timeout_secs: 7,
                hsts: true,
            },
            cert: certs(),
            site: vec![
                crate::config::SiteConfig {
                    name: "panel".into(),
                    listen: vec!["0.0.0.0:2222".into(), "0.0.0.0:2224".into()],
                    upstream: Some("127.0.0.1:18790".into()),
                    redirect: None,
                    websocket: true,
                    log_query: false,
                    read_timeout_secs: 3600,
                    max_body_bytes: None,
                    hsts: None,
                },
                crate::config::SiteConfig {
                    name: "old".into(),
                    listen: vec!["0.0.0.0:2223".into()],
                    upstream: None,
                    redirect: Some("https://{hostname}:2224{uri}".into()),
                    websocket: false,
                    log_query: true,
                    read_timeout_secs: 300,
                    max_body_bytes: Some(99),
                    hsts: Some(false),
                },
            ],
            store: None,
        };

        let input = to_input(&cfg);
        assert_eq!(input.site[0].listen.len(), 2, "多端口不能丢");
        assert_eq!(input.site[0].max_body_bytes, None, "继承默认必须还是 None");
        assert_eq!(input.site[1].max_body_bytes, Some(99));
        assert_eq!(input.site[1].hsts, Some(false));
        assert!(input.site[1].log_query);
        assert_eq!(input.cert[1].key, std::path::PathBuf::from("/etc/esc/pki/cloud.key"));

        let back = from_input(input).expect("往返应成功");
        assert_eq!(back.defaults.max_body_bytes, 2048);
        assert!(!back.defaults.tls12);
        assert_eq!(back.site.len(), 2);
        assert_eq!(back.site[0].upstream.as_deref(), Some("127.0.0.1:18790"));
        assert_eq!(back.site[1].redirect.as_deref(), Some("https://{hostname}:2224{uri}"));
    }

    /// 计数是累计值：面板拿两次采样算速率，所以只能增不能减。
    #[test]
    fn counters_never_go_backwards() {
        let m = Metrics::new();
        m.record_request();
        let first = m.snapshot().requests_total;
        m.record_request();
        assert!(m.snapshot().requests_total > first);

        m.record_handshake_failure();
        m.record_upstream_error();
        m.record_body_too_large();
        let c = m.snapshot();
        assert_eq!(c.handshake_failures, 1);
        assert_eq!(c.upstream_errors, 1);
        assert_eq!(c.body_too_large, 1);
    }

    /// 空配置要校验失败（没有证书等于端口开着也握手不了），
    /// 但 `TlsStatus::default()` 仍要能序列化成面板认识的 JSON。
    #[test]
    fn empty_defaults_do_not_break_serialization() {
        let cfg = TlsModuleConfig {
            defaults: Defaults::default(),
            cert: vec![],
            site: vec![],
            store: None,
        };
        assert!(cfg.validate().is_err(), "空配置应当校验失败");

        let json = serde_json::to_string(&TlsStatus::default()).unwrap();
        assert!(json.contains("\"sites\":[]"), "{json}");
        assert!(json.contains("\"config\""), "{json}");
    }
}
