//! 端到端测试：**真**上游 + **真**网关 + **真** TCP。
//!
//! 不用 `oneshot` 直接喂 `Request`：这个 crate 的价值有一半在"线上行为"上
//! （cookie 是否真的被浏览器带上、状态码是否符合预期、hop-by-hop 头是否真的
//! 被剥掉），这些都必须在真实 socket 上看。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use esc_gateway::config::GatewayConfig;
use esc_gateway::Gateway;

// ─────────────────────────────────────────────────────────────────────────────
// 测试脚手架
// ─────────────────────────────────────────────────────────────────────────────

/// 一个极简 HTTP/1.1 响应。
#[derive(Debug, Clone)]
struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
    fn set_cookie(&self) -> Option<&str> {
        self.header("set-cookie")
    }
}

/// 发一个请求。`headers` 里可以放 Cookie 之类。
async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Resp {
    let mut sock = tokio::net::TcpStream::connect(addr)
        .await
        .unwrap_or_else(|e| panic!("connect {addr}: {e}"));
    let body = body.unwrap_or("");
    // 只在调用方没自己指定 Connection 时才加 close ——
    // 否则会出现两条 Connection 头，hyper 只会认其中一条。
    let has_conn = headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("connection"));
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\n", body.len());
    if !has_conn {
        req.push_str("Connection: close\r\n");
    }
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    sock.write_all(req.as_bytes()).await.unwrap();

    let mut raw = Vec::new();
    sock.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("无法解析状态行: {text}"));
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    Resp {
        status,
        headers,
        body: body.to_string(),
    }
}

/// 起一个假上游：回显请求信息，便于断言"上游看到了什么"。
async fn start_upstream() -> (SocketAddr, CancellationToken) {
    let app = Router::new()
        .route("/", any(echo))
        .route("/{*rest}", any(echo));

    async fn echo(req: Request) -> Response {
        let method = req.method().to_string();
        let uri = req.uri().to_string();
        let headers: Vec<String> = req
            .headers()
            .iter()
            .map(|(k, v)| format!("{}: {}", k, v.to_str().unwrap_or("<bin>")))
            .collect();
        let body = axum::body::to_bytes(req.into_body(), 1 << 20)
            .await
            .map(|b| String::from_utf8_lossy(&b).to_string())
            .unwrap_or_default();
        let out = serde_json::json!({
            "method": method,
            "uri": uri,
            "headers": headers,
            "body": body,
        });
        ([("content-type", "application/json")], out.to_string()).into_response()
    }

    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let token = CancellationToken::new();
    let shut = token.clone();
    tokio::spawn(async move {
        let _ = axum::serve(l, app)
            .with_graceful_shutdown(async move { shut.cancelled().await })
            .await;
    });
    tokio::time::sleep(Duration::from_millis(40)).await;
    (addr, token)
}

struct Gw {
    /// route 名 → 真实监听地址（配置里写的是占位端口，实际绑 :0）
    addrs: std::collections::HashMap<String, SocketAddr>,
    token: CancellationToken,
}

impl Gw {
    fn addr(&self) -> SocketAddr {
        self.addrs["panel"]
    }
    fn addr_of(&self, route: &str) -> SocketAddr {
        self.addrs[route]
    }
}

/// 起网关。给定上游地址与两点自定义：额外 route、限速放宽。
async fn start_gateway(upstream: SocketAddr, tweak: impl FnOnce(&mut GatewayConfig)) -> Gw {
    let hash = esc_gateway::config::hash_password("pw-123").unwrap();
    let mut cfg: GatewayConfig = toml::from_str(&format!(
        r#"
title = "测试网关"
session_ttl_hours = 1
cookie_name = "esc_gw"

[rate_limit]
max_failures = 3
fail_window_secs = 60
max_failures_global = 1000
global_window_secs = 60

[[user]]
name = "admin"
hash = "{hash}"
routes = ["*"]

[[user]]
name = "viewer"
hash = "{hash}"
routes = ["panel"]

[[route]]
name = "panel"
listen = "127.0.0.1:0"
upstream = "{upstream}"

[[route]]
name = "admin-only"
listen = "127.0.0.1:1"
upstream = "{upstream}"
"#
    ))
    .unwrap();
    tweak(&mut cfg);

    let rt = cfg.build().expect("测试配置应合法");
    let gw = Arc::new(Gateway::new(Arc::new(rt)));
    let token = CancellationToken::new();

    // 手动绑定（listen 写的是 :0，需要真实的端口）
    let mut listens = Vec::new();
    for r in gw.cfg.routes.values() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut r2 = r.clone();
        r2.listen = l.local_addr().unwrap();
        listens.push((r2, l));
    }
    let mut addrs = std::collections::HashMap::new();
    for (r, l) in listens {
        addrs.insert(r.name.clone(), r.listen);
        let gw = gw.clone();
        let shut = token.clone();
        tokio::spawn(async move { esc_gateway::proxy::serve_route(gw, r, l, shut).await });
    }
    tokio::time::sleep(Duration::from_millis(40)).await;
    Gw { addrs, token }
}

