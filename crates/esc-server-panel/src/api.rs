//! HTTP 层：把 `sys` 的采样和 `docker` 的查询拼成前端要的 JSON。
//!
//! 路由只有两件事：**只读的状态查询** 与 **容器启停**。没有删除、没有 exec、
//! 没有创建——面板的定位是「看见不对的东西并把它拉起来」，不是替代 Docker CLI。

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use esc_panel_auth::{AccessControl, Denied};

use crate::docker::{CreateSpec, Docker, DockerError, Jobs, NewNetwork};
use crate::plugins::PluginTreeClient;
use crate::sys::{self, Sampler};
use crate::web;

#[derive(Clone)]
pub struct AppState {
    pub sampler: Arc<Sampler>,
    pub docker: Arc<Docker>,
    pub title: String,
    pub access: Arc<AccessControl>,
    /// 是否允许启停容器。关掉就是纯只读监控。
    pub allow_actions: bool,
    /// 是否允许创建容器 / 拉镜像。
    pub allow_create: bool,
    /// 是否允许删除容器 / 镜像。这是不可撤销的操作，单独一道闸。
    pub allow_delete: bool,
    /// 后台任务（拉镜像等）。
    pub jobs: Arc<Jobs>,
    /// `None` = 未配置服务器核心上游；插件树标签页会明确显示配置方法。
    pub plugin_tree: Option<PluginTreeClient>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .route("/ui/{*path}", get(ui_asset))
        .route("/api/overview", get(api_overview))
        .route("/api/host", get(api_host))
        .route("/api/plugins", get(api_plugins))
        .route(
            "/api/docker/containers",
            get(api_containers).post(api_create_container),
        )
        .route(
            "/api/docker/containers/{id}",
            get(api_container_detail).delete(api_remove_container),
        )
        .route("/api/docker/containers/{id}/logs", get(api_container_logs))
        .route(
            "/api/docker/containers/{id}/stats",
            get(api_container_stats),
        )
        .route(
            "/api/docker/containers/{id}/{action}",
            post(api_container_action),
        )
        .route("/api/docker/images", get(api_images))
        .route("/api/docker/images/pull", post(api_pull_image))
        .route(
            "/api/docker/images/{name}",
            axum::routing::delete(api_remove_image),
        )
        .route(
            "/api/docker/networks",
            get(api_networks).post(api_create_network),
        )
        .route(
            "/api/docker/networks/{id}",
            get(api_network_detail).delete(api_remove_network),
        )
        .route(
            "/api/docker/networks/{id}/connect",
            post(api_connect_network),
        )
        .route(
            "/api/docker/networks/{id}/disconnect",
            post(api_disconnect_network),
        )
        .route("/api/docker/jobs", get(api_jobs))
        .route("/api/docker/jobs/{id}", get(api_job))
        .route(
            "/api/docker/containers/{id}/detail",
            get(api_container_detail_full),
        )
        .route("/api/docker/containers/{id}/rename", post(api_rename))
        .route(
            "/api/docker/containers/{id}/restart-policy",
            post(api_set_restart),
        )
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

/// 认证 / 白名单 / 限速，全部委托给共享的 `esc-panel-auth`。
async fn guard(
    State(st): State<AppState>,
    headers: HeaderMap,
    req: Request,
    next: Next,
) -> Response {
    // 只认 socket 来源，不看 X-Forwarded-For。
    let peer: Option<IpAddr> = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip());
    match st.access.check(&headers, peer) {
        Ok(()) => next.run(req).await,
        Err(Denied(resp)) => *resp,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 处理器
// ─────────────────────────────────────────────────────────────────────────────

async fn index(State(st): State<AppState>) -> Html<String> {
    Html(web::index_html(&st.title))
}
async fn ui_asset(Path(path): Path<String>) -> Response {
    match web::asset(&path) {
        Some(web::Asset::Css(body)) => {
            ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], body).into_response()
        }
        Some(web::Asset::JavaScript(body)) => (
            [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
            body,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn healthz() -> &'static str {
    "ok"
}

async fn api_plugins(State(st): State<AppState>) -> Response {
    let Some(client) = st.plugin_tree else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": "未配置服务器核心插件树上游。请在 server-panel.toml 设置 plugin_tree_url = \"http://127.0.0.1:8787/api/plugins\"；若隧道面板有认证，再设置 plugin_tree_auth = \"user:password\"。"
            })),
        )
            .into_response();
    };
    match client.fetch().await {
        Ok(tree) => Json(tree).into_response(),
        Err(error) => (
            axum::http::StatusCode::BAD_GATEWAY,
            Json(json!({ "error": error })),
        )
            .into_response(),
    }
}

