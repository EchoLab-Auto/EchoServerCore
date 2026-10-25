//! Web 面板插件（**子进程**）。
//!
//! # 它认识什么，不认识什么
//!
//! 这个 crate 的依赖列表里**没有** `esc-engine`、`esc-common`、`rustls`、`yamux`。
//! 它只知道 [`esc_control_api`] 里那份契约：方法名 + JSON。
//!
//! ```text
//!   浏览器 ──HTTP──▶ 本插件(axum) ──HostCall("esc.control", method, params)──▶ 宿主
//!                                                                                │
//!                                                              Ctx: Arc<dyn TunnelControl>
//!                                                                                │
//!                                                                          esc-engine
//! ```
//!
//! 换句话说：**把面板整个删掉，隧道照常转发**；把隧道换成另一个实现，面板不用改。
//!
//! # 为什么要有 [`ControlBackend`] 这一层
//!
//! HTTP 处理器不直接调用 `HostClient`，而是面向一个只有两个方法的小 trait。
//! 生产实现是 [`HostCallBackend`]；测试实现是一个内存假后端。
//! 于是「面板的 API 是否正确映射到契约方法」这件事可以在**没有框架、没有引擎**
//! 的情况下单测——这也是解耦带来的直接好处。

pub mod web;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use esc_panel_auth::{AccessControl, AuthConfig, Denied};
use echo_plugin_sdk::echo_plugin_api::ServiceContribution;
use echo_plugin_sdk::{
    async_trait, serve_stdio_with, Contribution, HostClient, InvokeOutcome, InvokeRequest,
    PluginHandler, ServeOptions,
};
use serde_json::{json, Value};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use esc_control_api::{method, CONTROL_SERVICE};

/// 面板在宿主里暴露的贡献名（宿主可以用 `invoke("esc.panel", …)` 探测它）。
pub const PANEL_KEY: &str = "esc.panel";

// ─────────────────────────────────────────────────────────────────────────────
// 配置
// ─────────────────────────────────────────────────────────────────────────────

/// 面板自己的配置（来自 `Hello.config`）。
#[derive(Debug, Clone)]
pub struct PanelConfig {
    /// HTTP 监听地址。默认只监听回环。
    pub listen: String,
    /// `user:password`。留空表示不做认证——但**只允许**在回环地址上这么干。
    pub auth: Option<String>,
    /// 页面标题。
    pub title: String,
    /// 允许跨机器访问时的额外提示（目前只用于日志）。
    pub behind_proxy: bool,
    /// 同一个来源 IP 在窗口内允许的认证失败次数；`0` = 不限制。
    ///
    /// 面板一旦暴露到公网，会立刻被扫描器持续试探口令。20 位随机口令本身猜不中，
    /// 但没理由让对面无限次尝试——默认 8 次/分钟，超过就 429。
    pub max_auth_failures: u32,
    /// 认证失败计数的窗口长度（秒）。
    pub auth_fail_window_secs: u64,
    /// 允许访问的来源地址（IPv4 CIDR 或精确 IP）。空 = 不限。
    ///
    /// 暴露到公网时这是最有效的一道锁：只放行自己常用的出口 IP 段，
    /// 别人连认证提示都拿不到。
    pub allow_cidrs: Vec<String>,
}

impl Default for PanelConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8787".into(),
            auth: None,
            title: "esc 隧道面板".into(),
            behind_proxy: false,
            max_auth_failures: 8,
            auth_fail_window_secs: 60,
            allow_cidrs: Vec::new(),
        }
    }
}

impl PanelConfig {
    pub fn from_json(v: &Value) -> Self {
        let d = Self::default();
        let shared = AuthConfig::from_json(v);
        Self {
            listen: v
                .get("listen")
                .and_then(Value::as_str)
                .unwrap_or(&d.listen)
                .to_string(),
            auth: shared.auth.clone(),
            title: v
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or(&d.title)
                .to_string(),
            behind_proxy: v.get("behind_proxy").and_then(Value::as_bool).unwrap_or(false),
            // 与访问控制有关的字段统一由共享 crate 解析，避免两套规则漂移。
            max_auth_failures: shared.max_auth_failures,
            auth_fail_window_secs: shared.auth_fail_window_secs,
            allow_cidrs: shared.allow_cidrs,
        }
    }

