//! 服务器面板 HTTP 层测试：走**真实 socket**，因为来源白名单与限速依赖
//! `ConnectInfo`（socket 对端地址）——用 `oneshot` 直接喂 `Request` 是测不到的。
//!
//! Docker 部分用一个指向不存在路径的 socket，从而稳定地验证「未安装 Docker」
//! 这条分支（这台 CI 机器上装没装 Docker 都不影响结果）。

use std::net::SocketAddr;
use std::sync::Arc;

use esc_panel_auth::{AccessControl, AuthConfig};
use esc_server_panel::api::{router, AppState};
use esc_server_panel::docker::Docker;
use esc_server_panel::sys::Sampler;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

/// 极简 HTTP/1.1 客户端：`Connection: close` + 读到 EOF，不需要解析 chunked。
async fn http(addr: &str, method: &str, path: &str, auth: Option<&str>) -> (u16, String) {
    let mut sock = tokio::net::TcpStream::connect(addr)
        .await
        .unwrap_or_else(|e| panic!("connect {addr}: {e}"));
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Length: 0\r\n"
    );
    if let Some(a) = auth {
        req.push_str(&format!("Authorization: Basic {a}\r\n"));
    }
    req.push_str("\r\n");
    sock.write_all(req.as_bytes()).await.unwrap();

    let mut raw = Vec::new();
    sock.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw).to_string();
    let status = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("无法解析状态行: {text}"));
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

/// 带 JSON body 的 `http()`：写端点要用它，否则 axum 的 `Json` 提取器
/// 会先因为缺 Content-Type 回一个 415，测不到我们自己的逻辑。
async fn http_json(addr: &str, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut sock = tokio::net::TcpStream::connect(addr)
        .await
        .unwrap_or_else(|e| panic!("connect {addr}: {e}"));
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    sock.write_all(req.as_bytes()).await.unwrap();

    let mut raw = Vec::new();
    sock.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw).to_string();
    let status = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("无法解析状态行: {text}"));
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

fn basic(user_pass: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(user_pass)
}

struct Running {
    addr: String,
    token: CancellationToken,
}

/// 起一个面板。`tweak` 用来改认证/白名单之类的配置。
async fn start(tweak: impl FnOnce(&mut AuthConfig, &mut bool)) -> Running {
    let mut auth = AuthConfig::default();
    let mut allow_actions = true;
    tweak(&mut auth, &mut allow_actions);

    launch(auth, allow_actions, allow_actions, allow_actions).await
}

/// 起一个面板，**三档权限各给各的**。
///
/// `start()` 把它们绑成同一个开关（老测试只需要全开或全关）；网络这块要验证
/// 「允许读、允许建、但不允许删」这类组合，所以需要一个更细的入口。
async fn start_perm(allow_actions: bool, allow_create: bool, allow_delete: bool) -> Running {
    launch(
        AuthConfig::default(),
        allow_actions,
        allow_create,
        allow_delete,
    )
    .await
}

