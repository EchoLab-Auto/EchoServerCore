//! 转发核心：一个最小但正确的 HTTP/1.1 反向代理。
//!
//! # 为什么不用现成的代理中间件
//!
//! axum 生态里没有一个「开箱即用且行为可预期」的反向代理。`hyper-reverse-proxy`
//! 长期没维护；`tower-http` 的 proxy 层仍在演进。而代理是那种「看起来简单、
//! 细节全是坑」的组件，行为必须完全可解释——所以这里手写，每个决策都写下来。
//!
//! # 明确不支持
//!
//! * **WebSocket / Upgrade**：返回 501。面板用不到，而支持它要额外处理连接
//!   升级、协议切换、双向流复制——占整个代理一半的复杂度。将来真需要再加。
//! * **HTTP/2**：上游和下游都是 h1。面板不需要多路复用，明文 h2 还需要
//!   prior-knowledge 协商。
//! * **gzip 重压/解压**：原样透传 `Accept-Encoding` 与 `Content-Encoding`。
//! * **重定向跟随**：代理不该跟随重定向，那是客户端的事。
//!
//! # 每个请求经过的步骤
//!
//! ```text
//! accept → 来源白名单 → 登录态/权限 → 登录端点？ → 剥 hop-by-hop 头
//!        → 重写 Host/路径 → 连上游 → 转发 → 透传响应（剥 hop-by-hop）
//! ```

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use bytes::Bytes;
use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request as HyperRequest, Response as HyperResponse};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

use crate::auth::{self, AuthState, Denied};
use crate::config::{RouteRuntime, RuntimeConfig, UserRuntime};
use crate::routes::{EffectiveRoute, ListenerSet, RouteRegistry, RouteSource, RouteStore};
use crate::store::AccountStore;
use crate::web;

/// 上游连接超时。本地回环，1 秒都算慢。
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// 单个请求从收到到上游返回响应头的上限。
///
/// 不覆盖「读响应体」的时长：日志接口会流式返回几百 KB，那是正常的。
const UPSTREAM_HEAD_TIMEOUT: Duration = Duration::from_secs(60);

pub type BoxErr = Box<dyn std::error::Error + Send + Sync>;
pub type ProxyBody = http_body_util::combinators::BoxBody<Bytes, BoxErr>;

/// 网关的共享状态。
#[derive(Debug)]
pub struct Gateway {
    pub cfg: Arc<RuntimeConfig>,
    pub auth: Arc<AuthState>,
    /// 面板管理的账号（叠加在 `cfg.users` 之上）。
    pub store: Arc<AccountStore>,
    /// 面板管理的路由（叠加在 `cfg.routes` 之上）。
    pub rt_store: Arc<RouteStore>,
    /// **当前生效**的路由。每个请求都从这里读一次，所以改上游不用重启。
    pub routes: RouteRegistry,
    /// 正在监听的端口，支持热插拔。
    pub listeners: ListenerSet,
}

/// 账号是从哪来的。面板上必须标出来——「这条改了不生效」全靠它解释。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountSource {
    /// 来自 `gateway.toml`，面板只读。
    Config,
    /// 来自账号库，面板可改。
    Store,
    /// 账号库里有一条同名的，把配置文件里的那条覆盖了。
    StoreOverConfig,
}

impl AccountSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Config => "配置文件（只读）",
            Self::Store => "账号库",
            Self::StoreOverConfig => "账号库（覆盖了配置文件）",
        }
    }
    pub fn editable(self) -> bool {
        !matches!(self, Self::Config)
    }
}

/// 一条**生效**的账号。
#[derive(Debug, Clone)]
pub struct Account {
    pub user: UserRuntime,
    pub source: AccountSource,
}

impl Gateway {
    pub fn new(cfg: Arc<RuntimeConfig>) -> Self {
        let auth = Arc::new(AuthState::new(&cfg));
        let store = Arc::new(AccountStore::load(cfg.users_file.as_deref()));
        // 账号到底存在哪个文件里，是排查「改了没生效」时第一个要确认的事，
        // 所以启动时就打出来，而不是等出问题再猜。
        match (store.path(), store.writable()) {
            (Some(p), true) => tracing::info!(
                target: "esc_gateway",
                "账号库：{}（可写，{} 个账号）", p.display(), store.count()
            ),
            (Some(p), false) => tracing::warn!(
                target: "esc_gateway",
                "账号库：{}（只读，{} 个账号）", p.display(), store.count()
            ),
            (None, _) => tracing::info!(
                target: "esc_gateway",
                "未配置 users_file：管理面板只读，账号只来自配置文件"
            ),
        }
        if let Some(note) = store.note() {
            tracing::warn!(target: "esc_gateway", "{note}");
        }
        for r in store.rejected() {
            tracing::error!(target: "esc_gateway", "账号库里的条目被拒绝：{r}");
        }
        // 同名覆盖必须吵一次：否则「改了 gateway.toml 里的口令却不生效」
        // 会变成一次很贵的排查。
        for u in store.all() {
            if cfg.users.contains_key(&u.name) {
                tracing::warn!(
                    target: "esc_gateway",
                    user = %u.name,
                    "账号库里有同名账号，已覆盖 gateway.toml 中的定义"
                );
            }
        }
        let rt_store = Arc::new(RouteStore::load(cfg.routes_file.as_deref()));
        match (rt_store.path(), rt_store.writable()) {
            (Some(p), true) => tracing::info!(
                target: "esc_gateway",
                "路由库：{}（可写，{} 条）", p.display(), rt_store.count()
            ),
            (Some(p), false) => tracing::warn!(
                target: "esc_gateway",
                "路由库：{}（只读，{} 条）", p.display(), rt_store.count()
            ),
            (None, _) => tracing::info!(
                target: "esc_gateway",
                "未配置 routes_file：端口只能在配置文件里改"
            ),
        }
        if let Some(note) = rt_store.note() {
            tracing::warn!(target: "esc_gateway", "{note}");
        }
        for r in rt_store.rejected() {
            tracing::error!(target: "esc_gateway", "路由库里的条目被拒绝：{r}");
        }
        for r in rt_store.all() {
            if cfg.routes.contains_key(&r.name) {
                tracing::warn!(
                    target: "esc_gateway",
                    route = %r.name,
                    "路由库里有同名路由，已覆盖 gateway.toml 中的定义"
                );
            }
        }

        // 初始注册表 = 配置文件 ∪ 路由库。合并结果必须整体合法（端口冲突、
        // 名字重复、自环都在这里一次性暴露），否则启动就报错而不是等到访问。
        let mut initial: Vec<_> = cfg.routes.values().cloned().collect();
        for r in rt_store.all() {
            initial.retain(|x| x.name != r.name);
            if let Ok(rt) = r.to_runtime() {
                initial.push(rt);
            }
        }
        let registry = RouteRegistry::new(initial);

        Self {
            cfg,
            auth,
            store,
            rt_store,
            routes: registry,
            listeners: ListenerSet::new(),
        }
    }