/// 登录，返回 session cookie。
async fn login(gw: &Gw, user: &str, pass: &str) -> Option<String> {
    let body = format!("user={user}&pass={pass}&next=/");
    let r = http(
        gw.addr(),
        "POST",
        "/__auth/login",
        &[("content-type", "application/x-www-form-urlencoded")],
        Some(&body),
    )
    .await;
    if r.status != 303 {
        return None;
    }
    r.set_cookie()
        .and_then(|c| c.split(';').next())
        .map(str::to_string)
}

/// 在指定端口登录（多 route 场景下需要）。
async fn login_at(
    _gw: &Gw,
    addr: SocketAddr,
    user: &str,
    pass: &str,
) -> Option<String> {
    let body = format!("user={user}&pass={pass}&next=/");
    let r = http(
        addr,
        "POST",
        "/__auth/login",
        &[("content-type", "application/x-www-form-urlencoded")],
        Some(&body),
    )
    .await;
    if r.status != 303 {
        return None;
    }
    r.set_cookie()
        .and_then(|c| c.split(';').next())
        .map(str::to_string)
}

// ─────────────────────────────────────────────────────────────────────────────
// 未认证
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn browser_navigation_is_redirected_to_login() {
    let (up, _u) = start_upstream().await;
    let gw = start_gateway(up, |_| {}).await;

    // 浏览器导航：带 Sec-Fetch-Mode
    let r = http(
        gw.addr(),
        "GET",
        "/api/status",
        &[("sec-fetch-mode", "navigate")],
        None,
    )
    .await;
    assert_eq!(r.status, 302, "浏览器应被跳到登录页");
    let loc = r.header("location").unwrap();
    assert!(loc.starts_with("/__auth/login?next="), "{loc}");
    // `/` 在 query string 里不必编码；`?` 与 `&` 会（否则参数会被拆开）
    assert!(loc.contains("/api/status"), "next 应带回原路径: {loc}");

    // 老浏览器没有 Sec-Fetch-Mode，看 Accept
    let r = http(gw.addr(), "GET", "/x", &[("accept", "text/html")], None).await;
    assert_eq!(r.status, 302);
    gw.token.cancel();
    _u.cancel();
}

#[tokio::test]
async fn api_calls_get_401_json_not_a_redirect() {
    let (up, _u) = start_upstream().await;
    let gw = start_gateway(up, |_| {}).await;

    // 面板的 fetch 默认 Accept: */*，不能当成浏览器导航
    let r = http(gw.addr(), "GET", "/api/status", &[("accept", "*/*")], None).await;
    assert_eq!(r.status, 401, "API 应拿 401 而不是 302");
    assert_eq!(r.header("content-type"), Some("application/json"));
    let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
    assert!(v["error"].as_str().unwrap().contains("登录"));

    // 带 json 的 Accept 也一样
    let r = http(
        gw.addr(),
        "GET",
        "/api/mappings",
        &[("accept", "application/json")],
        None,
    )
    .await;
    assert_eq!(r.status, 401);
    gw.token.cancel();
    _u.cancel();
}