async fn launch(
    auth: AuthConfig,
    allow_actions: bool,
    allow_create: bool,
    allow_delete: bool,
) -> Running {
    let state = AppState {
        sampler: Arc::new(Sampler::new()),
        // 指向不存在的 socket：稳定复现「没装 Docker」
        docker: Arc::new(Docker::new("/nonexistent/docker.sock")),
        title: "服务器面板测试".into(),
        access: Arc::new(AccessControl::new(&auth)),
        allow_actions,
        allow_create,
        allow_delete,
        jobs: Arc::new(esc_server_panel::docker::Jobs::new()),
        plugin_tree: None,
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let token = CancellationToken::new();
    let shutdown = token.clone();
    let app = router(state).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    Running {
        addr: addr.to_string(),
        token,
    }
}

#[tokio::test]
async fn index_is_served_with_title_and_escaping() {
    let r = start(|a, _| a.auth = Some("admin:pw".into())).await;
    let (code, body) = http(&r.addr, "GET", "/", Some(&basic("admin:pw"))).await;
    assert_eq!(code, 200, "{body}");
    assert!(body.contains("<!DOCTYPE html>"));
    assert!(body.contains("服务器面板测试"), "标题应被替换");
    assert!(!body.contains("__TITLE__"), "占位符必须被替换掉");
    r.token.cancel();
}

#[tokio::test]
async fn auth_is_enforced_on_every_route_including_healthz() {
    let r = start(|a, _| a.auth = Some("admin:pw".into())).await;

    for path in ["/", "/healthz", "/api/overview", "/api/docker/containers"] {
        let (code, _) = http(&r.addr, "GET", path, None).await;
        assert_eq!(code, 401, "{path} 未认证应 401");
    }
    let (code, body) = http(&r.addr, "GET", "/healthz", Some(&basic("admin:wrong"))).await;
    assert_eq!(code, 401);
    assert!(body.contains("需要认证"));

    let (code, _) = http(&r.addr, "GET", "/healthz", Some(&basic("admin:pw"))).await;
    assert_eq!(code, 200);
    r.token.cancel();
}

#[tokio::test]
async fn overview_reports_host_fields_and_docker_state() {
    let r = start(|a, _| a.auth = Some("admin:pw".into())).await;
    let (code, body) = http(&r.addr, "GET", "/api/overview", Some(&basic("admin:pw"))).await;
    assert_eq!(code, 200, "{body}");

    let v: serde_json::Value = serde_json::from_str(&body).expect("必须是合法 JSON");
    // 主机部分
    assert!(v["host"]["hostname"]
        .as_str()
        .is_some_and(|s| !s.is_empty()));
    assert!(v["host"]["cpu_count"].as_u64().is_some_and(|n| n >= 1));
    assert!(
        v["mem"]["total_bytes"].as_u64().is_some_and(|n| n > 0),
        "内存总量应有值"
    );
    assert!(v["cpu"]["usage_pct"].as_f64().is_some(), "CPU% 应存在");
    assert!(
        v["disks"].as_array().is_some_and(|a| !a.is_empty()),
        "至少有一个挂载"
    );
    assert!(v["procs"].as_array().is_some());
    assert!(v["version"].as_str().is_some());
    // Docker 部分：用的是不存在的 socket，必须是 unavailable
    assert_eq!(v["docker"]["state"], "unavailable");
    assert!(v["docker"]["message"]
        .as_str()
        .unwrap_or("")
        .contains("Docker"));
    assert_eq!(v["allow_actions"], true);
    r.token.cancel();
}

#[tokio::test]
async fn host_endpoint_returns_the_snapshot_directly() {
    let r = start(|_, _| {}).await; // 无认证（回环测试环境）
    let (code, body) = http(&r.addr, "GET", "/api/host", None).await;
    assert_eq!(code, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(v["host"]["hostname"].as_str().is_some());
    r.token.cancel();
}

#[tokio::test]
async fn docker_endpoints_degrade_to_503_when_docker_is_absent() {
    let r = start(|_, _| {}).await;

    // 「没装 Docker」应当是 503（服务不可用），而不是 500（我们的 bug）
    let (code, body) = http(&r.addr, "GET", "/api/docker/containers", None).await;
    assert_eq!(code, 503, "实际 {code}: {body}");
    assert!(body.contains("Docker 不可用"), "{body}");

    let (code, _) = http(&r.addr, "GET", "/api/docker/images", None).await;
    assert_eq!(code, 503);

    // 容器动作也用同一个 socket：应报不可用，且不能是 200
    let (code, body) = http(&r.addr, "POST", "/api/docker/containers/abc/start", None).await;
    assert_eq!(code, 503, "实际 {code}: {body}");
    r.token.cancel();
}

#[tokio::test]
async fn actions_are_refused_when_the_panel_is_read_only() {
    let r = start(|_, allow_actions| *allow_actions = false).await;

    let (code, body) = http(&r.addr, "POST", "/api/docker/containers/abc/start", None).await;
    assert_eq!(code, 403, "只读模式应拒绝所有动作，实际 {code}: {body}");
    assert!(body.contains("只读"), "{body}");

    // 只读不影响读取类接口
    let (code, _) = http(&r.addr, "GET", "/api/overview", None).await;
    assert_eq!(code, 200);
    r.token.cancel();
}

#[tokio::test]
async fn allow_list_blocks_foreign_sources() {
    // 回环不在白名单里 → 连认证都别想过
    let r = start(|a, _| {
        a.auth = Some("admin:pw".into());
        a.allow_cidrs = vec!["10.0.0.0/8".into()];
    })
    .await;
    let (code, body) = http(&r.addr, "GET", "/api/overview", Some(&basic("admin:pw"))).await;
    assert_eq!(code, 403, "实际 {code}: {body}");
    assert!(body.contains("白名单"));
    r.token.cancel();

    // 把回环段加进去 → 放行
    let r = start(|a, _| {
        a.auth = Some("admin:pw".into());
        a.allow_cidrs = vec!["127.0.0.0/8".into()];
    })
    .await;
    let (code, _) = http(&r.addr, "GET", "/api/overview", Some(&basic("admin:pw"))).await;
    assert_eq!(code, 200);
    r.token.cancel();
}

#[tokio::test]
async fn wrong_passwords_exhaust_the_budget_and_then_429_even_with_the_right_one() {
    let r = start(|a, _| {
        a.auth = Some("admin:pw".into());
        a.max_auth_failures = 3;
        a.auth_fail_window_secs = 60;
    })
    .await;

    for i in 0..3 {
        let (code, _) = http(&r.addr, "GET", "/healthz", Some(&basic("admin:no"))).await;
        assert_eq!(code, 401, "第 {} 次错误口令", i + 1);
    }
    // 预算用尽后，正确口令也要被挡——否则限速形同虚设
    let (code, body) = http(&r.addr, "GET", "/healthz", Some(&basic("admin:pw"))).await;
    assert_eq!(code, 429, "实际 {code}: {body}");
    r.token.cancel();
}

#[tokio::test]
async fn unknown_container_action_is_rejected_before_reaching_docker() {
    let r = start(|_, _| {}).await;
    // 即使 Docker 不可用，"不支持的动作"也应该先被允许清单挡下
    let (code, body) = http(&r.addr, "POST", "/api/docker/containers/abc/exec", None).await;
    assert_eq!(code, 400, "未知动作应 400，实际 {code}: {body}");
    assert!(body.contains("不支持的操作"), "{body}");
    r.token.cancel();
}

#[tokio::test]
async fn large_tail_value_is_clamped_not_trusted() {
    // tail 会被 clamp 到 [1, 2000]；这里只验证接口不因为荒谬的 tail 而炸掉
    let r = start(|_, _| {}).await;
    let (code, _) = http(
        &r.addr,
        "GET",
        "/api/docker/containers/abc/logs?tail=999999999",
        None,
    )
    .await;
    assert_eq!(code, 503, "Docker 不在时仍是 503（说明参数已通过校验）");
    r.token.cancel();
}

#[tokio::test]
async fn stop_uses_a_longer_timeout_than_the_grace_period() {
    // 回归：真实部署时 `stop` 报过 504——因为 HTTP 超时 5s < docker 的
    // `t=10` 优雅期。这条用例不能真的去停容器（测试机未必有 docker），
    // 所以验证的是「面板对 stop 的等待时间足够」这一常量关系，
    // 以及超时会被映射成 504 而不是 502。
    //
    // 常量关系在 docker.rs 的单测里断言（action_timeout_exceeds_...）；
    // 这里补的是 API 层：一个不存在的 daemon 走完全部流程仍然是 503。
    let r = start(|_, _| {}).await;
    let (code, _) = http(&r.addr, "POST", "/api/docker/containers/abc/stop", None).await;
    assert_eq!(
        code, 503,
        "Docker 不在时 stop 应为 503（不可用）而非 5xx 混淆"
    );
    r.token.cancel();
}

#[tokio::test]
async fn plugin_tree_tab_and_unconfigured_state_are_honest() {
    let r = start(|_, _| {}).await;
    let (code, body) = http(&r.addr, "GET", "/", None).await;
    assert_eq!(code, 200);
    assert!(body.contains("/ui/server.js"));
    assert!(body.contains("id=\"app\""));
    let (code, shared) = http(&r.addr, "GET", "/ui/panel.js", None).await;
    assert_eq!(code, 200);
    assert!(shared.contains("/api/plugins"));
    assert!(shared.contains("服务器核心插件树"));

    let (code, body) = http(&r.addr, "GET", "/api/plugins", None).await;
    assert_eq!(code, 503, "未配置上游不能伪装成空插件树：{body}");
    assert!(
        body.contains("plugin_tree_url"),
        "错误应给出配置入口：{body}"
    );
    r.token.cancel();
}

#[tokio::test]
async fn configured_plugin_tree_is_proxied_from_the_core_panel() {
    use esc_server_panel::plugins::PluginTreeClient;

    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream.local_addr().unwrap();
    let app = axum::Router::new().route(
        "/api/plugins",
        axum::routing::get(|| async {
            axum::Json(serde_json::json!({
                "schema_version": 1,
                "host": "esc-host",
                "framework": "plugin-framework",
                "context_services": ["esc.tunnel.control"],
                "host_services": ["esc.control", "esc.plugins"],
                "plugins": [{
                    "id": "esc.tunnel", "name": "隧道运行时", "version": "1",
                    "kind": "tunnel", "description": "", "runtime": "in_process",
                    "enabled": true, "mounted": true, "state": "ready", "children": []
                }]
            }))
        }),
    );
    tokio::spawn(async move {
        let _ = axum::serve(upstream, app).await;
    });

    let client =
        PluginTreeClient::new(&format!("http://{upstream_addr}/api/plugins"), None).unwrap();
    let state = AppState {
        sampler: Arc::new(Sampler::new()),
        docker: Arc::new(Docker::new("/nonexistent/docker.sock")),
        title: "服务器面板测试".into(),
        access: Arc::new(AccessControl::new(&AuthConfig::default())),
        allow_actions: false,
        allow_create: false,
        allow_delete: false,
        jobs: Arc::new(esc_server_panel::docker::Jobs::new()),
        plugin_tree: Some(client),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let token = CancellationToken::new();
    let shutdown = token.clone();
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            router(state).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await;
    });

    let (code, body) = http(&addr, "GET", "/api/plugins", None).await;
    assert_eq!(code, 200, "{body}");
    let tree: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(tree["host"], "esc-host");
    assert_eq!(tree["plugins"][0]["id"], "esc.tunnel");
    token.cancel();
}

// ── Docker 网络 ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn network_reads_degrade_to_503_when_docker_is_absent() {
    let r = start(|_, _| {}).await;
    for path in ["/api/docker/networks", "/api/docker/networks/bridge"] {
        let (code, body) = http(&r.addr, "GET", path, None).await;
        // 缺 Docker 是 503（此环境没这个功能），不能是 500（我们的 bug）
        assert_eq!(code, 503, "{path} 实际 {code}: {body}");
        assert!(body.contains("Docker 不可用"), "{path}: {body}");
    }
    r.token.cancel();
}