    pub fn title(&self) -> &str {
        &self.cfg.title
    }

    /// 解析**生效**的账号：账号库优先，配置文件兜底。
    ///
    /// 兜底顺序是刻意的：账号库丢了 / 坏了 / 卷没挂上，配置文件里的账号照样
    /// 能登进来修，不会把自己锁在外面。
    pub fn user(&self, name: &str) -> Option<UserRuntime> {
        if let Some(u) = self.store.get(name) {
            return Some(u.to_runtime());
        }
        self.cfg.user(name).cloned()
    }

    /// 生效账号全集（配置文件 ∪ 账号库），按名字排序。
    pub fn accounts(&self) -> Vec<Account> {
        let stored: std::collections::BTreeMap<String, UserRuntime> = self
            .store
            .all()
            .into_iter()
            .map(|u| (u.name.clone(), u.to_runtime()))
            .collect();

        let mut out: Vec<Account> = Vec::new();
        for (name, user) in &stored {
            let source = if self.cfg.users.contains_key(name) {
                AccountSource::StoreOverConfig
            } else {
                AccountSource::Store
            };
            out.push(Account {
                user: user.clone(),
                source,
            });
        }
        for (name, user) in &self.cfg.users {
            if !stored.contains_key(name) {
                out.push(Account {
                    user: user.clone(),
                    source: AccountSource::Config,
                });
            }
        }
        out.sort_by(|a, b| a.user.name.cmp(&b.user.name));
        out
    }

    // ── 路由 ──────────────────────────────────────────────────────────────

    /// 生效路由全集（配置文件 ∪ 路由库），按名字排序。面板用。
    pub fn effective_routes(&self) -> Vec<EffectiveRoute> {
        let stored: std::collections::BTreeMap<String, crate::config::RouteConfig> = self
            .rt_store
            .all()
            .into_iter()
            .map(|r| (r.name.clone(), r))
            .collect();

        let mut out: Vec<EffectiveRoute> = Vec::new();
        for (name, spec) in &stored {
            let source = if self.cfg.routes.contains_key(name) {
                RouteSource::StoreOverConfig
            } else {
                RouteSource::Store
            };
            out.push(EffectiveRoute {
                spec: spec.clone(),
                source,
            });
        }
        for (name, rt) in &self.cfg.routes {
            if stored.contains_key(name) {
                continue;
            }
            out.push(EffectiveRoute {
                spec: crate::config::RouteConfig {
                    name: rt.name.clone(),
                    listen: rt.listen.to_string(),
                    upstream: rt.upstream.to_string(),
                    allowed_cidrs: rt.allowed_cidrs_raw.clone(),
                    strip_prefix: rt.strip_prefix.clone(),
                    max_body_bytes: Some(rt.max_body_bytes),
                },
                source: RouteSource::Config,
            });
        }
        out.sort_by(|a, b| a.spec.name.cmp(&b.spec.name));
        out
    }

    /// 生效的路由名（面板里画成复选框、校验引用时用）。
    pub fn route_names(&self) -> Vec<String> {
        self.routes.names()
    }

    /// 用一份候选路由集合去算"合并后的完整列表"。
    ///
    /// 纯函数：不写盘、不绑端口，只回答"如果路由库变成这样，最终会监听哪些
    /// 端口、会不会冲突"。两阶段热应用的第一步就是它。
    pub fn candidate_runtimes(
        &self,
        pending: &std::collections::BTreeMap<String, crate::config::RouteConfig>,
    ) -> Result<Vec<crate::config::RouteRuntime>, String> {
        let specs: Vec<crate::config::RouteConfig> = self
            .effective_routes()
            .into_iter()
            .filter(|r| !pending.contains_key(&r.spec.name))
            .map(|r| r.spec)
            .chain(pending.values().cloned())
            .collect();
        crate::config::validate_route_list(&specs)
    }

    /// 热应用第一步：算合并结果 + **现绑新端口**。
    ///
    /// 纯失败无副作用：算不通或绑不上，返回 `Err`，旧监听一个都不动。
    pub async fn prepare_routes(
        &self,
        pending: &std::collections::BTreeMap<String, crate::config::RouteConfig>,
    ) -> Result<(Vec<crate::config::RouteRuntime>, crate::routes::Prepared), String> {
        let runtimes = self.candidate_runtimes(pending)?;
        let prepared = self.listeners.prepare(&runtimes).await?;
        Ok((runtimes, prepared))
    }