    /// 监听地址是否只落在回环上。
    ///
    /// 安全默认：**没有口令就不许监听公网地址**。这条检查在 `configure` 阶段做，
    /// 也就是说配置错了插件直接拒绝启动（宿主会看到 `Failed{invalid_config}`），
    /// 而不是悄悄把无认证的管理面挂到公网上。
    pub fn is_loopback_only(&self) -> bool {
        let host = self
            .listen
            .rsplit_once(':')
            .map(|(h, _)| h.trim_start_matches('[').trim_end_matches(']'))
            .unwrap_or(&self.listen);
        matches!(host, "127.0.0.1" | "localhost" | "::1")
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.listen.rsplit_once(':').is_none() {
            return Err(format!("listen 需要 host:port，收到 {:?}", self.listen));
        }
        if self.auth.is_none() && !self.is_loopback_only() {
            return Err(format!(
                "listen = {:?} 不是回环地址，但没有配置 auth；\
                 拒绝把无认证的管理面板暴露到网络上（改成 127.0.0.1:端口，或设置 auth = \"user:pass\"）",
                self.listen
            ));
        }
        // 白名单写错了必须报错，不能「解析失败就当没限制」——那样会静默地把
        // 管理面开给所有人，而配置看起来还像是锁着的。
        // 具体校验在共享的 `AuthConfig` 里（服务器面板用同一份）。
        self.auth_config().validate()
    }

    /// 把本配置里与访问控制相关的字段打包给 `esc-panel-auth`。
    pub fn auth_config(&self) -> AuthConfig {
        AuthConfig {
            auth: self.auth.clone(),
            allow_cidrs: self.allow_cidrs.clone(),
            max_auth_failures: self.max_auth_failures,
            auth_fail_window_secs: self.auth_fail_window_secs,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 后端抽象
// ─────────────────────────────────────────────────────────────────────────────

/// 面板把请求转给谁。
#[async_trait]
pub trait ControlBackend: Send + Sync + 'static {
    async fn call(&self, method: &str, params: Value) -> Result<Value, (StatusCode, String)>;
}

/// 引擎侧的操作（bind、写映射表、查状态）都是毫秒级的，所以 10 秒已经是
/// 非常宽松的上限。默认的 30 秒会让浏览器在一个真的卡住的调用上白等半分钟，
/// 而这时用户想看到的是「引擎没响应」而不是转圈。
const CONTROL_CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// 生产实现：走插件框架的 `HostCall`。
pub struct HostCallBackend {
    host: HostClient,
}

impl HostCallBackend {
    pub fn new(host: HostClient) -> Self {
        Self { host }
    }
}

#[async_trait]
impl ControlBackend for HostCallBackend {
    async fn call(&self, method: &str, params: Value) -> Result<Value, (StatusCode, String)> {
        match self
            .host
            .call_with_timeout(CONTROL_SERVICE, method, params, CONTROL_CALL_TIMEOUT)
            .await
        {
            Ok(v) => Ok(v),
            Err(e) => {
                // 用 `code` 而不是解析 `Display` 文本来决定状态码：错误文本会变，
                // 机器可读的 code 是契约的一部分。
                let (status, text) = match &e {
                    echo_plugin_sdk::HostCallError::Timeout => (
                        StatusCode::GATEWAY_TIMEOUT,
                        format!(
                            "引擎在 {}s 内没有响应（{}）——看宿主日志确认隧道引擎是否卡住",
                            CONTROL_CALL_TIMEOUT.as_secs(),
                            method
                        ),
                    ),
                    echo_plugin_sdk::HostCallError::Closed => (
                        StatusCode::SERVICE_UNAVAILABLE,
                        "与宿主的连接已断开，面板即将退出".to_string(),
                    ),
                    echo_plugin_sdk::HostCallError::Service { code, message } => {
                        // 码表在契约里（`ErrorKind::as_code` / `http_status`），
                        // 面板只管把码翻成状态码，不维护第二份字符串表。
                        let kind = esc_control_api::ErrorKind::from_code(code);
                        let status = StatusCode::from_u16(kind.http_status())
                            .unwrap_or(StatusCode::BAD_GATEWAY);
                        (status, message.clone())
                    }
                    other => (StatusCode::BAD_GATEWAY, other.to_string()),
                };
                Err((status, text))
            }
        }
    }
}

// 访问控制（Basic 认证 / 来源白名单 / 失败限速）住在 `esc-panel-auth` 里。
// 那边有一份完整实现与测试，服务器面板也用它——这套逻辑不能有两份。

// ─────────────────────────────────────────────────────────────────────────────
// HTTP
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct AppState {
    backend: Arc<dyn ControlBackend>,
    title: String,
    access: Arc<AccessControl>,
}

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<(StatusCode, String)> for ApiError {
    fn from((code, msg): (StatusCode, String)) -> Self {
        Self(code, msg)
    }
}

/// 组装路由（测试直接用它，不需要真的起插件）。
pub fn router(backend: Arc<dyn ControlBackend>, cfg: &PanelConfig) -> Router {
    let state = AppState {
        backend,
        title: cfg.title.clone(),
        access: Arc::new(AccessControl::new(&cfg.auth_config())),
    };
    Router::new()
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .route("/api/status", get(api_status))
        .route("/api/mappings", get(api_list).post(api_create))
        .route("/api/mappings/{id}", post(api_update).delete(api_delete))
        .route("/api/mappings/{id}/enabled", post(api_set_enabled))
        .route("/api/reload", post(api_reload))
        .route("/api/spokes/kick", post(api_kick))
        .route("/api/export", get(api_export))
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
        .with_state(state)
}

async fn auth_middleware(
    State(st): State<AppState>,
    headers: HeaderMap,
    req: Request,
    next: Next,
) -> Response {
    // 来源地址来自 accept 的 socket（`into_make_service_with_connect_info`）。
    // 不走 X-Forwarded-For：那个头谁都能伪造，在白名单/限速里信它等于没锁。
    let peer: Option<IpAddr> = req
        .extensions()
        .get::<axum::extract::ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip());

    match st.access.check(&headers, peer) {
        Ok(()) => next.run(req).await,
        Err(Denied(response)) => *response,
    }
}

async fn index(State(st): State<AppState>) -> Html<String> {
    Html(web::index_html(&st.title))
}

async fn healthz() -> &'static str {
    "ok"
}