/// 一次拿到「主机 + Docker 摘要」，前端每 3 秒轮询这一个端点就够了。
async fn api_overview(State(st): State<AppState>) -> Json<Value> {
    let sampler = st.sampler.clone();
    let (snap, sampling_error) = match tokio::task::spawn_blocking(move || sampler.sample()).await {
        Ok(s) => (s, None),
        Err(e) => {
            tracing::error!(error = %e, "采样任务失败");
            // 采样 panic 不该让整个接口 500，但**必须**告诉前端「这数据是空的，
            // 不是真的全 0」。静默给 0 会让人以为机器空闲，是最坏的一种降级。
            (emergency_snapshot(), Some(e.to_string()))
        }
    };

    let docker = docker_summary(&st.docker).await;

    Json(json!({
        "host": snap.host,
        "cpu": snap.cpu,
        "mem": snap.mem,
        "disks": snap.disks,
        "net": snap.net,
        "procs": snap.procs,
        "interval_ms": snap.interval_ms,
        "docker": docker,
        "allow_actions": st.allow_actions,
        "allow_create": st.allow_create,
        "allow_delete": st.allow_delete,
        "in_container": sys::is_containerised(),
        "version": env!("CARGO_PKG_VERSION"),
        "sampling_error": sampling_error,
    }))
}

async fn api_host(State(st): State<AppState>) -> Json<Value> {
    let sampler = st.sampler.clone();
    let snap = tokio::task::spawn_blocking(move || sampler.sample())
        .await
        .unwrap_or_else(|_| emergency_snapshot());
    Json(serde_json::to_value(snap).unwrap_or_else(|_| json!({})))
}

async fn api_containers(State(st): State<AppState>) -> Response {
    match st.docker.containers().await {
        Ok(list) => Json(json!({ "containers": list, "available": true })).into_response(),
        Err(e) => docker_error(e, "containers"),
    }
}

async fn api_container_detail(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    match st.docker.container(&id).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => docker_error(e, "detail"),
    }
}

#[derive(Debug, Deserialize)]
struct LogQuery {
    tail: Option<u32>,
    /// 只要这个时间点之后的行。**unix 秒**（可带小数）。
    ///
    /// 不用 RFC3339：Docker 29.8.x 对非法格式会静默忽略、退回「返回全部」，
    /// 于是前端的增量刷新会越滚越长。这里强制数字格式，传错就 400。
    since: Option<String>,
}

async fn api_container_logs(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<LogQuery>,
) -> Response {
    match st
        .docker
        .logs_since(&id, q.tail.unwrap_or(200), q.since.as_deref())
        .await
    {
        Ok(text) => Json(json!({ "text": text })).into_response(),
        Err(e) => docker_error(e, "logs"),
    }
}

async fn api_container_stats(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    match st.docker.stats(&id).await {
        Ok(s) => Json(json!(s)).into_response(),
        Err(e) => docker_error(e, "stats"),
    }
}

async fn api_images(State(st): State<AppState>) -> Response {
    match st.docker.images().await {
        Ok(list) => Json(json!({ "images": list })).into_response(),
        Err(e) => docker_error(e, "images"),
    }
}