    /// 热应用第二步：换注册表（上游 / 前缀 / 白名单立刻生效）并起新监听、停旧监听。
    ///
    /// 调用它之前必须先成功写盘——否则会出现"内存里改了、文件里没有"，
    /// 下次重启就静默回退。
    pub fn commit_routes(
        self: &Arc<Self>,
        runtimes: Vec<crate::config::RouteRuntime>,
        prepared: crate::routes::Prepared,
    ) {
        self.routes.replace_all(runtimes);
        self.listeners.commit(self, prepared);
    }

    /// 生效端口数（首页与面板都用）。
    pub fn route_count(&self) -> usize {
        self.routes.count()
    }

    /// 生效账号里的管理员数量。删账号 / 降权限前必须看这个。
    pub fn admin_count(&self) -> usize {
        self.accounts().iter().filter(|a| a.user.admin).count()
    }
}

/// 管理端口的地址前缀：`""`（就是这个端口）或 `"//host:port"`。
///
/// 主机名取请求的 `Host`（去端口）而不是配置里的 IP：配置里通常写
/// `127.0.0.1` 或 `0.0.0.0`，前者对远程浏览器没意义、后者不能访问。
///
/// 用协议相对的 `//host:port`，这样前面挂 TLS 也不用改。
fn admin_base(admin: SocketAddr, host: &str) -> String {
    if host.is_empty() {
        format!(":{}", admin.port())
    } else {
        format!("//{host}:{}", admin.port())
    }
}

/// 管理端口的 accept loop。
///
/// 它**不转发**任何东西：只提供 `/__auth/*`（登录/登出/探活/首页）和
/// `/__admin/*`（管理面板）。其余路径给一页说明。
pub async fn serve_admin(
    gw: Arc<Gateway>,
    addr: SocketAddr,
    listener: tokio::net::TcpListener,
    shutdown: CancellationToken,
) {
    tracing::info!(listen = %addr, "admin panel listening");
    loop {
        let (stream, peer) = tokio::select! {
            _ = shutdown.cancelled() => break,
            r = listener.accept() => match r {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "admin accept failed");
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    continue;
                }
            },
        };
        let _ = stream.set_nodelay(true);

        let gw2 = gw.clone();
        let shut = shutdown.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let service = service_fn(move |req: HyperRequest<Incoming>| {
                let gw = gw2.clone();
                async move { Ok::<_, std::convert::Infallible>(handle_admin(gw, peer, req).await) }
            });
            let conn = hyper::server::conn::http1::Builder::new()
                .keep_alive(true)
                .serve_connection(io, service);
            tokio::select! {
                r = conn => {
                    if let Err(e) = r {
                        tracing::debug!(%peer, error = %e, "admin connection closed");
                    }
                }
                _ = shut.cancelled() => {}
            }
        });
    }
    tracing::info!(%addr, "admin panel stopped");
}

async fn handle_admin(
    gw: Arc<Gateway>,
    peer: SocketAddr,
    req: HyperRequest<Incoming>,
) -> HyperResponse<ProxyBody> {
    let source = resolve_source(&gw, &req, peer);
    let local = gw.cfg.admin_listen.unwrap_or(peer);
    let path = req.uri().path().to_string();

    if path == "/__auth" || path.starts_with("/__auth/") {
        return gateway_endpoint(&gw, None, source, req).await;
    }
    if path == "/__admin" || path.starts_with("/__admin/") {
        return crate::admin::handle(&gw, source, local, req).await;
    }
    // 管理端口不转发。给一页"这里能干什么"，别让 / 变成 404。
    home(&gw, None, req.headers())
}

/// 在某个监听地址上起 accept loop。
///
/// `route` 只是**起监听那一刻**的快照，用来打日志和兜底：真正的定义在每个
/// 请求里按 `local_addr` 从注册表重读一次（见 [`handle`]），所以改了上游、
/// 前缀、白名单不需要重连，下一个请求就生效。
pub async fn serve_route(
    gw: Arc<Gateway>,
    route: Arc<RouteRuntime>,
    listener: tokio::net::TcpListener,
    shutdown: CancellationToken,
) {
    let local = listener
        .local_addr()
        .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0)));
    tracing::info!(
        route = %route.name,
        listen = %local,
        upstream = %route.upstream,
        allowed_cidrs = route.allow.len(),
        "route listening"
    );

    loop {
        let (stream, peer) = tokio::select! {
            _ = shutdown.cancelled() => break,
            r = listener.accept() => match r {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "accept failed");
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    continue;
                }
            },
        };
        // 小延迟缓解 accept 风暴（对方 SYN 洪水时不要把 CPU 打满）
        let _ = stream.set_nodelay(true);

        let gw2 = gw.clone();
        let route2 = route.clone();
        let shut = shutdown.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let service = service_fn(move |req: HyperRequest<Incoming>| {
                let gw = gw2.clone();
                let route = route2.clone();
                async move {
                    Ok::<_, std::convert::Infallible>(handle(gw, route, local, peer, req).await)
                }
            });
            let conn = hyper::server::conn::http1::Builder::new()
                .keep_alive(true)
                // 只允许 h1；不看 Upgrade。
                .serve_connection(io, service);
            tokio::select! {
                r = conn => {
                    if let Err(e) = r {
                        // 客户端半路断开是常态，不值得 warn
                        tracing::debug!(%peer, error = %e, "connection closed");
                    }
                }
                _ = shut.cancelled() => {}
            }
        });
    }
    tracing::info!(route = %route.name, "route stopped");
}

// ─────────────────────────────────────────────────────────────────────────────
// 单请求处理
// ─────────────────────────────────────────────────────────────────────────────