async fn api_status(State(st): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(st.backend.call(method::STATUS, Value::Null).await?))
}

async fn api_list(State(st): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(st.backend.call(method::MAPPING_LIST, Value::Null).await?))
}

async fn api_create(State(st): State<AppState>, Json(body): Json<Value>) -> Result<Json<Value>, ApiError> {
    Ok(Json(st.backend.call(method::MAPPING_UPSERT, body).await?))
}

async fn api_update(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(mut body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    // 路径里的 id 是权威的：防止前端 body 与 URL 不一致时改错记录。
    if let Some(obj) = body.as_object_mut() {
        obj.insert("id".into(), Value::String(id));
    }
    Ok(Json(st.backend.call(method::MAPPING_UPSERT, body).await?))
}

async fn api_delete(State(st): State<AppState>, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        st.backend
            .call(method::MAPPING_DELETE, json!({ "id": id }))
            .await?,
    ))
}

async fn api_set_enabled(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let enabled = body
        .get("enabled")
        .and_then(Value::as_bool)
        .ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, "缺少 enabled 字段".into()))?;
    Ok(Json(
        st.backend
            .call(method::MAPPING_SET_ENABLED, json!({ "id": id, "enabled": enabled }))
            .await?,
    ))
}

async fn api_reload(State(st): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(st.backend.call(method::ENGINE_RELOAD, Value::Null).await?))
}

async fn api_kick(State(st): State<AppState>, body: Option<Json<Value>>) -> Result<Json<Value>, ApiError> {
    let id = body
        .and_then(|Json(v)| v.get("id").and_then(Value::as_str).map(str::to_string));
    Ok(Json(
        st.backend
            .call(method::SPOKES_KICK, json!({ "id": id }))
            .await?,
    ))
}

async fn api_export(
    State(st): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    let role = q.get("role").cloned().unwrap_or_else(|| "hub".into());
    Ok(Json(
        st.backend
            .call(method::CONFIG_EXPORT, json!({ "role": role }))
            .await?,
    ))
}

// ─────────────────────────────────────────────────────────────────────────────
// 插件本体
// ─────────────────────────────────────────────────────────────���───────────────

pub struct Panel {
    cfg: RwLock<PanelConfig>,
    host: OnceLock<HostClient>,
    shutdown: CancellationToken,
    /// HTTP 服务是否已经在跑（`invoke` 靠它报告状态）。
    serving: AtomicBool,
    bound: RwLock<Option<String>>,
}

impl Default for Panel {
    fn default() -> Self {
        Self::new()
    }
}

impl Panel {
    pub fn new() -> Self {
        Self {
            cfg: RwLock::new(PanelConfig::default()),
            host: OnceLock::new(),
            shutdown: CancellationToken::new(),
            serving: AtomicBool::new(false),
            bound: RwLock::new(None),
        }
    }