#[tokio::test]
async fn login_page_is_served_without_cookie() {
    let (up, _u) = start_upstream().await;
    let gw = start_gateway(up, |_| {}).await;

    let r = http(gw.addr(), "GET", "/__auth/login", &[], None).await;
    assert_eq!(r.status, 200);
    assert!(r.body.contains("<!DOCTYPE html>"));
    assert!(r.body.contains(r#"action="/__auth/login""#));
    assert!(r.body.contains("测试网关"), "标题应来自配置");

    // next 会作为隐藏字段
    let r = http(gw.addr(), "GET", "/__auth/login?next=%2Fapi%2Fstatus", &[], None).await;
    assert!(r.body.contains(r#"value="/api/status""#), "{}", r.body);

    // 开放重定向在登录页就该被中和
    let r = http(gw.addr(), "GET", "/__auth/login?next=%2F%2Fevil.com", &[], None).await;
    assert!(!r.body.contains("evil.com"), "协议相对 URL 应被中和: {}", r.body);

    gw.token.cancel();
    _u.cancel();
}

#[tokio::test]
async fn healthz_works_without_auth_for_probes() {
    let (up, _u) = start_upstream().await;
    let gw = start_gateway(up, |_| {}).await;
    let r = http(gw.addr(), "GET", "/__auth/healthz", &[], None).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body, "ok");
    gw.token.cancel();
    _u.cancel();
}

// ─────────────────────────────────────────────────────────────────────────────
// 登录与转发
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn login_sets_httponly_strict_cookie_and_then_proxies() {
    let (up, _u) = start_upstream().await;
    let gw = start_gateway(up, |_| {}).await;

    let cookie = login(&gw, "admin", "pw-123").await.expect("登录应成功");

    // cookie 属性
    let r = http(
        gw.addr(),
        "POST",
        "/__auth/login",
        &[("content-type", "application/x-www-form-urlencoded")],
        Some("user=admin&pass=pw-123&next=/"),
    )
    .await;
    let sc = r.set_cookie().unwrap();
    assert!(sc.contains("HttpOnly"), "{sc}");
    assert!(sc.contains("SameSite=Strict"), "{sc}");
    assert!(sc.contains("Path=/"), "{sc}");

    // 带上 cookie 后能拿到上游内容
    let r = http(gw.addr(), "GET", "/api/status?x=1", &[("cookie", &cookie)], None).await;
    assert_eq!(r.status, 200, "{}", r.body);
    let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(v["method"], "GET");
    assert_eq!(v["uri"], "/api/status?x=1", "路径与查询串应原样转给上游");

    gw.token.cancel();
    _u.cancel();
}

#[tokio::test]
async fn upstream_sees_rewritten_host_and_xff_but_not_the_cookie() {
    let (up, _u) = start_upstream().await;
    let gw = start_gateway(up, |_| {}).await;
    let cookie = login(&gw, "admin", "pw-123").await.unwrap();

    let r = http(
        gw.addr(),
        "GET",
        "/whoami",
        &[("cookie", &cookie), ("x-forwarded-for", "6.6.6.6")],
        None,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
    let headers: Vec<String> = serde_json::from_value(v["headers"].clone()).unwrap();
    let find = |n: &str| {
        headers
            .iter()
            .find(|h| h.to_ascii_lowercase().starts_with(&format!("{n}:")))
            .cloned()
    };

    // Host 被重写成上游地址（客户端无法用 Host 影响上游）
    let host = find("host").expect("应有 Host");
    assert!(host.contains(&up.to_string()), "Host 应被重写为上游: {host}");

    // 客户端伪造的 XFF 必须被覆盖成真实来源 127.0.0.1
    let xff = find("x-forwarded-for").expect("应有 XFF");
    assert!(xff.contains("127.0.0.1"), "XFF 应被覆盖为真实来源: {xff}");
    assert!(!xff.contains("6.6.6.6"), "客户端伪造的 XFF 不能透传: {xff}");

    // session cookie 不该泄露给上游
    assert!(
        find("cookie").is_none(),
        "session cookie 不应转发给上游: {:?}",
        find("cookie")
    );

    gw.token.cancel();
    _u.cancel();
}

#[tokio::test]
async fn hop_by_hop_headers_are_stripped_before_reaching_upstream() {
    let (up, _u) = start_upstream().await;
    let gw = start_gateway(up, |_| {}).await;
    let cookie = login(&gw, "admin", "pw-123").await.unwrap();

    let r = http(
        gw.addr(),
        "GET",
        "/h",
        &[
            ("cookie", &cookie),
            // 显式带 close：既告诉服务端用完就关，又把 X-Secret 标成 hop-by-hop
            ("connection", "close, X-Secret"),
            ("x-secret", "should-not-pass"),
            ("te", "trailers"),
            ("x-normal", "should-pass"),
        ],
        None,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
    let headers: Vec<String> = serde_json::from_value(v["headers"].clone()).unwrap();
    let has = |n: &str| {
        headers
            .iter()
            .any(|h| h.to_ascii_lowercase().starts_with(&format!("{n}:")))
    };
    assert!(!has("x-secret"), "Connection 点名的头必须剥掉");
    assert!(!has("te"), "TE 是 hop-by-hop");
    assert!(has("x-normal"), "普通头应保留");
    gw.token.cancel();
    _u.cancel();
}

#[tokio::test]
async fn post_body_is_proxied_intact() {
    let (up, _u) = start_upstream().await;
    let gw = start_gateway(up, |_| {}).await;
    let cookie = login(&gw, "admin", "pw-123").await.unwrap();

    let payload = r#"{"name":"web","listen":"127.0.0.1:2399"}"#;
    let r = http(
        gw.addr(),
        "POST",
        "/api/mappings",
        &[
            ("cookie", &cookie),
            ("content-type", "application/json"),
            ("origin", &format!("http://{}", gw.addr())),
        ],
        Some(payload),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(v["body"], payload, "请求体应逐字节一致");
    assert_eq!(v["method"], "POST");
    gw.token.cancel();
    _u.cancel();
}

#[tokio::test]
async fn cross_origin_write_is_rejected() {
    let (up, _u) = start_upstream().await;
    let gw = start_gateway(up, |_| {}).await;
    let cookie = login(&gw, "admin", "pw-123").await.unwrap();

    let r = http(
        gw.addr(),
        "POST",
        "/api/mappings",
        &[
            ("cookie", &cookie),
            ("origin", "https://evil.com"),
            ("content-type", "application/json"),
        ],
        Some("{}"),
    )
    .await;
    assert_eq!(r.status, 403, "跨站写请求应被拒: {}", r.body);

    // GET 不受影响（SameSite 已经挡住 cookie，这层只用于纵深防御）
    let r = http(gw.addr(), "GET", "/x", &[("cookie", &cookie)], None).await;
    assert_eq!(r.status, 200);
    gw.token.cancel();
    _u.cancel();
}

// ─────────────────────────────────────────────────────────────────────────────
// 授权
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn viewer_can_reach_its_route_but_not_others() {
    let (up, _u) = start_upstream().await;
    let (up2, _u2) = start_upstream().await;
    let gw = start_gateway(up, |c| {
        // 让 admin-only 指向另一个上游，这样两个端口的响应可区分
        c.route[1].upstream = up2.to_string();
    })
    .await;

    let cookie = login(&gw, "viewer", "pw-123").await.unwrap();

    // ① viewer 在 panel 端口登录成功，且能访问
    let r = http(gw.addr(), "GET", "/marker", &[("cookie", &cookie)], None).await;
    assert_eq!(r.status, 200, "{}", r.body);

    // ② 拿着同一个 cookie 去 admin-only 端口 —— 必须 403（不是 401：
    //    已登录但没权限，和"没登录"是两件事）
    let admin_addr = gw.addr_of("admin-only");
    let r = http(admin_addr, "GET", "/marker", &[("cookie", &cookie)], None).await;
    assert_eq!(r.status, 403, "已登录但无权限应 403: {}", r.body);

    // ③ 同一端口上，浏览器导航会看到说明页而不是 JSON
    let r = http(
        admin_addr,
        "GET",
        "/marker",
        &[("cookie", &cookie), ("sec-fetch-mode", "navigate")],
        None,
    )
    .await;
    assert_eq!(r.status, 403);
    assert!(r.body.contains("无权访问"), "浏览器应看到说明页: {}", r.body);
    assert!(r.body.contains("viewer"), "应指明是哪个账号");

    // ④ admin 在 admin-only 端口能正常访问
    let admin_cookie = login_at(&gw, admin_addr, "admin", "pw-123")
        .await
        .expect("admin 应能在 admin-only 端口登录");
    let r = http(admin_addr, "GET", "/marker", &[("cookie", &admin_cookie)], None).await;
    assert_eq!(r.status, 200, "{}", r.body);

    gw.token.cancel();
    _u.cancel();
    _u2.cancel();
}

#[tokio::test]
async fn login_is_refused_for_a_route_the_user_cannot_access() {
    let (up, _u) = start_upstream().await;
    let gw = start_gateway(up, |_| {}).await;

    // 在 panel 端口用 viewer 登录 → 允许
    assert!(login(&gw, "viewer", "pw-123").await.is_some());

    // 用一个只允许 admin 的 route：把 panel 的 ACL 改成只有 admin
    gw.token.cancel();
    _u.cancel();

    let (up, _u) = start_upstream().await;
    let gw = start_gateway(up, |c| {
        // viewer 只能去 "admin-only"（不存在于它的列表）
        c.user[1].routes = vec!["admin-only".into()];
    })
    .await;
    let r = http(
        gw.addr(),
        "POST",
        "/__auth/login",
        &[("content-type", "application/x-www-form-urlencoded")],
        Some("user=viewer&pass=pw-123&next=/"),
    )
    .await;
    assert_eq!(r.status, 403, "登录被拒应是 403 而不是 401: {}", r.body);
    assert!(r.body.contains("没有此端口的访问权限"), "{}", r.body);
    gw.token.cancel();
    _u.cancel();
}

// ─────────────────────────────────────────────────────────────────────────────
// 失败与限速
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn wrong_password_is_401_and_does_not_leak_whether_the_user_exists() {
    let (up, _u) = start_upstream().await;
    let gw = start_gateway(up, |c| {
        c.rate_limit.max_failures = 100; // 这条测试里不触发限速
    })
    .await;

    let a = http(
        gw.addr(),
        "POST",
        "/__auth/login",
        &[("content-type", "application/x-www-form-urlencoded")],
        Some("user=admin&pass=wrong&next=/"),
    )
    .await;
    let b = http(
        gw.addr(),
        "POST",
        "/__auth/login",
        &[("content-type", "application/x-www-form-urlencoded")],
        Some("user=nosuchuser&pass=wrong&next=/"),
    )
    .await;

    assert_eq!(a.status, 401);
    assert_eq!(b.status, 401, "用户不存在也应是 401");
    // 两种情况的响应体必须一样，否则响应内容泄露了用户是否存在
    assert_eq!(a.body, b.body, "错误信息不能区分「用户不存在」与「口令错误」");
    assert!(a.set_cookie().is_none(), "失败时不能下发 cookie");
    gw.token.cancel();
    _u.cancel();
}

#[tokio::test]
async fn repeated_failures_are_rate_limited_and_correct_password_is_also_blocked() {
    let (up, _u) = start_upstream().await;
    let gw = start_gateway(up, |c| {
        c.rate_limit.max_failures = 3;
    })
    .await;

    for i in 0..3 {
        let r = http(
            gw.addr(),
            "POST",
            "/__auth/login",
            &[("content-type", "application/x-www-form-urlencoded")],
            Some("user=admin&pass=no&next=/"),
        )
        .await;
        assert_eq!(r.status, 401, "第 {} 次应是 401", i + 1);
    }

    // 预算用尽：**正确口令也要被挡**，否则限速形同虚设
    let r = http(
        gw.addr(),
        "POST",
        "/__auth/login",
        &[("content-type", "application/x-www-form-urlencoded")],
        Some("user=admin&pass=pw-123&next=/"),
    )
    .await;
    assert_eq!(r.status, 429, "超预算应 429: {}", r.body);
    assert!(r.header("retry-after").is_some(), "429 应带 Retry-After");
    gw.token.cancel();
    _u.cancel();
}

#[tokio::test]
async fn logout_invalidates_the_session() {
    let (up, _u) = start_upstream().await;
    let gw = start_gateway(up, |_| {}).await;
    let cookie = login(&gw, "admin", "pw-123").await.unwrap();

    assert_eq!(
        http(gw.addr(), "GET", "/", &[("cookie", &cookie)], None).await.status,
        200
    );

    let r = http(gw.addr(), "GET", "/__auth/logout", &[("cookie", &cookie)], None).await;
    assert_eq!(r.status, 302);
    assert!(r.set_cookie().unwrap().contains("Max-Age=0"));

    // 旧 cookie 立刻失效
    let r = http(gw.addr(), "GET", "/", &[("cookie", &cookie)], None).await;
    assert_eq!(r.status, 401, "登出后旧 cookie 必须无效");
    gw.token.cancel();
    _u.cancel();
}

#[tokio::test]
async fn forged_cookie_is_rejected() {
    let (up, _u) = start_upstream().await;
    let gw = start_gateway(up, |_| {}).await;
    for bad in ["esc_gw=deadbeef", "esc_gw=", "esc_gw=AAAA", "other=x"] {
        let r = http(gw.addr(), "GET", "/", &[("cookie", bad)], None).await;
        assert!(r.status == 401 || r.status == 302, "{bad} → {}", r.status);
    }
    gw.token.cancel();
    _u.cancel();
}

// ─────────────────────────────────────────────────────────────────────────────
// 上游故障
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn unreachable_upstream_yields_502_not_500() {
    // 绑一个端口再立刻释放，得到一个"几乎肯定连不上"的地址
    let dead = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        drop(l);
        a
    };
    let gw = start_gateway(dead, |_| {}).await;
    let cookie = login(&gw, "admin", "pw-123").await.unwrap();

    let r = http(gw.addr(), "GET", "/", &[("cookie", &cookie)], None).await;
    assert_eq!(r.status, 502, "上游连不上应 502（而不是 500）: {}", r.body);
    assert!(r.body.contains("无法连接上游"), "{}", r.body);
    gw.token.cancel();
}

#[tokio::test]
async fn oversized_body_is_rejected_with_413() {
    let (up, _u) = start_upstream().await;
    let gw = start_gateway(up, |c| {
        c.route[0].max_body_bytes = Some(256);
    })
    .await;
    let cookie = login(&gw, "admin", "pw-123").await.unwrap();

    let big = "x".repeat(4096);
    let r = http(
        gw.addr(),
        "POST",
        "/upload",
        &[
            ("cookie", &cookie),
            ("content-type", "text/plain"),
            ("origin", &format!("http://{}", gw.addr())),
        ],
        Some(&big),
    )
    .await;
    assert!(
        r.status == 413 || r.status == 502,
        "超大请求体应被拒（413/502），实际 {}: {}",
        r.status,
        &r.body[..r.body.len().min(120)]
    );
    gw.token.cancel();
    _u.cancel();
}

#[tokio::test]
async fn strip_prefix_rewrites_the_path_for_upstream() {
    let (up, _u) = start_upstream().await;
    let gw = start_gateway(up, |c| {
        c.route[0].strip_prefix = Some("/panel".into());
    })
    .await;
    let cookie = login(&gw, "admin", "pw-123").await.unwrap();

    let r = http(
        gw.addr(),
        "GET",
        "/panel/api/status?x=1",
        &[("cookie", &cookie)],
        None,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(v["uri"], "/api/status?x=1", "前缀应被剥掉: {}", v["uri"]);
    gw.token.cancel();
    _u.cancel();
}

#[tokio::test]
async fn source_allowlist_blocks_before_authentication() {
    let (up, _u) = start_upstream().await;
    let gw = start_gateway(up, |c| {
        c.route[0].allowed_cidrs = vec!["10.0.0.0/8".into()];
    })
    .await;

    // 回环不在白名单里 → 403，而且连登录页都不给
    let r = http(gw.addr(), "GET", "/__auth/login", &[], None).await;
    assert_eq!(r.status, 403, "白名单外应 403: {}", r.body);
    assert!(r.body.contains("白名单"), "{}", r.body);

    // 即使是登录请求也挡住
    let r = http(
        gw.addr(),
        "POST",
        "/__auth/login",
        &[("content-type", "application/x-www-form-urlencoded")],
        Some("user=admin&pass=pw-123&next=/"),
    )
    .await;
    assert_eq!(r.status, 403);
    gw.token.cancel();
    _u.cancel();
}