async fn handle(
    gw: Arc<Gateway>,
    snapshot: Arc<RouteRuntime>,
    local: SocketAddr,
    peer: SocketAddr,
    req: HyperRequest<Incoming>,
) -> HyperResponse<ProxyBody> {
    // 每个请求重读一次路由定义：改上游 / 前缀 / 白名单不用重启、不用重连。
    // 按 `local_addr` 查而不是按名字：路由改名之后老连接也能立刻用上新定义。
    let Some(route) = gw.routes.at(local) else {
        // 这个端口已经被删掉了。连接可能还没被 cancel 打断，所以自己收尾。
        return text(StatusCode::SERVICE_UNAVAILABLE, "这个端口已被移除");
    };
    let _ = &snapshot;
    let source = resolve_source(&gw, &req, peer);

    // ① 来源白名单（route 级）。放在认证之前：白名单外的人连登录页都看不到。
    if !route.source_allowed(source) {
        tracing::warn!(route = %route.name, source = ?source, "source not in allow_cidrs");
        return text(StatusCode::FORBIDDEN, "来源地址不在白名单内");
    }

    let path = req.uri().path().to_string();

    // ② 网关自己的端点（不转发）。`/__auth` 与 `/__auth/*`：登录/登出/探活/首页。
    //    注意 `/__auth`（不带斜杠）必须单独判一次——`starts_with("/__auth/")`
    //    对它是 false，于是它会一路转发到上游，变成一个「网关首页 404 或者
    //    上游页面」的诡异现象。
    if path == "/__auth" || path.starts_with("/__auth/") {
        return gateway_endpoint(&gw, Some(&route), source, req).await;
    }

    // ②b 管理面板。它要自己判登录态（普通用户也能进「我的账号」），
    //     所以不能走下面的通用认证流程。
    //
    //     配了 `admin_listen` 之后这个路径**不再由网关占用**：它就是一个普通
    //     路径，照常认证后转发给上游（上游没有它，于是 404）。
    //
    //     为什么不再给一页「管理面板在别处」+ 链接：那等于在每个公网端口上
    //     公开管理入口的位置。用户本来就是从业务端口敲过来的，指路的价值远
    //     不如"别告诉任何人管理面在哪"。
    if gw.cfg.admin_listen.is_none() && (path == "/__admin" || path.starts_with("/__admin/")) {
        return crate::admin::handle(&gw, source, local, req).await;
    }

    // ③ 登录态 + 权限
    let sid = auth::cookie_value(req.headers(), &gw.cfg.cookie_name);
    let session = sid.and_then(|s| gw.auth.get(s));
    let wants_html = auth::wants_html(req.headers());

    match session {
        None => {
            if wants_html {
                let target = format!(
                    "/__auth/login?next={}",
                    urlencode(&format!("{path}{}", query_suffix(&req)))
                );
                return redirect(StatusCode::FOUND, &target);
            }
            return json_error(
                Denied::Unauthenticated,
                &format!("route {:?} 需要登录", route.name),
            );
        }
        Some(s) if !s.can_access(&route.name) => {
            tracing::warn!(
                route = %route.name, user = %s.user, source = ?source,
                "authenticated but not authorised for this route"
            );
            if wants_html {
                return html(
                    StatusCode::FORBIDDEN,
                    web::error_page(
                        &gw.cfg.title,
                        "无权访问",
                        &format!(
                            "当前账号 <code>{}</code> 没有被授权访问 <code>{}</code>。",
                            html_escape(&s.user),
                            html_escape(&route.name)
                        ),
                    ),
                );
            }
            return json_error(Denied::Forbidden, "当前账号无权访问该端口");
        }
        Some(_) => {}
    }

    // ④ 状态变更请求校验 Origin（纵深防御，主防线是 SameSite=Strict）
    if !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS)
        && !auth::same_origin(req.headers())
    {
        tracing::warn!(route = %route.name, source = ?source, "cross-origin write rejected");
        return text(StatusCode::FORBIDDEN, "跨站请求被拒绝");
    }

    // ⑤ 转发
    forward(&route, req, source, &gw.cfg.cookie_name).await
}

/// 网关首页。需要登录（它要显示「我能去哪」，那本身就是授权信息）。
fn home(
    gw: &Gateway,
    route: Option<&RouteRuntime>,
    headers: &axum::http::HeaderMap,
) -> HyperResponse<ProxyBody> {
    let sid = auth::cookie_value(headers, &gw.cfg.cookie_name);
    let Some(session) = sid.and_then(|s| gw.auth.get(s)) else {
        if auth::wants_html(headers) {
            return redirect(StatusCode::FOUND, "/__auth/login?next=/__auth/");
        }
        return json_error(Denied::Unauthenticated, "网关首页需要登录");
    };

    // 只列这个会话**有权访问**的端口。理由和别处一致：没权限的东西连名字都
    // 不该出现——否则「这台机器开了哪些面板」就成了公开信息。
    let mut links: Vec<(String, u16)> = gw
        .routes
        .all()
        .into_iter()
        .filter(|r| session.can_access(&r.name))
        .map(|r| (r.name.clone(), r.listen.port()))
        .collect();
    links.sort_by_key(|(_, port)| *port);

    // 用当前请求的 Host 换端口拼链接。协议用 `//`（协议相对），这样将来
    // 前面挂了 TLS 终结也不用改这里。
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim()
        .trim_start_matches('[')
        .split(']')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_string();

    let routes: Vec<web::HomeLink> = links
        .into_iter()
        .map(|(name, port)| web::HomeLink {
            name,
            href: if host.is_empty() {
                format!(":{}", port)
            } else {
                format!("//{host}:{port}/")
            },
        })
        .collect();

    let view = web::HomeView {
        title: gw.title().to_string(),
        user: session.user.clone(),
        admin: session.admin,
        routes,
        current_route: route.map(|r| r.name.clone()).unwrap_or_default(),
        // 配了独立管理端口时，首页上的管理链接要指到那个端口去；
        // 否则在业务端口点「管理账号」会被当普通路径转发 → 上游 404。
        admin_base: gw
            .cfg
            .admin_listen
            .map(|a| admin_base(a, &host))
            .unwrap_or_default(),
    };
    html(StatusCode::OK, web::home_page(&view))
}