async fn api_container_action(
    State(st): State<AppState>,
    Path((id, action)): Path<(String, String)>,
) -> Response {
    if !st.allow_actions {
        return (
            axum::http::StatusCode::FORBIDDEN,
            Json(json!({ "error": "面板配置为只读（allow_actions = false）" })),
        )
            .into_response();
    }
    tracing::info!(%id, %action, "面板请求容器操作");
    match st.docker.action(&id, &action).await {
        Ok(()) => Json(json!({ "ok": true, "id": id, "action": action })).into_response(),
        Err(e) => {
            tracing::warn!(%id, %action, error = %e, "容器操作失败");
            docker_error(e, "action")
        }
    }
}

/// 创建容器。`?pull=1` 时先拉镜像（镜像不存在会自动拉）。
///
/// 创建是唯一会「留下东西」的操作，所以它有两段式风险：
/// ① 镜像不存在 → 先拉；② 创建成功但启动失败 → 容器已经存在。
/// 后者把 id 带进错误信息里，否则使用者会以为什么都没发生。
async fn api_create_container(
    State(st): State<AppState>,
    Json(spec): Json<CreateSpec>,
) -> Response {
    if !st.allow_create {
        return forbidden("面板配置为只读（allow_create = false）");
    }
    if let Err(m) = spec.validate() {
        return bad_request(&m);
    }
    tracing::info!(image = %spec.image, name = ?spec.name, "面板请求创建容器");

    let docker = st.docker.clone();
    // 镜像不存在时先拉。放在同一个请求里是有意的：使用者点一次「创建」，
    // 期望的就是「能用」——让他先手动拉镜像属于把内部细节泄漏给使用者。
    let image = spec.image.clone();
    if !docker.image_exists(&image).await {
        tracing::info!(%image, "镜像不存在，先拉取");
        if let Err(e) = docker.pull_image(&image).await {
            return docker_error(e, "pull-before-create");
        }
    }
    match docker.create_container(&spec).await {
        Ok(id) => Json(json!({ "ok": true, "id": id })).into_response(),
        Err(e) => docker_error(e, "create"),
    }
}

async fn api_remove_container(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    if !st.allow_delete {
        return forbidden("面板配置不允许删除（allow_delete = false）");
    }
    tracing::warn!(%id, "面板请求删除容器");
    match st.docker.remove_container(&id, true).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => docker_error(e, "remove"),
    }
}

async fn api_container_detail_full(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    match st.docker.detail(&id).await {
        Ok(d) => Json(json!(d)).into_response(),
        Err(e) => docker_error(e, "detail"),
    }
}

#[derive(Debug, Deserialize)]
struct RenameBody {
    name: String,
}

async fn api_rename(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<RenameBody>,
) -> Response {
    if !st.allow_actions {
        return forbidden("面板配置为只读（allow_actions = false）");
    }
    let name = body.name.trim();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return bad_request("容器名只允许字母数字与 . _ -");
    }
    tracing::info!(%id, %name, "面板请求改容器名");
    match st.docker.rename(&id, name).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => docker_error(e, "rename"),
    }
}

#[derive(Debug, Deserialize)]
struct RestartBody {
    policy: String,
}

async fn api_set_restart(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<RestartBody>,
) -> Response {
    if !st.allow_actions {
        return forbidden("面板配置为只读（allow_actions = false）");
    }
    tracing::info!(%id, policy = %body.policy, "面板请求改重启策略");
    match st.docker.set_restart_policy(&id, &body.policy).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => docker_error(e, "restart-policy"),
    }
}

/// 网络列表。
///
/// 同时返回两份：`networks` 是完整形状（驱动 / 网段 / internal / 是否系统网络），
/// `names` 只有名字。后者是历史行为的兜底 —— 「创建容器」的下拉框和任何外部
/// 脚本都按 `names` 取值，不能让这次扩展把它们打断。
async fn api_networks(State(st): State<AppState>) -> Response {
    match st.docker.networks_detail().await {
        Ok(list) => {
            let names: Vec<&str> = list.iter().map(|n| n.name.as_str()).collect();
            Json(json!({ "networks": list, "names": names })).into_response()
        }
        Err(e) => docker_error(e, "networks"),
    }
}

