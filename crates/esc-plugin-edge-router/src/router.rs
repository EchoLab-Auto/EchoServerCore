//! 路由与转发：把 `Host` 的第一段映射到本机端口，并做 HTTP 反向代理。
//!
//! # 匹配规则
//!
//! 1. `Host` 去掉端口、去掉末尾的点、转小写；
//! 2. 整串命中路由表（键里带点的完整域名，如 `cad.example.com`）优先；
//! 3. 否则取第一段（`cad`）再查一次；
//! 4. 都没有 → 若配了 `default_port` 就用它，否则 **421**。
//!
//! 为什么用 421（Misdirected Request）而不是 404：后端自己也会回 404，
//! 两者混在一起就分不清"前缀没配"和"路径不对"。421 在本仓的语义是
//! "这个 Host 没有对应的服务"，且带一段可读的 JSON 说明该怎么加路由。
//!
//! # 与 nginx 版的差异
//!
//! 有一处**故意的**差异：`X-Forwarded-For` 这里是**追加**（`客户端, 已有值`），
//! nginx 版是覆盖。追加是反向代理的通行做法（多层代理时能留下链路），
//! 代价是客户端能伪造前面的项 —— 所以后端**只能信任最后一项**。这条写进
//! 文档（`docs/EDGE-ROUTER.md`），避免以后有人把 `XFF` 第一项当真实 IP。

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use tokio::net::TcpListener;
use tracing::{debug, warn};

use esc_control_api::ControlError;

use crate::config::RouterConfig;
use crate::control::{Counters, RouteView, RouterStatus};

/// 跳过的逐跳首部（RFC 9110 §7.6.1）。转发时两端都不该带。
const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// 网关自己重算的首部：客户端带来的同名值一律丢掉，避免伪造。
const REWRITTEN: [&str; 4] = [
    "x-forwarded-for",
    "x-forwarded-proto",
    "x-forwarded-host",
    "x-forwarded-port",
];

/// 一次请求的路由判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// 命中某个后端端口，`matched` 是命中的路由键（用于日志与指标）。
    Backend { port: u16, matched: String },
    /// 走 `default_port` 兜底。
    Default { port: u16 },
    /// 没有路由。`prefix` 是 Host 的第一段（纯 IP 时为 `None`）。
    Unrouted { prefix: Option<String> },
}

/// 从 `Host` 里取第一段作为前缀。
///
/// | 输入 | 输出 |
/// | --- | --- |
/// | `cad.example.com` | `Some("cad")` |
/// | `cad.example.com:2229` | `Some("cad")` |
/// | `CAD.Example.COM.` | `Some("cad")` |
/// | `cad.114.132.241.46.nip.io` | `Some("cad")` |
/// | `114.132.241.46` | `None`（IPv4 没有前缀） |
/// | `[::1]:8080` | `None` |
/// | `localhost` | `None`（只有一段，不是前缀） |
/// | `` | `None` |
pub fn first_label(host: &str) -> Option<String> {
    let host = host.trim();
    // IPv6 字面量（`[::1]:8080`）没有"前缀"这个概念，方括号里的冒号也不是端口分隔符。
    if host.is_empty() || host.starts_with('[') {
        return None;
    }
    // 主机名里不可能出现冒号，所以第一个冒号之前就是主机部分。
    let host = host.split(':').next().unwrap_or(host);
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    // 纯 IPv4（`1.2.3.4`）不是"前缀"，否则 `114.132.241.46` 会被当成前缀 114。
    if host.parse::<std::net::Ipv4Addr>().is_ok() {
        return None;
    }
    match host.split_once('.') {
        // 单段（localhost / 裸机器名）没有前缀
        None => None,
        Some((first, rest)) if first.is_empty() || rest.is_empty() => None,
        Some((first, _)) => Some(first.to_string()),
    }
}

/// 命中路由表的状态 + 转发客户端。
///
/// 路由表放在 `RwLock` 里是为了支持运行期增删（[`crate::EdgeRouterControl`]）。
/// 读路径只有一次 `get`，几十条路由的规模下锁竞争可以忽略；换 `ArcSwap`
/// 要引入新依赖，收益不明显。
pub struct EdgeRouter {
    routes: RwLock<BTreeMap<String, u16>>,
    default_port: Option<u16>,
    client: Client<HttpConnector, Body>,
    timeout: Option<Duration>,
    listens: Vec<SocketAddr>,
    counters: Counters,
}