/// 解析请求来源。
///
/// `trust_forwarded_for = false`（默认）时**只认 socket 对端**。那个头谁都能写，
/// 拿它做访问控制等于没锁。
fn resolve_source(gw: &Gateway, req: &HyperRequest<Incoming>, peer: SocketAddr) -> Option<IpAddr> {
    if !gw.cfg.trust_forwarded_for {
        return Some(peer.ip());
    }
    // 开启信任时取 XFF 的**最后**一个（最靠近我们的那一跳）。
    // 取第一个是错的：那是客户端自己写的，可以随便伪造。
    req.headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.rsplit(',').next())
        .and_then(|s| s.trim().parse().ok())
        .or(Some(peer.ip()))
}

// ─────────────────────────────────────────────────────────────────────────────
// 网关自己的端点
// ─────────────────────────────────────────────────────────────────────────────

/// `route` 为 `None` 表示这是**管理端口**：此时登录不检查任何 route 权限
/// （管理权限由 `session.admin` 在 `/__admin/` 那边判）。
async fn gateway_endpoint(
    gw: &Gateway,
    route: Option<&RouteRuntime>,
    source: Option<IpAddr>,
    req: HyperRequest<Incoming>,
) -> HyperResponse<ProxyBody> {
    let path = req.uri().path().to_string();
    let is_post = req.method() == Method::POST;

    match path.as_str() {
        "/__auth/login" if is_post => {
            let (parts, body) = req.into_parts();
            do_login(gw, route, source, &parts.headers, parts.uri.query(), body).await
        }
        "/__auth/login" => {
            let next = query_param(req.uri().query(), "next").unwrap_or_else(|| "/".into());
            let next = sanitize_next(&next);
            login_page(gw, route, &next, None)
        }
        "/__auth/logout" => {
            let sid = auth::cookie_value(req.headers(), &gw.cfg.cookie_name);
            if let Some(sid) = sid {
                let who = gw.auth.get(sid).map(|s| s.user).unwrap_or_default();
                gw.auth.logout(sid);
                tracing::info!(route = %route_label(route), user = %who, source = ?source, "logout");
            }
            let mut resp = redirect(StatusCode::FOUND, "/__auth/login");
            resp.headers_mut()
                .insert(header::SET_COOKIE, auth::clear_cookie(&gw.cfg.cookie_name));
            resp
        }
        "/__auth/healthz" => text(StatusCode::OK, "ok"),
        // 网关自己的首页：我是谁、我能去哪些端口、管理入口在哪。
        // 没有它的话，登录成功后落在被代理的面板里，那个页面完全不知道
        // 「网关」的存在——于是 /__admin/ 变成一个只能靠记 URL 才到得了的地方。
        "/__auth" | "/__auth/" => home(gw, route, req.headers()),
        _ => text(StatusCode::NOT_FOUND, "没有这个网关端点"),
    }
}

async fn do_login(
    gw: &Gateway,
    route: Option<&RouteRuntime>,
    source: Option<IpAddr>,
    headers: &HeaderMap,
    query: Option<&str>,
    body_in: Incoming,
) -> HyperResponse<ProxyBody> {
    // 限速优先于一切：被挡的人不该有机会触发 argon2 计算（那是 CPU 成本）
    if let Some(wait) = gw.auth.blocked_for(source) {
        let secs = wait.as_secs().max(1);
        tracing::warn!(source = ?source, retry_after = secs, "login rate limited");
        let mut resp = login_page(
            gw,
            route,
            "/",
            Some(&format!("尝试过于频繁，请 {secs} 秒后再试")),
        );
        *resp.status_mut() = StatusCode::TOO_MANY_REQUESTS;
        resp.headers_mut().insert(
            header::RETRY_AFTER,
            HeaderValue::from_str(&secs.to_string()).unwrap_or(HeaderValue::from_static("60")),
        );
        return resp;
    }

    // 读表单。上限 4 KB —— 登录表单不该有别的用途。
    let body = match Limited::new(body_in, 4096).collect().await {
        Ok(b) => b.to_bytes(),
        Err(_) => return text(StatusCode::BAD_REQUEST, "请求体过大或读取失败"),
    };
    let Ok(form) = std::str::from_utf8(&body) else {
        return text(StatusCode::BAD_REQUEST, "请求体不是合法 UTF-8");
    };
    let fields = parse_form(form);
    let user = fields.get("user").map(String::as_str).unwrap_or("").trim();
    let pass = fields.get("pass").map(String::as_str).unwrap_or("");
    let from_query = query_param(query, "next");
    let next_raw = fields
        .get("next")
        .map(String::as_str)
        .or(from_query.as_deref())
        .unwrap_or("/");
    let next = sanitize_next(next_raw);

    // 手工比对而不是查表后短路：**用户名不存在时也要跑一次哈希校验**，
    // 否则响应时间会泄露「这个用户名存在」。
    let candidate = gw.user(user);
    let hash = candidate
        .as_ref()
        .map(|u| u.hash.clone())
        .unwrap_or_else(dummy_hash);
    let ok = crate::config::verify_password(pass, &hash);
    let user_rt = match (ok, candidate) {
        (true, Some(u)) => u,
        _ => {
            gw.auth.record_failure(source);
            tracing::warn!(source = ?source, user = %user, "login failed");
            // 不区分"用户不存在"和"口令错误"——那本身就是信息泄露。
            let mut resp = login_page(gw, route, &next, Some("用户名或口令不正确"));
            *resp.status_mut() = StatusCode::UNAUTHORIZED;
            return resp;
        }
    };

    // 权限：登录成功但这个 route 不给它访问，直接告诉他，别让他登进去再撞墙。
    // 管理端口不检查 route 权限——那里要判的是 admin，由 admin::handle 负责
    // （否则一个"只能看隧道面板"的账号会连自己的改密码页面都进不去）。
    if let Some(r) = route {
        if !user_rt.can_access(&r.name) {
            gw.auth.record_failure(source);
            tracing::warn!(
                source = ?source, user = %user_rt.name, route = %r.name,
                "login ok but not authorised for this route"
            );
            let mut resp = login_page(gw, route, &next, Some("该账号没有此端口的访问权限"));
            *resp.status_mut() = StatusCode::FORBIDDEN;
            return resp;
        }
    }

    gw.auth.clear_source(source);
    // 登录成功后重建 session id（防会话固定）
    if let Some(old) = auth::cookie_value(headers, &gw.cfg.cookie_name) {
        gw.auth.logout(old);
    }
    let sid = gw.auth.create(&user_rt, source);
    tracing::info!(
        route = %route_label(route), user = %user_rt.name, source = ?source,
        "login ok"
    );

    let mut resp = redirect(StatusCode::SEE_OTHER, &next);
    resp.headers_mut().insert(
        header::SET_COOKIE,
        auth::set_cookie(
            &gw.cfg.cookie_name,
            &sid,
            gw.cfg.session_ttl,
            gw.cfg.secure_cookie,
        ),
    );
    resp
}

