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
async fn http(
    addr: &str,
    method: &str,
    path: &str,
    auth: Option<&str>,
) -> (u16, String) {
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

    let state = AppState {
        sampler: Arc::new(Sampler::new()),
        // 指向不存在的 socket：稳定复现「没装 Docker」
        docker: Arc::new(Docker::new("/nonexistent/docker.sock")),
        title: "服务器面板测试".into(),
        access: Arc::new(AccessControl::new(&auth)),
        allow_actions,
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

// ─────────────────────────────────────────────────────────────────────────────

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
    assert!(v["host"]["hostname"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(v["host"]["cpu_count"].as_u64().is_some_and(|n| n >= 1));
    assert!(v["mem"]["total_bytes"].as_u64().is_some_and(|n| n > 0), "内存总量应有值");
    assert!(v["cpu"]["usage_pct"].as_f64().is_some(), "CPU% 应存在");
    assert!(v["disks"].as_array().is_some_and(|a| !a.is_empty()), "至少有一个挂载");
    assert!(v["procs"].as_array().is_some());
    assert!(v["version"].as_str().is_some());
    // Docker 部分：用的是不存在的 socket，必须是 unavailable
    assert_eq!(v["docker"]["state"], "unavailable");
    assert!(v["docker"]["message"].as_str().unwrap_or("").contains("Docker"));
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
    let (code, body) = http(
        &r.addr,
        "POST",
        "/api/docker/containers/abc/start",
        None,
    )
    .await;
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