/// 单个网络 + 它上面的成员容器。
///
/// 成员必须走这个端点：`GET /networks` 的返回里没有 `Containers` 字段，
/// 列表页拿不到「谁接在上面」。
async fn api_network_detail(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    match st.docker.network(&id).await {
        Ok((network, containers)) => {
            Json(json!({ "network": network, "containers": containers })).into_response()
        }
        Err(e) => docker_error(e, "network-detail"),
    }
}

async fn api_create_network(State(st): State<AppState>, Json(body): Json<NewNetwork>) -> Response {
    if !st.allow_create {
        return forbidden("面板配置不允许创建网络（allow_create = false）");
    }
    // 先校验再记日志：拒掉的请求不该在日志里留下「要建 xxx」的痕迹。
    if let Err(m) = body.validate() {
        return bad_request(&m);
    }
    tracing::info!(
        name = %body.name.trim(),
        internal = body.internal,
        subnet = body.subnet.as_deref().unwrap_or("(自动)"),
        "面板请求创建网络"
    );
    match st.docker.create_network(&body).await {
        Ok((id, warning)) => {
            Json(json!({ "ok": true, "id": id, "warning": warning })).into_response()
        }
        Err(e) => docker_error(e, "network-create"),
    }
}

async fn api_remove_network(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    if !st.allow_delete {
        return forbidden("面板配置不允许删除（allow_delete = false）");
    }
    // 删网络不可撤销，还会打断依赖它的容器间通信 —— 用 warn 留痕。
    tracing::warn!(%id, "面板请求删除网络");
    match st.docker.remove_network(&id).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => docker_error(e, "network-remove"),
    }
}

#[derive(Debug, Deserialize)]
struct NetworkLinkBody {
    /// 容器 ID 或容器名，Docker 自己解析两者。
    container: String,
    /// 接入时的固定 IPv4（留空由 Docker 分配）。
    #[serde(default)]
    ipv4: Option<String>,
    /// 断开时是否强制。
    ///
    /// 默认 **false**：运行中的容器被断开最后一个网络后会彻底失去网络，
    /// 这种后果必须由使用者显式确认，不能靠前端的默认值决定。
    #[serde(default)]
    force: bool,
}

/// 把容器接入网络。
///
/// 走 `allow_actions` 而不是 `allow_create`：它不新增任何东西，改的是
/// 既有容器的网络归属 —— 与「启停容器」同一档风险。
async fn api_connect_network(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<NetworkLinkBody>,
) -> Response {
    if !st.allow_actions {
        return forbidden("面板配置为只读（allow_actions = false）");
    }
    let container = body.container.trim();
    if container.is_empty() {
        return bad_request("container 不能为空");
    }
    tracing::info!(%id, %container, "面板请求把容器接入网络");
    match st
        .docker
        .connect_network(&id, container, body.ipv4.as_deref())
        .await
    {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => docker_error(e, "network-connect"),
    }
}

async fn api_disconnect_network(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<NetworkLinkBody>,
) -> Response {
    if !st.allow_actions {
        return forbidden("面板配置为只读（allow_actions = false）");
    }
    let container = body.container.trim();
    if container.is_empty() {
        return bad_request("container 不能为空");
    }
    tracing::info!(%id, %container, force = body.force, "面板请求把容器从网络断开");
    match st
        .docker
        .disconnect_network(&id, container, body.force)
        .await
    {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => docker_error(e, "network-disconnect"),
    }
}

#[derive(Debug, Deserialize)]
struct PullBody {
    image: String,
}

/// 拉镜像：**立刻返回 job id**，真正的等待在后台。
///
/// 为什么不同步等：大镜像可能拉十分钟，浏览器/代理早就超时了，而使用者
/// 看到的是「失败」——然后重试，于是两个拉取并行。给一个 job id 让前端
/// 轮询进度，就不会有这种误解。
async fn api_pull_image(State(st): State<AppState>, Json(body): Json<PullBody>) -> Response {
    if !st.allow_create {
        return forbidden("面板配置不允许拉取镜像（allow_create = false）");
    }
    let image = body.image.trim().to_string();
    if image.is_empty() {
        return bad_request("image 不能为空");
    }
    if !image
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '/' | ':' | '-' | '_' | '@'))
    {
        return bad_request("image 含非法字符");
    }

    let id = st.jobs.start("pull");
    let jobs = st.jobs.clone();
    let docker = st.docker.clone();
    let job_id = id.clone();
    let for_task = image.clone();
    tokio::spawn(async move {
        let result = match docker.pull_image(&for_task).await {
            Ok(m) => Ok(m),
            Err(e) => Err(e.to_string()),
        };
        jobs.finish(&job_id, result);
    });
    tracing::info!(%image, job = %id, "开始拉取镜像");
    Json(json!({ "ok": true, "job": id, "image": image })).into_response()
}