impl EdgeRouter {
    pub fn new(cfg: &RouterConfig, listens: Vec<SocketAddr>) -> Arc<Self> {
        let mut connector = HttpConnector::new();
        // 只连 127.0.0.1 —— 这个插件的定位是"把域名指到本机端口"，不是
        // 通用出网代理。限制目标地址也顺手关掉了 SSRF 面。
        connector.set_connect_timeout(Some(Duration::from_secs(5)));
        connector.set_nodelay(true);
        let client = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(30))
            .build::<_, Body>(connector);

        Arc::new(Self {
            routes: RwLock::new(cfg.routes.clone()),
            default_port: cfg.default_port,
            client,
            timeout: (cfg.timeout_ms > 0).then(|| Duration::from_millis(cfg.timeout_ms)),
            listens,
            counters: Counters::default(),
        })
    }

    /// 判定一个 `Host` 该去哪个端口。除读路由表外无副作用，所以路由语义
    /// 可以脱离网络单独测。
    pub fn resolve(&self, host: &str) -> Resolution {
        let table = self.routes.read().unwrap_or_else(|e| e.into_inner());

        // 完整域名精确匹配优先于"第一段"匹配：允许同一前缀在不同域名下
        // 指向不同端口（`cad.a.com` -> 2228、`cad.b.com` -> 2229）。
        let hostname = normalize_host(host);
        if !hostname.is_empty() {
            if let Some(port) = table.get(&hostname) {
                return Resolution::Backend { port: *port, matched: hostname };
            }
        }
        let prefix = first_label(host);
        if let Some(prefix) = &prefix {
            if let Some(port) = table.get(prefix) {
                return Resolution::Backend { port: *port, matched: prefix.clone() };
            }
        }
        match self.default_port {
            Some(port) => Resolution::Default { port },
            None => Resolution::Unrouted { prefix },
        }
    }

    pub fn routes(&self) -> Vec<RouteView> {
        self.routes
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(prefix, port)| RouteView { prefix: prefix.clone(), port: *port })
            .collect()
    }

    pub fn upsert_route(&self, prefix: &str, port: u16) -> Result<RouteView, ControlError> {
        let prefix = prefix.trim().to_ascii_lowercase();
        let ok = !prefix.is_empty()
            && prefix.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.')
            })
            && !prefix.starts_with('.')
            && !prefix.ends_with('.');
        if !ok {
            return Err(ControlError::invalid(format!(
                "前缀 {prefix:?} 只能含小写字母/数字/-/_/.，且不能以点开头结尾"
            )));
        }
        if port == 0 {
            return Err(ControlError::invalid("端口不能是 0"));
        }
        self.routes
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(prefix.clone(), port);
        Ok(RouteView { prefix, port })
    }

    pub fn delete_route(&self, prefix: &str) -> Result<(), ControlError> {
        let prefix = prefix.trim().to_ascii_lowercase();
        let removed = self.routes.write().unwrap_or_else(|e| e.into_inner()).remove(&prefix);
        match removed {
            Some(_) => Ok(()),
            None => Err(ControlError::not_found(format!("没有这个前缀：{prefix}"))),
        }
    }

    pub fn default_port(&self) -> Option<u16> {
        self.default_port
    }

    pub fn listen_addrs(&self) -> &[SocketAddr] {
        &self.listens
    }

    pub fn counters(&self) -> &Counters {
        &self.counters
    }

    pub fn status(&self) -> RouterStatus {
        RouterStatus {
            listens: self.listens.iter().map(|a| a.to_string()).collect(),
            routes: self.routes(),
            default_port: self.default_port,
            counters: self.counters.snapshot(),
        }
    }

    /// 实际转发。
    ///
    /// 错误语义（都写成 JSON 响应体，`curl` 一眼能看出问题）：
    /// - 前缀没配 → `421 no_route`
    /// - 请求要 Upgrade（WebSocket）→ `501 upgrade_not_supported`（v1 不做，
    ///   见 `docs/EDGE-ROUTER.md`）
    /// - 后端连不上 → `502 bad_gateway`；超时 → `504 upstream_timeout`
    async fn forward(
        self: Arc<Self>,
        req: Request,
        port: u16,
        matched: String,
        peer: Option<SocketAddr>,
    ) -> Response {
        let (parts, body) = req.into_parts();

        // Upgrade 必须在动 body 之前判断：那种请求的 body 是空壳，真正的
        // 数据在 101 之后走 TCP。v1 明确拒绝（501）而不是挂着不动。
        if parts.headers.contains_key(header::UPGRADE) {
            warn!(prefix = %matched, "拒绝 Upgrade 请求（edge router v1 不支持 WebSocket 透传）");
            self.counters.record_upgrade_rejected();
            return json_error(
                StatusCode::NOT_IMPLEMENTED,
                "upgrade_not_supported",
                "edge router v1 不支持 WebSocket/Upgrade 透传；该路径请走端口直连（v2 计划支持）",
                serde_json::json!({ "prefix": matched, "route": port }),
            );
        }

        let path = parts
            .uri
            .path_and_query()
            .map(|pq| pq.as_str().to_string())
            .unwrap_or_else(|| "/".to_string());
        // 用 127.0.0.1 而不是 localhost：省掉一次可能出问题的 DNS 解析。
        let upstream_uri: Uri = match format!("http://127.0.0.1:{port}{path}").parse() {
            Ok(u) => u,
            Err(e) => {
                self.counters.record_upstream_error();
                return json_error(
                    StatusCode::BAD_GATEWAY,
                    "bad_request_uri",
                    &format!("无法构造上游 URI: {e}"),
                    serde_json::json!({ "prefix": matched, "route": port }),
                );
            }
        };

        let mut builder = Request::builder().method(parts.method.clone()).uri(upstream_uri);
        {
            let headers = builder
                .headers_mut()
                .expect("builder 刚创建，headers 一定存在");
            copy_end_to_end(&parts.headers, headers);
            // 保留原始 Host（含端口）：后端可能用它拼绝对链接。
            if let Some(host) = parts.headers.get(header::HOST) {
                headers.insert(header::HOST, host.clone());
            }
            if let Some(peer) = peer {
                let forwarded = match parts
                    .headers
                    .get("x-forwarded-for")
                    .and_then(|v| v.to_str().ok())
                {
                    Some(existing) if !existing.is_empty() => format!("{existing}, {}", peer.ip()),
                    _ => peer.ip().to_string(),
                };
                if let Ok(v) = HeaderValue::from_str(&forwarded) {
                    headers.insert(HeaderName::from_static("x-forwarded-for"), v);
                }
                if let Ok(v) = HeaderValue::from_str(&peer.port().to_string()) {
                    headers.insert(HeaderName::from_static("x-forwarded-port"), v);
                }
            }
            if let Some(host) = parts.headers.get(header::HOST).and_then(|v| v.to_str().ok()) {
                if let Ok(v) = HeaderValue::from_str(host) {
                    headers.insert(HeaderName::from_static("x-forwarded-host"), v);
                }
            }
            headers.insert(
                HeaderName::from_static("x-forwarded-proto"),
                HeaderValue::from_static("http"),
            );
        }

        let upstream_req = match builder.body(body) {
            Ok(r) => r,
            Err(e) => {
                self.counters.record_upstream_error();
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "bad_request",
                    &format!("无法构造上游请求: {e}"),
                    serde_json::json!({ "prefix": matched, "route": port }),
                );
            }
        };

        let result = match self.timeout {
            Some(t) => match tokio::time::timeout(t, self.client.request(upstream_req)).await {
                Ok(r) => r,
                Err(_) => {
                    self.counters.record_upstream_error();
                    warn!(prefix = %matched, port, "上游超时");
                    return json_error(
                        StatusCode::GATEWAY_TIMEOUT,
                        "upstream_timeout",
                        &format!(
                            "后端 127.0.0.1:{port} 在 {}ms 内没有响应",
                            t.as_millis()
                        ),
                        serde_json::json!({ "prefix": matched, "route": port }),
                    );
                }
            },
            None => self.client.request(upstream_req).await,
        };

        match result {
            Ok(resp) => {
                self.counters.record_proxied();
                let (rparts, rbody) = resp.into_parts();
                let mut builder = Response::builder().status(rparts.status);
                {
                    let headers = builder
                        .headers_mut()
                        .expect("builder 刚创建，headers 一定存在");
                    copy_end_to_end(&rparts.headers, headers);
                }
                builder.body(Body::new(rbody)).unwrap_or_else(|_| {
                    json_error(
                        StatusCode::BAD_GATEWAY,
                        "bad_response",
                        "无法构造响应",
                        serde_json::json!({ "prefix": matched }),
                    )
                })
            }
            Err(e) => {
                self.counters.record_upstream_error();
                debug!(prefix = %matched, port, error = %e, "上游请求失败");
                json_error(
                    StatusCode::BAD_GATEWAY,
                    "bad_gateway",
                    &format!("后端 127.0.0.1:{port} 不可达: {e}"),
                    serde_json::json!({ "prefix": matched, "route": port }),
                )
            }
        }
    }
}