/// 用户名不存在时用来跑一次"假校验"的哈希。
///
/// 值本身无所谓，只要是一个合法的 argon2 串，让 verify 花掉与真实用户相同的
/// 时间。硬编码在这里是刻意的：**不能**从配置里取，否则「配置为空」会让
/// 时间差重新出现。
fn dummy_hash() -> String {
    // "dummy-password" 的 argon2id 哈希（m=19456,t=2,p=1）
    "$argon2id$v=19$m=19456,t=2,p=1$ZHVtbXlzYWx0ZHVtbXk$0Yp2PZ0z1n0m3rN0zYb0uZ0dU0kQ0l0Yb0uZ0dU0kQ0"
        .to_string()
}

fn login_page(
    gw: &Gateway,
    route: Option<&RouteRuntime>,
    next: &str,
    error: Option<&str>,
) -> HyperResponse<ProxyBody> {
    html(
        StatusCode::OK,
        web::login_page(&gw.cfg.title, route_label(route), next, error),
    )
}

/// 登录页上「要访问的端口」那一行显示什么。管理端口没有 route，
/// 所以给它一句人话，而不是留空或者显示一个内部标识符。
fn route_label(route: Option<&RouteRuntime>) -> &str {
    route.map(|r| r.name.as_str()).unwrap_or("管理面板")
}

// ─────────────────────────────────────────────────────────────────────────────
// 转发
// ─────────────────────────────────────────────────────────────────────────────

async fn forward(
    route: &RouteRuntime,
    req: HyperRequest<Incoming>,
    source: Option<IpAddr>,
    session_cookie: &str,
) -> HyperResponse<ProxyBody> {
    let (mut parts, body) = req.into_parts();

    // ① 上游连接
    let stream = match tokio::time::timeout(
        UPSTREAM_CONNECT_TIMEOUT,
        TcpStream::connect(route.upstream),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            tracing::error!(route = %route.name, upstream = %route.upstream, error = %e,
                "upstream connect failed");
            return text(
                StatusCode::BAD_GATEWAY,
                &format!("无法连接上游 {}（{e}）", route.upstream),
            );
        }
        Err(_) => {
            tracing::error!(route = %route.name, upstream = %route.upstream, "upstream connect timeout");
            return text(StatusCode::GATEWAY_TIMEOUT, "连接上游超时");
        }
    };
    let _ = stream.set_nodelay(true);

    // ② 剥 hop-by-hop 头（RFC 9110 §7.6.1）
    strip_hop_by_hop(&mut parts.headers);

    // ③ **剥掉网关自己的 session cookie**
    //
    // 这是网关与浏览器之间的凭据，上游不需要也不该看到。不剥的话：
    // session id 会出现在上游的访问日志里；上游若被换成第三方服务（比如把
    // 面板换成一个 SaaS 状态页），cookie 就直接泄露给外部了。
    strip_cookie(&mut parts.headers, session_cookie);

    // ③ 重写 Host：上游只看得到自己的地址，客户端无法用 Host 影响上游路由
    if let Ok(v) = HeaderValue::from_str(&route.upstream.to_string()) {
        parts.headers.insert(header::HOST, v);
    }
    // 把真实来源告诉上游（只作为信息，上游不该用它做安全判断——文档里会写）
    if let Some(ip) = source {
        if let Ok(v) = HeaderValue::from_str(&ip.to_string()) {
            parts.headers.insert("x-forwarded-for", v);
        }
    }
    parts
        .headers
        .insert("x-forwarded-proto", HeaderValue::from_static("http"));
    parts.headers.insert(
        "x-esc-gateway-route",
        HeaderValue::from_str(&route.name).unwrap_or(HeaderValue::from_static("-")),
    );

    // ④ 路径重写
    if let Some(prefix) = &route.strip_prefix {
        if let Some(rest) = parts.uri.path().strip_prefix(prefix.as_str()) {
            let rest = if rest.is_empty() { "/" } else { rest };
            let new_path = match parts.uri.query() {
                Some(q) => format!("{rest}?{q}"),
                None => rest.to_string(),
            };
            // 只换 path_and_query，保持 origin-form。
            // 塞进 scheme+authority 会变成 absolute-form——虽然合法，但会让
            // 上游看到 `http://upstream/api/status` 这种它自己也要解析一遍的
            // 绝对 URL，而 origin-form 是代理转发时的惯用形态。
            let mut builder = hyper::Uri::builder();
            let rebuilt = match new_path.parse::<hyper::http::uri::PathAndQuery>() {
                Ok(pq) => {
                    builder = builder.path_and_query(pq);
                    builder.build().ok()
                }
                Err(_) => None,
            };
            if let Some(u) = rebuilt {
                parts.uri = u;
            }
        }
    } else {
        // 没有 strip_prefix 时也要把 authority 换掉，否则 h1 的 origin-form 就够了。
        // 这里保持 origin-form（只 path），hyper 的 client 也能处理。
    }

    // ⑤ 请求体：限制大小后透传（不缓冲，边收边发）
    let max = route.max_body_bytes as usize;
    let limited = Limited::new(body, max);
    let up_body = http_body_util::BodyExt::map_err(limited, |e| -> BoxErr {
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("request body: {e}"),
        ))
    })
    .boxed();
    let up_req = HyperRequest::from_parts(parts, up_body);

    // ⑥ 连到上游并发送
    let (mut sender, conn) = match tokio::time::timeout(
        UPSTREAM_CONNECT_TIMEOUT,
        hyper::client::conn::http1::handshake(TokioIo::new(stream)),
    )
    .await
    {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            tracing::error!(route = %route.name, error = %e, "upstream handshake failed");
            return text(StatusCode::BAD_GATEWAY, "与上游握手失败");
        }
        Err(_) => return text(StatusCode::GATEWAY_TIMEOUT, "与上游握手超时"),
    };
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            tracing::debug!(error = %e, "upstream connection ended");
        }
    });

    let resp = match tokio::time::timeout(UPSTREAM_HEAD_TIMEOUT, sender.send_request(up_req)).await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::error!(route = %route.name, error = %e, "upstream request failed");
            // 请求体超限时 hyper 会在发完之前报错，这类要区分出来。
            let msg = e.to_string();
            if msg.contains("request body") || msg.contains("length limit") {
                return text(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    &format!("请求体超过上限（{} 字节）", route.max_body_bytes),
                );
            }
            return text(StatusCode::BAD_GATEWAY, "上游返回错误");
        }
        Err(_) => {
            tracing::error!(route = %route.name, "upstream response head timeout");
            return text(StatusCode::GATEWAY_TIMEOUT, "等待上游响应超时");
        }
    };

    // ⑦ 响应：剥 hop-by-hop，透传 body
    let (mut rparts, rbody) = resp.into_parts();
    strip_hop_by_hop(&mut rparts.headers);
    let out = http_body_util::BodyExt::map_err(rbody, |e| -> BoxErr { Box::new(e) }).boxed();
    HyperResponse::from_parts(rparts, out)
}