#[tokio::test]
async fn creating_a_network_needs_allow_create() {
    let r = start_perm(true, false, true).await;
    let (code, body) =
        http_json(&r.addr, "POST", "/api/docker/networks", r#"{"name":"x-net"}"#).await;
    assert_eq!(code, 403, "allow_create=false 应拒绝创建网络，实际 {code}: {body}");
    assert!(body.contains("allow_create"), "{body}");
    r.token.cancel();
}

#[tokio::test]
async fn deleting_a_network_needs_allow_delete_and_reads_do_not() {
    let r = start_perm(true, true, false).await;

    let (code, body) = http(&r.addr, "DELETE", "/api/docker/networks/abc", None).await;
    assert_eq!(code, 403, "allow_delete=false 应拒绝删除网络，实际 {code}: {body}");
    assert!(body.contains("不允许删除"), "{body}");

    // 同一组合下读接口不受影响（Docker 缺席所以是 503，但**不是** 403）
    let (code, _) = http(&r.addr, "GET", "/api/docker/networks", None).await;
    assert_eq!(code, 503, "只有写操作该被权限闸拦住");
    r.token.cancel();
}

#[tokio::test]
async fn network_link_operations_follow_allow_actions() {
    let r = start_perm(false, true, true).await;

    for action in ["connect", "disconnect"] {
        let (code, body) = http_json(
            &r.addr,
            "POST",
            &format!("/api/docker/networks/abc/{action}"),
            r#"{"container":"web"}"#,
        )
        .await;
        assert_eq!(
            code, 403,
            "allow_actions=false 应拒绝 {action}，实际 {code}: {body}"
        );
    }
    r.token.cancel();
}

#[tokio::test]
async fn invalid_network_names_are_rejected_before_docker_is_touched() {
    // 这个面板的 socket 不存在：任何真的打到 Docker 的请求都会变成 503。
    // 因此「拿到 400」本身就证明校验发生在 socket 之前。
    let r = start_perm(true, true, true).await;

    for bad in ["", "   ", "-leading", "has space", "中文名"] {
        let (code, body) = http_json(
            &r.addr,
            "POST",
            "/api/docker/networks",
            &serde_json::json!({ "name": bad }).to_string(),
        )
        .await;
        assert_eq!(code, 400, "非法网络名 {bad:?} 应被拒，实际 {code}: {body}");
        assert!(!body.contains("Docker 不可用"), "不该走到 Docker 那一层：{body}");
    }

    // 合法入参则应继续走到 Docker —— 于是表现为 503，说明前面的 400 不是运气
    let (code, body) = http_json(
        &r.addr,
        "POST",
        "/api/docker/networks",
        r#"{"name":"ok-net","internal":true,"subnet":"172.30.0.0/24","gateway":"172.30.0.1"}"#,
    )
    .await;
    assert_eq!(code, 503, "合法请求应到达 Docker 层，实际 {code}: {body}");
    r.token.cancel();
}

#[tokio::test]
async fn bad_subnets_are_rejected_with_an_explanation() {
    let r = start_perm(true, true, true).await;

    for (subnet, why) in [
        ("172.30.0.0/16", "前缀过大"),
        ("172.30.0.0/30", "前缀过小"),
        ("172.30.0.7/24", "不是网络地址"),
        ("nonsense", "格式错误"),
    ] {
        let (code, body) = http_json(
            &r.addr,
            "POST",
            "/api/docker/networks",
            &serde_json::json!({ "name": "n1", "subnet": subnet }).to_string(),
        )
        .await;
        assert_eq!(code, 400, "{why}：{subnet} 应被拒，实际 {code}: {body}");
    }

    // 网关跑出子网之外
    let (code, body) = http_json(
        &r.addr,
        "POST",
        "/api/docker/networks",
        r#"{"name":"n1","subnet":"172.30.0.0/24","gateway":"10.0.0.1"}"#,
    )
    .await;
    assert_eq!(code, 400, "网关不在子网内应被拒，实际 {code}: {body}");
    r.token.cancel();
}

#[tokio::test]
async fn network_link_without_a_container_is_a_400() {
    let r = start_perm(true, true, true).await;
    let (code, body) = http_json(
        &r.addr,
        "POST",
        "/api/docker/networks/abc/connect",
        r#"{"container":"   "}"#,
    )
    .await;
    assert_eq!(code, 400, "空容器标识应被拒，实际 {code}: {body}");
    r.token.cancel();
}