/// `Host` 转成查表用的完整域名键：小写、去端口、去末尾的点。
/// 纯 IP / 单段时返回空串（表示"不是一个域名"）。
fn normalize_host(host: &str) -> String {
    let host = host.trim();
    if host.is_empty() || host.starts_with('[') {
        return String::new();
    }
    let host = host.split(':').next().unwrap_or(host);
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.parse::<std::net::Ipv4Addr>().is_ok() || !host.contains('.') {
        return String::new();
    }
    host
}

/// 复制端到端首部：跳过逐跳首部、以及 `Connection` 里点名的那几个。
fn copy_end_to_end(src: &HeaderMap, dst: &mut HeaderMap) {
    // `Connection: foo, bar` 里点名的首部同样是逐跳的，必须一起丢
    // （`x-secret` 这种"只在连接内向一跳说话"的首部不能漏出去）。
    let extra: Vec<String> = src
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();

    for (name, value) in src.iter() {
        let lower = name.as_str();
        if HOP_BY_HOP.contains(&lower) || REWRITTEN.contains(&lower) {
            continue;
        }
        if extra.iter().any(|e| e == lower) {
            continue;
        }
        // 多值首部（如 Set-Cookie）逐条 append，不能只留第一个。
        dst.append(name.clone(), value.clone());
    }
}