/// 从 `Cookie` 头里删掉指定名字的那一项，保留其余。
///
/// 只删我们自己的那一个：上游可能也有自己的 cookie（比如面板的其它功能），
/// 整个 `Cookie` 头删掉会连带破坏它们。
fn strip_cookie(headers: &mut HeaderMap, name: &str) {
    let values: Vec<String> = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(str::to_string)
        .collect();
    if values.is_empty() {
        return;
    }
    let kept: Vec<String> = values
        .iter()
        .flat_map(|v| v.split(';'))
        .map(str::trim)
        .filter(|pair| {
            let key = pair.split_once('=').map(|(k, _)| k.trim()).unwrap_or(pair);
            !key.eq_ignore_ascii_case(name)
        })
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();

    headers.remove(header::COOKIE);
    if kept.is_empty() {
        return;
    }
    if let Ok(v) = HeaderValue::from_str(&kept.join("; ")) {
        headers.insert(header::COOKIE, v);
    }
}

/// 剥离 hop-by-hop 头，以及 `Connection` 里点名的那些头。
///
/// 这一步是**必须**的，不是讲究：如果原样转发 `Connection: keep-alive`，
/// 上游会按它自己的连接语义处理；更糟的是 `Transfer-Encoding` 与
/// `Content-Length` 并存会构成请求走私的经典载体。
fn strip_hop_by_hop(headers: &mut HeaderMap) {
    // RFC 9110 §7.6.1 列出的固定名单
    const HOP: &[&str] = &[
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ];
    // 先把 Connection 里点名的头收集起来（它们也是 hop-by-hop）
    let mut named: Vec<String> = Vec::new();
    if let Some(v) = headers
        .get(header::CONNECTION)
        .and_then(|v| v.to_str().ok())
    {
        named = v
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
    }
    for h in HOP {
        headers.remove(*h);
    }
    let mut seen: HashSet<String> = HashSet::new();
    for name in named {
        if seen.insert(name.clone()) {
            if let Ok(n) = HeaderName::from_bytes(name.as_bytes()) {
                headers.remove(n);
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 小工具
// ─────────────────────────────────────────────────────────────────────────────

fn query_suffix(req: &HyperRequest<Incoming>) -> String {
    match req.uri().query() {
        Some(q) => format!("?{q}"),
        None => String::new(),
    }
}

pub fn query_param(q: Option<&str>, key: &str) -> Option<String> {
    let q = q?;
    for pair in q.split('&') {
        let (k, v) = match pair.split_once('=') {
            Some(kv) => kv,
            None => (pair, ""),
        };
        if urldecode(k) == key {
            return Some(urldecode(v));
        }
    }
    None
}

/// 只接受站内相对路径，防止 `next=//evil.com` 这类开放重定向。
///
/// 开放重定向在登录页上尤其危险：攻击者可以让受害者看到**我们域名的登录页**，
/// 登录成功后被送去钓鱼站，看起来就像我们在钓他。
pub fn sanitize_next(next: &str) -> String {
    let n = next.trim();
    if n.is_empty() || !n.starts_with('/') {
        return "/".into();
    }
    // `//host` 与 `/\host` 都会被浏览器当成协议相对 URL
    if n.starts_with("//") || n.starts_with("/\\") {
        return "/".into();
    }
    // 控制字符（含 \r\n）用于 header 注入
    if n.chars().any(|c| c.is_control()) {
        return "/".into();
    }
    n.to_string()
}

pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = |c: u8| -> Option<u8> {
                    match c {
                        b'0'..=b'9' => Some(c - b'0'),
                        b'a'..=b'f' => Some(c - b'a' + 10),
                        b'A'..=b'F' => Some(c - b'A' + 10),
                        _ => None,
                    }
                };
                match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                    (Some(a), Some(b)) => {
                        out.push(a * 16 + b);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_form(body: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for pair in body.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some(kv) => kv,
            None => (pair, ""),
        };
        map.entry(urldecode(k)).or_insert_with(|| urldecode(v));
    }
    map
}

pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn full(bytes: impl Into<Bytes>) -> ProxyBody {
    http_body_util::Full::new(bytes.into())
        .map_err(|never| -> BoxErr { match never {} })
        .boxed()
}

pub fn html(status: StatusCode, body: String) -> HyperResponse<ProxyBody> {
    let mut resp = HyperResponse::new(full(body));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

pub fn text(status: StatusCode, body: &str) -> HyperResponse<ProxyBody> {
    let mut resp = HyperResponse::new(full(body.to_string()));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}

pub fn redirect(status: StatusCode, location: &str) -> HyperResponse<ProxyBody> {
    let mut resp = HyperResponse::new(full(String::new()));
    *resp.status_mut() = status;
    if let Ok(v) = HeaderValue::from_str(location) {
        resp.headers_mut().insert(header::LOCATION, v);
    }
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

pub(crate) fn json_error(kind: Denied, message: &str) -> HyperResponse<ProxyBody> {
    let mut resp = HyperResponse::new(full(serde_json::json!({ "error": message }).to_string()));
    *resp.status_mut() = kind.status_code();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if let Denied::RateLimited { retry_after_secs } = kind {
        resp.headers_mut().insert(
            header::RETRY_AFTER,
            HeaderValue::from_str(&retry_after_secs.to_string())
                .unwrap_or(HeaderValue::from_static("60")),
        );
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hop_by_hop_headers_are_stripped_including_named_ones() {
        let mut h = HeaderMap::new();
        h.insert(
            header::CONNECTION,
            HeaderValue::from_static("keep-alive, X-Custom"),
        );
        h.insert("x-custom", HeaderValue::from_static("v"));
        h.insert(
            header::TRANSFER_ENCODING,
            HeaderValue::from_static("chunked"),
        );
        h.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        h.insert(header::TE, HeaderValue::from_static("trailers"));
        h.insert("x-keep", HeaderValue::from_static("this one survives"));
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));

        strip_hop_by_hop(&mut h);

        assert!(h.get(header::CONNECTION).is_none());
        assert!(h.get(header::TRANSFER_ENCODING).is_none());
        assert!(
            h.get(header::UPGRADE).is_none(),
            "Upgrade 必须剥（我们不支持）"
        );
        assert!(h.get(header::TE).is_none());
        assert!(h.get("x-custom").is_none(), "Connection 点名的头也要剥");
        assert_eq!(h.get("x-keep").unwrap(), "this one survives");
        assert_eq!(h.get(header::CONTENT_TYPE).unwrap(), "text/plain");
    }

    #[test]
    fn stripping_is_case_insensitive_for_named_headers() {
        let mut h = HeaderMap::new();
        h.insert(header::CONNECTION, HeaderValue::from_static("X-Foo"));
        h.insert("x-foo", HeaderValue::from_static("1"));
        strip_hop_by_hop(&mut h);
        assert!(h.get("x-foo").is_none());
    }

    #[test]
    fn open_redirect_payloads_are_neutralised() {
        assert_eq!(sanitize_next("/api/status"), "/api/status");
        assert_eq!(sanitize_next("/a?b=c"), "/a?b=c");
        // 协议相对 URL —— 浏览器会当成 https://evil.com
        assert_eq!(sanitize_next("//evil.com"), "/");
        assert_eq!(sanitize_next("/\\evil.com"), "/");
        // 绝对 URL
        assert_eq!(sanitize_next("https://evil.com"), "/");
        // 头注入
        assert_eq!(sanitize_next("/x\r\nSet-Cookie: a=b"), "/");
        assert_eq!(sanitize_next(""), "/");
        assert_eq!(sanitize_next("relative"), "/");
    }

    #[test]
    fn form_and_query_parsing_handle_encoding() {
        let m = parse_form("user=ad%6Din&pass=p%40ss%2Bword&next=%2Fapi");
        assert_eq!(m["user"], "admin");
        assert_eq!(m["pass"], "p@ss+word");
        assert_eq!(m["next"], "/api");

        assert_eq!(query_param(Some("a=1&b=2"), "b"), Some("2".into()));
        assert_eq!(query_param(Some("a=1"), "nope"), None);
        assert_eq!(query_param(None, "a"), None);
        // 中文与空格
        assert_eq!(
            query_param(Some("t=%E4%B8%AD+%E6%96%87"), "t"),
            Some("中 文".into())
        );
    }

    #[test]
    fn urlencode_round_trips() {
        for s in ["/a/b", "/中文?x=1", "/a b", "/%", "/a+b"] {
            let enc = urlencode(s);
            if s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "/-_.".contains(c))
            {
                assert_eq!(enc, s);
            }
            assert_eq!(urldecode(&enc), s, "round trip failed for {s:?}");
        }
    }

    #[test]
    fn html_escaping_covers_the_dangerous_five() {
        assert_eq!(html_escape("<script>"), "&lt;script&gt;");
        assert_eq!(html_escape("a&b"), "a&amp;b");
        assert_eq!(html_escape("\"'"), "&quot;&#39;");
    }
}