    /// 起 HTTP 服务。抽出来是为了能在测试里直接调用。
    pub async fn serve(
        &self,
        backend: Arc<dyn ControlBackend>,
        cfg: PanelConfig,
        shutdown: CancellationToken,
    ) -> std::io::Result<String> {
        let app = router(backend, &cfg);
        let listener = tokio::net::TcpListener::bind(&cfg.listen).await?;
        let addr = listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| cfg.listen.clone());
        let shutdown_signal = {
            let token = shutdown.clone();
            async move { token.cancelled().await }
        };
        if !cfg.is_loopback_only() {
            // 这一条只打一次日志，但值得显眼：面板是明文 HTTP，
            // Basic 认证的口令在链路上就是 base64，等于明文。
            tracing::warn!(
                listen = %cfg.listen,
                allow_cidrs = cfg.allow_cidrs.len(),
                max_auth_failures = cfg.max_auth_failures,
                "panel is listening on a non-loopback address: 管理面已暴露到网络。\
                 口令以 Basic 认证明文传输，请只在可信链路使用，并考虑加 TLS 终结\
                 （见 docs/PANEL.md 的「暴露到公网」一节）"
            );
        }
        tokio::spawn(async move {
            if let Err(e) = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(shutdown_signal)
            .await
            {
                tracing::warn!(error = %e, "panel http server stopped");
            }
        });
        *self.bound.write().await = Some(addr.clone());
        self.serving.store(true, Ordering::SeqCst);
        Ok(addr)
    }
}

#[async_trait]
impl PluginHandler for Panel {
    async fn configure(&self, config: Value) -> Result<(), String> {
        let cfg = PanelConfig::from_json(&config);
        cfg.validate()?;
        tracing::info!(
            listen = %cfg.listen,
            auth = cfg.auth.is_some(),
            "panel configured"
        );
        *self.cfg.write().await = cfg;
        Ok(())
    }

    fn contributions(&self) -> Vec<Contribution> {
        vec![Contribution::Service(ServiceContribution {
            key: PANEL_KEY.to_string(),
            kind: "web-panel".to_string(),
            version: 1,
        })]
    }

    async fn invoke(&self, req: InvokeRequest) -> InvokeOutcome {
        match req.contribution.as_str() {
            PANEL_KEY => {
                let cfg = self.cfg.read().await.clone();
                let bound = self.bound.read().await.clone();
                let up = self.serving.load(Ordering::SeqCst);
                InvokeOutcome::Ok {
                    text: String::new(),
                    images: Vec::new(),
                    result: json!({
                        "listen": bound.unwrap_or(cfg.listen),
                        "serving": up,
                        "auth": cfg.auth.is_some(),
                        "title": cfg.title,
                    }),
                }
            }
            other => InvokeOutcome::Error {
                code: "unknown_contribution".into(),
                message: format!("未知贡献 {other:?}"),
            },
        }
    }

    async fn on_ready(&self, host: HostClient) {
        // 只保存句柄：在这里直接 await 回呼会等读循环启动，可能自锁。
        if self.host.set(host.clone()).is_err() {
            return;
        }
        let cfg = self.cfg.read().await.clone();
        let backend: Arc<dyn ControlBackend> = Arc::new(HostCallBackend::new(host));
        match self.serve(backend, cfg.clone(), self.shutdown.clone()).await {
            Ok(addr) => {
                tracing::info!(addr = %addr, "panel http server listening");
            }
            Err(e) => {
                tracing::error!(listen = %cfg.listen, error = %e, "panel cannot bind http listener");
            }
        }
    }

    async fn on_cancel(&self, call_id: &str) {
        tracing::warn!(call_id, "panel call cancelled");
    }

    async fn on_drain(&self, deadline_ms: u64) {
        tracing::info!(deadline_ms, "panel draining");
        // 先礼貌地关掉 HTTP（正在传输的请求有机会写完），再让进程退出。
        self.serving.store(false, Ordering::SeqCst);
        self.shutdown.cancel();
        // axum 的 graceful shutdown 在独立任务里跑，这里给一点时间让它收尾。
        tokio::time::sleep(std::time::Duration::from_millis(deadline_ms.min(300))).await;
    }
}

/// 插件入口（二进制里调它）。
pub async fn run() -> std::io::Result<()> {
    serve_stdio_with(
        Panel::new(),
        ServeOptions {
            plugin_id: "esc.panel".into(),
            ..Default::default()
        },
    )
    .await
}