fn json_error(status: StatusCode, code: &str, message: &str, extra: serde_json::Value) -> Response {
    let mut body = serde_json::Map::new();
    body.insert("error".into(), serde_json::Value::String(code.into()));
    body.insert("message".into(), serde_json::Value::String(message.into()));
    if let serde_json::Value::Object(m) = extra {
        for (k, v) in m {
            body.insert(k, v);
        }
    }
    let body = serde_json::Value::Object(body).to_string();
    (
        status,
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        body,
    )
        .into_response()
}

// ─────────────────────────────────────────────────────────────────────────────
// axum 入口
// ─────────────────────────────────────────────────────────────────────────────

async fn handle(State(router): State<Arc<EdgeRouter>>, req: Request) -> Response {
    router.counters().record_request();
    // ConnectInfo 由 `into_make_service_with_connect_info` 注入 extensions；
    // 直接读 extensions 而不是用 Option<ConnectInfo<_>> 提取器，是为了在
    // "没有注入"（单测直接调 handler）时也能工作，而不是 500。
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0);
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    match router.resolve(&host) {
        Resolution::Backend { port, matched } => {
            debug!(host = %host, prefix = %matched, port, "代理");
            router.forward(req, port, matched, peer).await
        }
        Resolution::Default { port } => {
            router.forward(req, port, "<default>".into(), peer).await
        }
        Resolution::Unrouted { prefix } => {
            router.counters().record_unrouted();
            json_error(
                StatusCode::MISDIRECTED_REQUEST,
                "no_route",
                "该域名前缀没有配置路由。查 edge_router.routes，或经 EdgeRouterControl 加一条。",
                serde_json::json!({ "host": host, "prefix": prefix }),
            )
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 生命周期
// ─────────────────────────────────────────────────────────────────────────────

/// 已启动的边缘路由。`shutdown()` 走 axum 的优雅关闭（在途请求跑完）。
pub struct ServerHandle {
    addrs: Vec<SocketAddr>,
    router: Arc<EdgeRouter>,
    shutdown: tokio::sync::watch::Sender<bool>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl ServerHandle {
    pub fn addrs(&self) -> &[SocketAddr] {
        &self.addrs
    }

    pub fn router(&self) -> Arc<EdgeRouter> {
        Arc::clone(&self.router)
    }

    /// 停止接收新连接，等在途连接跑完。
    pub async fn shutdown(mut self) {
        let _ = self.shutdown.send(true);
        for t in self.tasks.drain(..) {
            let _ = t.await;
        }
    }
}

/// 先同步 bind 完所有监听地址，再交给异步任务 serve。
///
/// 为什么在 mount 里同步 bind：`mount()` 是同步签名，而 bind 失败是**必须
/// 让宿主知道**的错误（端口被占、没有绑特权端口的权限）。如果像隧道插件
/// 那样把启动整个塞进 spawn，mount 只能报"成功"，错误要等到第一次请求才
/// 暴露。bind 本身是微秒级操作，在 async 上下文里阻塞这一下是可接受的代价。
pub fn bind(cfg: &RouterConfig) -> Result<Vec<std::net::TcpListener>, ControlError> {
    let mut listeners = Vec::with_capacity(cfg.listen.len());
    for addr in &cfg.listen {
        let sock: SocketAddr = addr
            .parse()
            .map_err(|e| ControlError::invalid(format!("listen {addr:?} 解析失败: {e}")))?;
        match std::net::TcpListener::bind(sock) {
            Ok(l) => {
                // `TcpListener::from_std` 要求非阻塞；先设好，后面直接接管。
                if let Err(e) = l.set_nonblocking(true) {
                    return Err(ControlError::other(format!(
                        "listen {addr} 设置非阻塞失败: {e}"
                    )));
                }
                listeners.push(l);
            }
            Err(e) => {
                return Err(ControlError::bind(format!(
                    "无法监听 {addr}: {e}（端口被占用，或需要 root/CAP_NET_BIND_SERVICE 才能绑 <1024 端口）"
                )))
            }
        }
    }
    Ok(listeners)
}

/// 把 bind 好的监听交给后台任务，返回可关闭的句柄。
pub fn spawn(
    router: Arc<EdgeRouter>,
    listeners: Vec<std::net::TcpListener>,
) -> Result<ServerHandle, ControlError> {
    let rt = tokio::runtime::Handle::try_current()
        .map_err(|_| ControlError::other("edge router 需要 tokio 运行时"))?;
    let (tx, _) = tokio::sync::watch::channel(false);
    let mut addrs = Vec::with_capacity(listeners.len());
    let mut tasks = Vec::with_capacity(listeners.len());

    for std_listener in listeners {
        let addr = std_listener
            .local_addr()
            .map_err(|e| ControlError::other(format!("读取监听地址失败: {e}")))?;
        let listener = TcpListener::from_std(std_listener)
            .map_err(|e| ControlError::other(format!("接管监听 {addr} 失败: {e}")))?;
        addrs.push(addr);

        let app = axum::Router::new()
            .fallback(handle)
            .with_state(Arc::clone(&router));
        let mut rx = tx.subscribe();
        let task = rt.spawn(async move {
            let served = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                // 初始值是 false；只有收到 true 才退出。
                while rx.changed().await.is_ok() {
                    if *rx.borrow() {
                        return;
                    }
                }
            })
            .await;
            if let Err(e) = served {
                warn!(%addr, error = %e, "edge router 监听退出");
            } else {
                debug!(%addr, "edge router 监听已关闭");
            }
        });
        tasks.push(task);
    }

    Ok(ServerHandle { addrs, router, shutdown: tx, tasks })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn router_with(routes: &[(&str, u16)], default: Option<u16>) -> Arc<EdgeRouter> {
        let cfg = RouterConfig {
            listen: vec!["127.0.0.1:0".into()],
            routes: routes.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            default_port: default,
            timeout_ms: 5_000,
        };
        EdgeRouter::new(&cfg, vec![])
    }

    #[test]
    fn first_label_handles_the_shapes_we_actually_see() {
        assert_eq!(first_label("cad.example.com").as_deref(), Some("cad"));
        assert_eq!(first_label("cad.example.com:2229").as_deref(), Some("cad"));
        assert_eq!(first_label("CAD.Example.COM.").as_deref(), Some("cad"));
        assert_eq!(first_label("cad.114.132.241.46.nip.io").as_deref(), Some("cad"));
        // 没有前缀的形态必须是 None，否则纯 IP 会被当成前缀 114。
        assert_eq!(first_label("114.132.241.46"), None);
        assert_eq!(first_label("114.132.241.46:2229"), None);
        assert_eq!(first_label("[::1]:8080"), None);
        assert_eq!(first_label("localhost"), None);
        assert_eq!(first_label(""), None);
        assert_eq!(first_label("   "), None);
    }

    #[test]
    fn prefix_match_beats_nothing_and_default_is_the_fallback() {
        let r = router_with(&[("cad", 2228), ("panel", 8080)], None);
        assert_eq!(
            r.resolve("cad.whatever.cn"),
            Resolution::Backend { port: 2228, matched: "cad".into() }
        );
        assert_eq!(
            r.resolve("CAD.WHATEVER.CN:2229"),
            Resolution::Backend { port: 2228, matched: "cad".into() }
        );
        assert_eq!(
            r.resolve("nope.whatever.cn"),
            Resolution::Unrouted { prefix: Some("nope".into()) }
        );
        // 纯 IP 没有前缀，仍然是 Unrouted（不会被误判成 114）。
        assert_eq!(r.resolve("114.132.241.46"), Resolution::Unrouted { prefix: None });
    }

    #[test]
    fn full_domain_key_wins_over_prefix_key() {
        // 同一前缀在不同域名下可以指向不同端口。
        let r = router_with(&[("cad", 2228), ("cad.special.com", 9999)], None);
        assert_eq!(
            r.resolve("cad.special.com"),
            Resolution::Backend { port: 9999, matched: "cad.special.com".into() }
        );
        assert_eq!(
            r.resolve("cad.other.com"),
            Resolution::Backend { port: 2228, matched: "cad".into() }
        );
    }

    #[test]
    fn default_port_only_catches_unrouted_hosts() {
        let r = router_with(&[("cad", 2228)], Some(9999));
        assert_eq!(
            r.resolve("cad.x.com"),
            Resolution::Backend { port: 2228, matched: "cad".into() }
        );
        assert_eq!(r.resolve("x.com"), Resolution::Default { port: 9999 });
        assert_eq!(r.resolve("1.2.3.4"), Resolution::Default { port: 9999 });
    }

    #[test]
    fn routes_can_be_added_and_removed_at_runtime() {
        let r = router_with(&[], None);
        assert!(matches!(r.resolve("cad.x.com"), Resolution::Unrouted { .. }));
        r.upsert_route("cad", 2228).unwrap();
        assert_eq!(
            r.resolve("cad.x.com"),
            Resolution::Backend { port: 2228, matched: "cad".into() }
        );
        r.delete_route("cad").unwrap();
        assert!(matches!(r.resolve("cad.x.com"), Resolution::Unrouted { .. }));
        // 删不存在的要报 not_found（而不是静默成功）。
        let err = r.delete_route("cad").unwrap_err();
        assert_eq!(err.kind, esc_control_api::ErrorKind::NotFound);
    }

    #[test]
    fn upsert_rejects_garbage_prefixes() {
        let r = router_with(&[], None);
        for bad in ["", "  ", "a b", "*.x", ".cad", "cad."] {
            assert!(r.upsert_route(bad, 2228).is_err(), "应拒绝 {bad:?}");
        }
        assert!(r.upsert_route("cad", 0).is_err());
    }

    #[test]
    fn hop_by_hop_and_forwarded_headers_are_dropped() {
        let mut src = HeaderMap::new();
        src.insert(header::CONNECTION, HeaderValue::from_static("keep-alive, x-secret"));
        src.insert("x-secret", HeaderValue::from_static("must-not-leak"));
        src.insert("keep-alive", HeaderValue::from_static("timeout=5"));
        src.insert("x-forwarded-for", HeaderValue::from_static("1.2.3.4"));
        src.insert("x-keep", HeaderValue::from_static("yes"));
        let mut dst = HeaderMap::new();
        copy_end_to_end(&src, &mut dst);
        assert!(!dst.contains_key("x-secret"), "Connection 点名的首部必须丢");
        assert!(!dst.contains_key("keep-alive"));
        assert!(!dst.contains_key("x-forwarded-for"), "网关自己重算");
        assert_eq!(dst.get("x-keep").unwrap(), "yes");

        // 多值首部要逐条保留（Set-Cookie 是最常见的坑）。
        let mut src = HeaderMap::new();
        src.append(header::SET_COOKIE, HeaderValue::from_static("a=1"));
        src.append(header::SET_COOKIE, HeaderValue::from_static("b=2"));
        let mut dst = HeaderMap::new();
        copy_end_to_end(&src, &mut dst);
        assert_eq!(dst.get_all(header::SET_COOKIE).iter().count(), 2);
    }
}