async fn api_remove_image(State(st): State<AppState>, Path(name): Path<String>) -> Response {
    if !st.allow_delete {
        return forbidden("面板配置不允许删除（allow_delete = false）");
    }
    tracing::warn!(%name, "面板请求删除镜像");
    match st.docker.remove_image(&name, true).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => docker_error(e, "image-remove"),
    }
}

async fn api_jobs(State(st): State<AppState>) -> Json<Value> {
    Json(json!({ "jobs": st.jobs.list() }))
}

async fn api_job(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    match st.jobs.get(&id) {
        Some(j) => Json(json!(j)).into_response(),
        None => (
            axum::http::StatusCode::NOT_FOUND,
            Json(json!({ "error": "任务不存在或已过期" })),
        )
            .into_response(),
    }
}

fn forbidden(msg: &str) -> Response {
    (
        axum::http::StatusCode::FORBIDDEN,
        Json(json!({ "error": msg })),
    )
        .into_response()
}

fn bad_request(msg: &str) -> Response {
    (
        axum::http::StatusCode::BAD_REQUEST,
        Json(json!({ "error": msg })),
    )
        .into_response()
}

// ─────────────────────────────────────────────────────────────────────────────
// Docker 状态摘要
// ─────────────────────────────────────────────────────────────────────────────

/// Docker 的三态：`unavailable`（socket 不在）/ `error`（在但连不上）/ `ok`。
///
/// 分开是为了让前端能说人话：「这台机器没装 Docker」和「装了但 daemon 挂了」
/// 是两种完全不同的处置。
async fn docker_summary(docker: &Docker) -> Value {
    if !docker.socket_exists() {
        return json!({
            "state": "unavailable",
            "socket": docker.socket_path().display().to_string(),
            "message": "未检测到 Docker socket（这台机器可能没装 Docker）",
        });
    }
    // 探活 + 取字段：`/info` 与 `/version` 的字段**不重叠**，两个都要。
    //
    //   /version → Version, ApiVersion, Os, Arch     （没有 RegistryConfig）
    //   /info    → ServerVersion, OperatingSystem, Architecture,
    //              RegistryConfig.Mirrors            （没有 ApiVersion）
    //
    // 早先只用 `/version` 探活，于是 mirrors 永远是空列表 —— 而「拉不动镜像」
    // 时第一个要看的就是它。现在主调用用 `/info`（带 mirrors），
    // ApiVersion 再单独问一次；后者失败无关紧要，不影响"可用"判定。
    match docker.info().await {
        Ok(v) => {
            let containers = docker.containers().await;
            let (total, running, stopped) = match &containers {
                Ok(list) => {
                    let running = list.iter().filter(|c| c.state == "running").count();
                    (list.len(), running, list.len() - running)
                }
                Err(_) => (0, 0, 0),
            };
            let mirrors: Vec<String> = v
                .get("RegistryConfig")
                .and_then(|r| r.get("Mirrors"))
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let api_version = docker
                .probe()
                .await
                .ok()
                .and_then(|ver| {
                    ver.get("ApiVersion")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_default();
            json!({
                "state": "ok",
                "socket": docker.socket_path().display().to_string(),
                "version": v.get("ServerVersion").and_then(Value::as_str).unwrap_or(""),
                "api_version": api_version,
                "os": v.get("OperatingSystem").and_then(Value::as_str).unwrap_or(""),
                "arch": v.get("Architecture").and_then(Value::as_str).unwrap_or(""),
                "containers_total": total,
                "containers_running": running,
                "containers_stopped": stopped,
                "mirrors": mirrors,
                "list_error": containers.err().map(|e| e.to_string()),
            })
        }
        Err(e) => json!({
            "state": "error",
            "socket": docker.socket_path().display().to_string(),
            "message": e.to_string(),
        }),
    }
}

fn docker_error(e: DockerError, context: &str) -> Response {
    use axum::http::StatusCode;
    let status = match &e {
        // 没装 Docker 不是「服务器错误」，是「这个功能在此环境不可用」。
        DockerError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
        DockerError::Failed { status, .. } => match status {
            404 => StatusCode::NOT_FOUND,
            304 => StatusCode::OK,
            // 我们自己合成的 504（请求 daemon 超时）原样透出：
            // 502 的意思是"上游返回了错误"，504 才是"上游没在期限内回答"，
            // 这两种情况运维的处置不一样。
            504 => StatusCode::GATEWAY_TIMEOUT,
            400..=499 => StatusCode::BAD_REQUEST,
            _ => StatusCode::BAD_GATEWAY,
        },
        DockerError::Protocol(_) => StatusCode::BAD_GATEWAY,
    };
    tracing::warn!(context, error = %e, "Docker 请求失败");
    (
        status,
        Json(json!({ "error": e.to_string(), "context": context })),
    )
        .into_response()
}

/// 采样任务 panic 时的兜底：宁可给一份「全 0 + 说明」，也不要把接口打成 500
/// 让整个面板白屏。
fn emergency_snapshot() -> crate::sys::Snapshot {
    use crate::sys::*;
    Snapshot {
        host: HostInfo {
            hostname: "unknown".into(),
            os: "unknown".into(),
            kernel: String::new(),
            arch: std::env::consts::ARCH.into(),
            uptime_secs: 0,
            cpu_count: 0,
        },
        cpu: CpuInfo {
            usage_pct: 0.0,
            per_core: Vec::new(),
            load1: 0.0,
            load5: 0.0,
            load15: 0.0,
            total_ticks: 0,
        },
        mem: MemInfo {
            total_bytes: 0,
            used_bytes: 0,
            free_bytes: 0,
            available_bytes: 0,
            buffers_bytes: 0,
            cached_bytes: 0,
            swap_total_bytes: 0,
            swap_used_bytes: 0,
            usage_pct: 0.0,
        },
        disks: Vec::new(),
        net: NetInfo {
            rx_total_bytes: 0,
            tx_total_bytes: 0,
            rx_per_sec: 0.0,
            tx_per_sec: 0.0,
            interfaces: Vec::new(),
        },
        procs: Vec::new(),
        interval_ms: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use esc_panel_auth::AuthConfig;

    fn state(readonly: bool, docker_sock: &str) -> AppState {
        AppState {
            sampler: Arc::new(Sampler::new()),
            docker: Arc::new(Docker::new(docker_sock)),
            title: "测试".into(),
            access: Arc::new(AccessControl::new(&AuthConfig::default())),
            allow_actions: readonly,
            allow_create: !readonly,
            allow_delete: !readonly,
            jobs: Arc::new(Jobs::new()),
            plugin_tree: None,
        }
    }

    #[test]
    fn router_builds_without_serving() {
        // 路由表拼错（比如 axum 0.8 的 {id} 语法）应在这里暴露，而不是部署后 404。
        let _ = router(state(false, "/nonexistent.sock"));
    }

    #[tokio::test]
    async fn unconfigured_plugin_tree_is_not_an_empty_tree() {
        let response = api_plugins(State(state(false, "/nonexistent.sock"))).await;
        assert_eq!(
            response.status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[test]
    fn emergency_snapshot_is_well_formed() {
        let s = emergency_snapshot();
        assert_eq!(s.host.hostname, "unknown");
        assert_eq!(s.mem.total_bytes, 0);
        assert!(s.disks.is_empty());
        assert!(serde_json::to_value(&s).is_ok());
    }
}
