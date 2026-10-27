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
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\n",
        body.len()
    );
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
    /// 共享状态本身。测试里查会话表、账号库用。
    gw: Arc<Gateway>,
    /// 配了 admin_listen 时的管理端口地址。
    admin: Option<SocketAddr>,
    token: CancellationToken,
}

impl Gw {
    fn addr(&self) -> SocketAddr {
        self.addrs["panel"]
    }
    fn addr_of(&self, route: &str) -> SocketAddr {
        self.addrs[route]
    }
    fn admin_addr(&self) -> SocketAddr {
        self.admin.expect("这个测试需要 admin_listen")
    }
}

/// 起网关。给定上游地址与两点自定义：额外 route、限速放宽。
async fn start_gateway(upstream: SocketAddr, tweak: impl FnOnce(&mut GatewayConfig)) -> Gw {
    let hash = esc_gateway::config::hash_password("pw-123").unwrap();
    // 配置里必须写**真实端口**：热应用会拿配置里的 listen 去 bind，
    // 占位值（:0 / :1）会让"改路由"变成"给 1 号端口提权失败"。
    let panel_port = pick_port();
    let other_port = pick_port();
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
listen = "127.0.0.1:{panel_port}"
upstream = "{upstream}"

[[route]]
name = "admin-only"
listen = "127.0.0.1:{other_port}"
upstream = "{upstream}"
"#
    ))
    .unwrap();
    tweak(&mut cfg);

    let rt = cfg.build().expect("测试配置应合法");
    let gw = Arc::new(Gateway::new(Arc::new(rt)));
    let token = CancellationToken::new();

    // 手工绑定配置里的端口，并**登记进 ListenerSet**：这样管理面板做热应用时
    // 会认为这些端口"已经在听"，不会重复 bind，删除时也能正确停掉。
    let mut addrs = std::collections::HashMap::new();
    for r in gw.cfg.routes.values() {
        let l = tokio::net::TcpListener::bind(r.listen)
            .await
            .unwrap_or_else(|e| panic!("绑定 {} 失败: {e}", r.listen));
        let addr = l.local_addr().unwrap();
        addrs.insert(r.name.clone(), addr);
        let mut r2 = r.clone();
        r2.listen = addr;
        gw.routes.replace_route(r2.clone());

        let tok = gw.listeners.adopt(addr);
        let gw = gw.clone();
        let shut = token.clone();
        tokio::spawn(async move {
            // 让 route 的取消令牌与测试的 token 联动
            let t = tok.clone();
            tokio::spawn(async move {
                shut.cancelled().await;
                t.cancel();
            });
            esc_gateway::proxy::serve_route(gw, Arc::new(r2), l, tok).await;
        });
    }
    // 配了独立管理端口就一起起起来（和 run() 里一样的做法：不登记进
    // ListenerSet，免得被热应用当成多余的路由端口停掉）。
    let admin_addr = match gw.cfg.admin_listen {
        Some(addr) => {
            let l = tokio::net::TcpListener::bind(addr)
                .await
                .unwrap_or_else(|e| panic!("绑定管理端口 {addr} 失败: {e}"));
            let addr = l.local_addr().unwrap();
            let gw2 = gw.clone();
            let shut = token.clone();
            tokio::spawn(async move { esc_gateway::proxy::serve_admin(gw2, addr, l, shut).await });
            Some(addr)
        }
        None => None,
    };

    tokio::time::sleep(Duration::from_millis(40)).await;
    Gw {
        addrs,
        gw,
        admin: admin_addr,
        token,
    }
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
async fn login_at(_gw: &Gw, addr: SocketAddr, user: &str, pass: &str) -> Option<String> {
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
    let r = http(
        gw.addr(),
        "GET",
        "/__auth/login?next=%2Fapi%2Fstatus",
        &[],
        None,
    )
    .await;
    assert!(r.body.contains(r#"value="/api/status""#), "{}", r.body);

    // 开放重定向在登录页就该被中和
    let r = http(
        gw.addr(),
        "GET",
        "/__auth/login?next=%2F%2Fevil.com",
        &[],
        None,
    )
    .await;
    assert!(
        !r.body.contains("evil.com"),
        "协议相对 URL 应被中和: {}",
        r.body
    );

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
    let r = http(
        gw.addr(),
        "GET",
        "/api/status?x=1",
        &[("cookie", &cookie)],
        None,
    )
    .await;
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
    assert!(
        host.contains(&up.to_string()),
        "Host 应被重写为上游: {host}"
    );

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
    assert!(
        r.body.contains("无权访问"),
        "浏览器应看到说明页: {}",
        r.body
    );
    assert!(r.body.contains("viewer"), "应指明是哪个账号");

    // ④ admin 在 admin-only 端口能正常访问
    let admin_cookie = login_at(&gw, admin_addr, "admin", "pw-123")
        .await
        .expect("admin 应能在 admin-only 端口登录");
    let r = http(
        admin_addr,
        "GET",
        "/marker",
        &[("cookie", &admin_cookie)],
        None,
    )
    .await;
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
    assert_eq!(
        a.body, b.body,
        "错误信息不能区分「用户不存在」与「口令错误」"
    );
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
        http(gw.addr(), "GET", "/", &[("cookie", &cookie)], None)
            .await
            .status,
        200
    );

    let r = http(
        gw.addr(),
        "GET",
        "/__auth/logout",
        &[("cookie", &cookie)],
        None,
    )
    .await;
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

// ─────────────────────────────────────────────────────────────────────────────
// 管理面板
// ─────────────────────────────────────────────────────────────────────────────

fn tmpdir(tag: &str) -> std::path::PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!("esc-gw-e2e-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// 带可写账号库的网关，`admin` 账号提成管理员。
async fn start_admin_gateway(upstream: SocketAddr) -> (Gw, std::path::PathBuf) {
    let dir = tmpdir("panel");
    let path = dir.join("gateway-users.toml");
    let routes = dir.join("gateway-routes.toml");
    let gw = start_gateway(upstream, |c| {
        c.users_file = Some(path.clone());
        c.routes_file = Some(routes);
        for u in &mut c.user {
            if u.name == "admin" {
                u.admin = true;
            }
        }
    })
    .await;
    (gw, path)
}

/// 同步版：挑一个空闲端口（配置文本里要用，所以不能 await）。
fn pick_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// 找一个当前空闲的 TCP 端口。
///
/// bind 完立刻释放：这里有个天然的竞态（释放到再次 bind 之间别人可能抢走），
/// 但测试里没有别的竞争者，实际稳定。
async fn free_port() -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    l.local_addr().unwrap().port()
}

fn form_escape(s: &str) -> String {
    s.replace('%', "%25")
        .replace('&', "%26")
        .replace('=', "%3D")
        .replace('+', "%2B")
        .replace(' ', "%20")
        .replace('\n', "%0A")
        .replace(':', "%3A")
}

/// 带 cookie 的 POST（表单）。
async fn post_form(addr: SocketAddr, path: &str, cookie: &str, body: &str) -> Resp {
    http(
        addr,
        "POST",
        path,
        &[
            ("cookie", cookie),
            ("content-type", "application/x-www-form-urlencoded"),
        ],
        Some(body),
    )
    .await
}

#[tokio::test]
async fn admin_panel_needs_login_like_everything_else() {
    let (up, _u) = start_upstream().await;
    let (gw, _dir) = start_admin_gateway(up).await;

    // 浏览器：跳登录页，且把原路径带回去
    let r = http(
        gw.addr(),
        "GET",
        "/__admin/users",
        &[("sec-fetch-mode", "navigate")],
        None,
    )
    .await;
    assert_eq!(r.status, 302);
    let loc = r.header("location").unwrap();
    assert!(loc.starts_with("/__auth/login?next="), "{loc}");
    assert!(loc.contains("__admin"), "登录后要回到面板: {loc}");

    // 脚本：401 JSON（和普通页面的规则一致）
    let r = http(gw.addr(), "GET", "/__admin/", &[("accept", "*/*")], None).await;
    assert_eq!(r.status, 401);
    assert_eq!(r.header("content-type"), Some("application/json"));

    gw.token.cancel();
    _u.cancel();
}

#[tokio::test]
async fn panel_pages_are_not_cacheable_and_not_framable() {
    let (up, _u) = start_upstream().await;
    let (gw, _dir) = start_admin_gateway(up).await;
    let c = login(&gw, "admin", "pw-123").await.unwrap();

    let r = http(gw.addr(), "GET", "/__admin/", &[("cookie", &c)], None).await;
    assert_eq!(r.status, 200);
    // 账号列表被中间层缓存下来会是什么后果，不用细想
    assert_eq!(r.header("cache-control"), Some("no-store"));
    assert_eq!(r.header("x-frame-options"), Some("DENY"));
    assert_eq!(r.header("x-content-type-options"), Some("nosniff"));

    gw.token.cancel();
    _u.cancel();
}

#[tokio::test]
async fn plain_user_can_only_reach_my_account() {
    let (up, _u) = start_upstream().await;
    let (gw, _dir) = start_admin_gateway(up).await;
    let c = login(&gw, "viewer", "pw-123").await.unwrap();

    // `/__admin/` 带他去看自己的账号，而不是一个冷冰冰的 403
    let r = http(gw.addr(), "GET", "/__admin/", &[("cookie", &c)], None).await;
    assert_eq!(r.status, 302);
    assert_eq!(r.header("location"), Some("/__admin/me"));

    let r = http(gw.addr(), "GET", "/__admin/me", &[("cookie", &c)], None).await;
    assert_eq!(r.status, 200);
    assert!(r.body.contains("我的账号"));

    // 但改账号一律 403——包括猜出来的端点
    for path in [
        "/__admin/users",
        "/__admin/users/admin",
        "/__admin/sessions",
    ] {
        let r = http(gw.addr(), "GET", path, &[("cookie", &c)], None).await;
        assert_eq!(r.status, 403, "{path} 应对普通用户关闭");
    }
    let r = post_form(gw.addr(), "/__admin/users/admin/delete", &c, "").await;
    assert_eq!(r.status, 403, "普通用户不能删账号");
    assert!(gw.gw.user("admin").is_some(), "账号不该被删掉");

    gw.token.cancel();
    _u.cancel();
}

#[tokio::test]
async fn admin_creates_an_account_that_can_then_log_in() {
    let (up, _u) = start_upstream().await;
    let (gw, path) = start_admin_gateway(up).await;
    let c = login(&gw, "admin", "pw-123").await.unwrap();

    // 列表里能看到两个配置文件账号
    let r = http(gw.addr(), "GET", "/__admin/", &[("cookie", &c)], None).await;
    assert_eq!(r.status, 200);
    assert!(r.body.contains("viewer"));
    assert!(r.body.contains("配置文件"), "要标出账号来源");

    let r = post_form(
        gw.addr(),
        "/__admin/users",
        &c,
        "name=bob&pass=bob-pass-123&pass2=bob-pass-123&route=panel",
    )
    .await;
    assert_eq!(r.status, 302, "成功后重定向: {}", r.body);
    assert_eq!(
        r.header("location"),
        Some("/__admin/"),
        "提示走会话，不进 URL"
    );
    // 提示条只在紧接着的那次 GET 出现一次
    let page = http(gw.addr(), "GET", "/__admin/", &[("cookie", &c)], None).await;
    assert!(page.body.contains("已创建"), "{}", page.body);
    let again = http(gw.addr(), "GET", "/__admin/", &[("cookie", &c)], None).await;
    assert!(!again.body.contains("已创建"), "刷新不该重复出现提示");

    // 落盘了，而且没有明文口令
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(raw.contains("name = \"bob\""), "{raw}");
    assert!(!raw.contains("bob-pass-123"), "账号库不能出现明文口令");

    // 新账号能登录并访问被授权的端口
    let bob = login(&gw, "bob", "bob-pass-123").await;
    assert!(bob.is_some(), "新建的账号应该能登录");
    let r = http(
        gw.addr(),
        "GET",
        "/",
        &[("cookie", &bob.unwrap()), ("accept", "*/*")],
        None,
    )
    .await;
    assert_eq!(r.status, 200, "应被转发到上游: {}", r.body);

    gw.token.cancel();
    _u.cancel();
}

#[tokio::test]
async fn granted_routes_are_enforced_at_login_and_afterwards() {
    let (up, _u) = start_upstream().await;
    let (gw, _dir) = start_admin_gateway(up).await;
    let c = login(&gw, "admin", "pw-123").await.unwrap();

    let r = post_form(
        gw.addr(),
        "/__admin/users",
        &c,
        "name=bob&pass=bob-pass-123&pass2=bob-pass-123&route=panel",
    )
    .await;
    assert_eq!(r.status, 302);

    // 只能在授权过的端口登录
    assert!(login(&gw, "bob", "bob-pass-123").await.is_some());
    let on_other = login_at(&gw, gw.addr_of("admin-only"), "bob", "bob-pass-123").await;
    assert!(on_other.is_none(), "没授权的端口不该能登录");

    gw.token.cancel();
    _u.cancel();
}

#[tokio::test]
async fn the_last_admin_can_be_neither_deleted_nor_demoted() {
    let (up, _u) = start_upstream().await;
    let (gw, _dir) = start_admin_gateway(up).await;
    let c = login(&gw, "admin", "pw-123").await.unwrap();

    // 配置文件里的账号面板动不了，先接管进账号库
    let r = post_form(gw.addr(), "/__admin/users/admin/adopt", &c, "").await;
    assert_eq!(r.status, 302);
    assert!(gw.gw.store.get("admin").is_some());

    // 删：拒绝
    let r = post_form(gw.addr(), "/__admin/users/admin/delete", &c, "").await;
    assert_eq!(r.status, 302);
    let page = http(gw.addr(), "GET", "/__admin/", &[("cookie", &c)], None).await;
    assert!(page.body.contains("最后一个管理员"), "{}", page.body);
    assert!(gw.gw.user("admin").is_some(), "最后一个管理员不能被删");

    // 降级：拒绝。这里是表单提交，所以错误要回到表单上（200 + 内联提示），
    // 而不是重定向——否则用户填的东西全丢了。
    let r = post_form(gw.addr(), "/__admin/users/admin", &c, "route=panel").await;
    assert_eq!(r.status, 200, "表单错误应原地显示: {}", r.body);
    assert!(r.body.contains("最后一个管理员"), "{}", r.body);
    assert!(r.body.contains("type=\"checkbox\""), "表单要重新渲染出来");
    assert!(gw.gw.user("admin").unwrap().admin, "管理权限不该被取消");

    // 有第二个管理员之后，一切恢复正常
    let r = post_form(
        gw.addr(),
        "/__admin/users",
        &c,
        "name=boss&pass=boss-pass-123&pass2=boss-pass-123&route=panel&admin=1",
    )
    .await;
    assert_eq!(r.status, 302);
    // 但不能用它自己的会话删自己：那会把当前会话一起打掉，
    // 用户看到的是一个莫名其妙的登录页
    let r = post_form(gw.addr(), "/__admin/users/admin/delete", &c, "").await;
    assert_eq!(r.status, 302);
    let page = http(gw.addr(), "GET", "/__admin/", &[("cookie", &c)], None).await;
    assert!(page.body.contains("正在使用的账号"), "{}", page.body);

    // 换成 boss 的会话删，就成了
    let boss = login(&gw, "boss", "boss-pass-123")
        .await
        .expect("boss 应能登录");
    let r = post_form(gw.addr(), "/__admin/users/admin/delete", &boss, "").await;
    assert_eq!(r.status, 302);
    assert!(
        gw.gw.store.get("admin").is_none(),
        "有了第二个管理员就该允许删除"
    );

    // 有意思的地方：删掉的是**账号库里那条**，gateway.toml 里的 admin 于是
    // 又生效了。这不是 bug，是刻意留的救援通道——管理面板永远无法把配置
    // 文件里的账号删掉，所以「把自己锁在门外」这件事有硬下限。
    assert!(gw.gw.user("admin").is_some(), "配置文件里的账号应重新生效");
    let page = http(gw.addr(), "GET", "/__admin/", &[("cookie", &boss)], None).await;
    assert!(page.body.contains("配置文件（只读）"), "{}", page.body);

    gw.token.cancel();
    _u.cancel();
}

#[tokio::test]
async fn changing_a_password_kills_other_sessions_and_adopts_the_config_account() {
    let (up, _u) = start_upstream().await;
    let (gw, path) = start_admin_gateway(up).await;

    // 同一个账号开两个会话：一个自己用，一个「被人拿了 cookie」
    let mine = login(&gw, "admin", "pw-123").await.unwrap();
    let stolen = login(&gw, "admin", "pw-123").await.unwrap();
    assert_eq!(gw.gw.auth.session_count(), 2);

    let r = post_form(
        gw.addr(),
        "/__admin/me",
        &mine,
        "old=pw-123&pass=new-pass-456&pass2=new-pass-456",
    )
    .await;
    assert_eq!(r.status, 302);
    assert_eq!(r.header("location"), Some("/__admin/me"));

    // 旧 cookie 立刻失效，自己的这个保留
    let r = http(gw.addr(), "GET", "/__admin/", &[("cookie", &stolen)], None).await;
    assert_eq!(r.status, 401, "被盗的会话必须马上踢掉");
    let r = http(gw.addr(), "GET", "/__admin/", &[("cookie", &mine)], None).await;
    assert_eq!(r.status, 200, "自己的会话不该被自己踢掉");

    // 口令真的换了
    assert!(
        login(&gw, "admin", "pw-123").await.is_none(),
        "旧口令必须失效"
    );
    assert!(login(&gw, "admin", "new-pass-456").await.is_some());

    // 配置文件里的账号被接管进了账号库，否则新口令无处可落
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(
        raw.contains("name = \"admin\""),
        "自助改密应自动接管: {raw}"
    );

    gw.token.cancel();
    _u.cancel();
}

#[tokio::test]
async fn self_service_password_change_requires_the_current_password() {
    let (up, _u) = start_upstream().await;
    let (gw, _dir) = start_admin_gateway(up).await;
    let c = login(&gw, "admin", "pw-123").await.unwrap();

    // 光有 cookie 不够：不验证当前口令的话，一次会话劫持就变成永久占有
    let r = post_form(
        gw.addr(),
        "/__admin/me",
        &c,
        "old=wrong-pw&pass=new-pass-456&pass2=new-pass-456",
    )
    .await;
    assert_eq!(r.status, 200, "口令不对应该回到表单");
    assert!(r.body.contains("当前口令不正确"), "{}", r.body);
    assert!(
        login(&gw, "admin", "pw-123").await.is_some(),
        "口令不该被改掉"
    );

    // 两次新口令不一致
    let r = post_form(
        gw.addr(),
        "/__admin/me",
        &c,
        "old=pw-123&pass=new-pass-456&pass2=new-pass-457",
    )
    .await;
    assert!(r.body.contains("不一致"), "{}", r.body);

    gw.token.cancel();
    _u.cancel();
}

#[tokio::test]
async fn cross_origin_admin_post_is_rejected() {
    let (up, _u) = start_upstream().await;
    let (gw, _dir) = start_admin_gateway(up).await;
    let c = login(&gw, "admin", "pw-123").await.unwrap();

    // SameSite=Strict 已经让浏览器不发 cookie，但纵深防御这一层要能独立成立
    let r = http(
        gw.addr(),
        "POST",
        "/__admin/users",
        &[
            ("cookie", &c),
            ("content-type", "application/x-www-form-urlencoded"),
            ("origin", "http://evil.example"),
        ],
        Some("name=mallory&pass=mallory-pass&pass2=mallory-pass&route=panel"),
    )
    .await;
    assert_eq!(r.status, 403, "跨站 POST 必须拒绝");
    assert!(gw.gw.user("mallory").is_none(), "不该留下账号");

    gw.token.cancel();
    _u.cancel();
}

#[tokio::test]
async fn sessions_can_be_kicked_out_by_prefix_and_by_account() {
    let (up, _u) = start_upstream().await;
    let (gw, _dir) = start_admin_gateway(up).await;
    let admin = login(&gw, "admin", "pw-123").await.unwrap();
    let viewer = login(&gw, "viewer", "pw-123").await.unwrap();

    // 会话页能看到双方
    let r = http(
        gw.addr(),
        "GET",
        "/__admin/sessions",
        &[("cookie", &admin)],
        None,
    )
    .await;
    assert_eq!(r.status, 200);
    assert!(r.body.contains("viewer"));
    // 完整 session id 不进 HTML：一次 XSS 就等于把所有在线会话交出去
    assert!(
        !r.body.contains(&sid_of(&viewer)),
        "页面不该包含完整会话 id"
    );

    // 按账号全踢
    let r = post_form(gw.addr(), "/__admin/users/viewer/revoke", &admin, "").await;
    assert_eq!(r.status, 302);
    let r = http(
        gw.addr(),
        "GET",
        "/api/x",
        &[("cookie", &viewer), ("accept", "*/*")],
        None,
    )
    .await;
    assert_eq!(r.status, 401, "被踢的会话应立即失效");

    // 按前缀踢单个（用自己的第二个会话试）
    let other = login(&gw, "admin", "pw-123").await.unwrap();
    let other_sid = sid_of(&other);
    let prefix: String = gw
        .gw
        .auth
        .list()
        .into_iter()
        .filter(|(sid, _)| sid.starts_with(&other_sid))
        .map(|(sid, _)| sid.chars().take(12).collect())
        .next()
        .expect("应找到刚建的那个会话");
    let r = post_form(
        gw.addr(),
        &format!("/__admin/sessions/{prefix}/revoke"),
        &admin,
        "",
    )
    .await;
    assert_eq!(r.status, 302);
    assert_eq!(r.header("location"), Some("/__admin/sessions"));
    let r = http(gw.addr(), "GET", "/__admin/", &[("cookie", &other)], None).await;
    assert_eq!(r.status, 401);

    gw.token.cancel();
    _u.cancel();
}

/// 从 cookie 里取 session id（`esc_gw=<sid>`）。
fn sid_of(cookie: &str) -> String {
    cookie
        .split_once('=')
        .map(|(_, v)| v.to_string())
        .unwrap_or_default()
}

#[tokio::test]
async fn a_gateway_without_users_file_is_a_read_only_panel() {
    let (up, _u) = start_upstream().await;
    // 不设 users_file（等价于直接 build 的配置）
    let gw = start_gateway(up, |c| {
        for u in &mut c.user {
            if u.name == "admin" {
                u.admin = true;
            }
        }
    })
    .await;
    let c = login(&gw, "admin", "pw-123").await.unwrap();

    let r = http(gw.addr(), "GET", "/__admin/", &[("cookie", &c)], None).await;
    assert_eq!(r.status, 200);
    assert!(r.body.contains("只读"), "页面要说明为什么改不了");

    // 提交了也不会生效，而且要说清楚原因
    let r = post_form(
        gw.addr(),
        "/__admin/users",
        &c,
        "name=bob&pass=bob-pass-123&pass2=bob-pass-123&route=panel",
    )
    .await;
    assert_eq!(r.status, 200, "错误应回到表单而不是重定向");
    assert!(r.body.contains("只读"), "{}", r.body);
    assert!(gw.gw.user("bob").is_none());

    gw.token.cancel();
    _u.cancel();
}

#[tokio::test]
async fn admin_allowed_cidrs_is_fail_closed() {
    let (up, _u) = start_upstream().await;
    let gw = start_gateway(up, |c| {
        c.admin_allowed_cidrs = vec!["10.0.0.0/8".to_string()];
        for u in &mut c.user {
            if u.name == "admin" {
                u.admin = true;
            }
        }
    })
    .await;
    let c = login(&gw, "admin", "pw-123").await.unwrap();

    // 测试从 127.0.0.1 发起，不在 10/8 里 → 连登录页都不该给
    let r = http(gw.addr(), "GET", "/__admin/", &[("cookie", &c)], None).await;
    assert_eq!(r.status, 403);
    assert!(r.body.contains("admin_allowed_cidrs"), "{}", r.body);

    // 普通页面不受影响
    let r = http(
        gw.addr(),
        "GET",
        "/api/x",
        &[("cookie", &c), ("accept", "*/*")],
        None,
    )
    .await;
    assert_eq!(r.status, 200);

    gw.token.cancel();
    _u.cancel();
}

// ─────────────────────────────────────────────────────────────────────────────
// 网关首页
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn gateway_home_lists_only_the_routes_you_may_use() {
    let (up, _u) = start_upstream().await;
    let (gw, _dir) = start_admin_gateway(up).await;

    // 未登录：跳登录页
    let r = http(
        gw.addr(),
        "GET",
        "/__auth/",
        &[("sec-fetch-mode", "navigate")],
        None,
    )
    .await;
    assert_eq!(r.status, 302);
    assert_eq!(r.header("location"), Some("/__auth/login?next=/__auth/"));

    // admin 看得到两个端口，并且有管理入口
    let c = login(&gw, "admin", "pw-123").await.unwrap();
    let r = http(gw.addr(), "GET", "/__auth/", &[("cookie", &c)], None).await;
    assert_eq!(r.status, 200);
    assert!(r.body.contains("panel") && r.body.contains("admin-only"));
    assert!(r.body.contains("/__admin/"), "管理员应有管理入口");
    assert!(r.body.contains("你好，admin"));

    // viewer 只能看到 panel：没权限的端口**连名字都不该出现**
    let v = login(&gw, "viewer", "pw-123").await.unwrap();
    let r = http(gw.addr(), "GET", "/__auth", &[("cookie", &v)], None).await;
    assert_eq!(r.status, 200, "/__auth 不带斜杠也认");
    assert!(r.body.contains("panel"));
    assert!(
        !r.body.contains("admin-only"),
        "不该泄露自己无权访问的端口名：{}",
        r.body
    );
    // 用链接而不是文案判断：页面上有一句「管理账号…需要管理员权限」的说明，
    // 按文案断言会把提示语误判成入口。
    assert!(
        !r.body.contains("href=\"/__admin/\""),
        "普通用户不该看到账号列表入口：{}",
        r.body
    );
    assert!(
        !r.body.contains(">/__admin/sessions<") && !r.body.contains("href=\"/__admin/sessions\""),
        "也不该看到会话列表入口"
    );
    assert!(
        r.body.contains("href=\"/__admin/me\""),
        "但改自己口令要能给：{}",
        r.body
    );

    gw.token.cancel();
    _u.cancel();
}

#[tokio::test]
async fn gateway_home_links_carry_the_port_and_keep_the_scheme() {
    let (up, _u) = start_upstream().await;
    let (gw, _dir) = start_admin_gateway(up).await;
    let c = login(&gw, "admin", "pw-123").await.unwrap();

    let r = http(gw.addr(), "GET", "/__auth/", &[("cookie", &c)], None).await;
    // 端口取自**配置**（用户实际要连的那个），不是当前监听 socket。
    // 测试配置里写的是占位端口，所以这里只验证形状：协议相对 + 带主机名 + 带端口。
    assert!(
        r.body.contains("href=\"//127.0.0.1:"),
        "入口链接应是协议相对的 //host:port/：{}",
        r.body
    );
    assert!(
        !r.body.contains("href=\"http://"),
        "不该写死 scheme——前面挂 TLS 后就会跳回明文"
    );

    gw.token.cancel();
    _u.cancel();
}

// ─────────────────────────────────────────────────────────────────────────────
// 代理端口的管理面板（热应用）
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_port_added_from_the_panel_starts_listening_immediately() {
    let (up, _u) = start_upstream().await;
    let (gw, _dir) = start_admin_gateway(up).await;
    let c = login(&gw, "admin", "pw-123").await.unwrap();

    // 列表页能看到现有的两个端口
    let r = http(gw.addr(), "GET", "/__admin/routes", &[("cookie", &c)], None).await;
    assert_eq!(r.status, 200);
    assert!(r.body.contains("panel") && r.body.contains("admin-only"));
    assert!(r.body.contains("配置文件"), "要标出端口来源");

    let port = free_port().await;
    let body = format!(
        "name=fresh&listen={}&upstream={}",
        form_escape(&format!("127.0.0.1:{port}")),
        form_escape(&up.to_string())
    );
    let r = post_form(gw.addr(), "/__admin/routes", &c, &body).await;
    assert_eq!(r.status, 302, "{}", r.body);

    // 真的开始听了：新端口上未登录访问会被要求登录（= 网关在服务）
    let new_addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let r = http(new_addr, "GET", "/api/x", &[("accept", "*/*")], None).await;
    assert_eq!(r.status, 401, "新端口应已被网关接管: {}", r.body);

    // 带上凭据就能拿到上游内容（admin 的 routes 是 *）
    let r = http(
        new_addr,
        "GET",
        "/",
        &[("cookie", &c), ("accept", "*/*")],
        None,
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.body.contains("\"uri\":\"/\""), "{}", r.body);

    gw.token.cancel();
    _u.cancel();
}

#[tokio::test]
async fn a_port_added_from_the_panel_is_written_to_disk() {
    let (up, _u) = start_upstream().await;
    let (gw, _dir) = start_admin_gateway(up).await;
    let c = login(&gw, "admin", "pw-123").await.unwrap();
    let port = free_port().await;

    let body = format!(
        "name=fresh&listen={}&upstream={}&allowed_cidrs={}",
        form_escape(&format!("127.0.0.1:{port}")),
        form_escape(&up.to_string()),
        form_escape("10.0.0.0/8\n192.168.0.0/16"),
    );
    let r = post_form(gw.addr(), "/__admin/routes", &c, &body).await;
    assert_eq!(r.status, 302);

    let routes_file = gw.gw.cfg.routes_file.clone().unwrap();
    let raw = std::fs::read_to_string(&routes_file).unwrap();
    assert!(raw.contains("name = \"fresh\""), "{raw}");
    assert!(raw.contains(&port.to_string()), "{raw}");
    assert!(raw.contains("10.0.0.0/8"), "白名单要落盘: {raw}");

    // 落盘的东西能被重新读出来（下次启动不会丢）
    let reloaded = esc_gateway::RouteStore::load(Some(&routes_file));
    let spec = reloaded.get("fresh").expect("重载后应有 fresh");
    assert_eq!(spec.allowed_cidrs.len(), 2);

    gw.token.cancel();
    _u.cancel();
}

#[tokio::test]
async fn changing_the_upstream_takes_effect_on_the_next_request() {
    let (up, _u) = start_upstream().await;
    let (gw, _dir) = start_admin_gateway(up).await;
    let c = login(&gw, "admin", "pw-123").await.unwrap();
    let port = free_port().await;
    let new_addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

    let body = format!(
        "name=fresh&listen={}&upstream={}",
        form_escape(&format!("127.0.0.1:{port}")),
        form_escape(&up.to_string())
    );
    assert_eq!(
        post_form(gw.addr(), "/__admin/routes", &c, &body)
            .await
            .status,
        302
    );
    // 先确认通
    let r = http(
        new_addr,
        "GET",
        "/",
        &[("cookie", &c), ("accept", "*/*")],
        None,
    )
    .await;
    assert_eq!(r.status, 200);

    // 把上游改成一个没人听的端口：不改监听，只改转发目标
    let body = format!(
        "name=&listen={}&upstream={}",
        form_escape(&format!("127.0.0.1:{port}")),
        form_escape("127.0.0.1:1"),
    );
    let r = post_form(gw.addr(), "/__admin/routes/fresh", &c, &body).await;
    assert_eq!(r.status, 302, "{}", r.body);

    // 同一个地址、下一个请求，就该 502 了 —— 证明每个请求都重读了路由定义
    let r = http(
        new_addr,
        "GET",
        "/",
        &[("cookie", &c), ("accept", "*/*")],
        None,
    )
    .await;
    assert_eq!(r.status, 502, "改上游后应立刻生效: {}", r.body);

    gw.token.cancel();
    _u.cancel();
}

#[tokio::test]
async fn a_port_that_cannot_be_bound_changes_nothing() {
    let (up, _u) = start_upstream().await;
    let (gw, _dir) = start_admin_gateway(up).await;
    let c = login(&gw, "admin", "pw-123").await.unwrap();

    // 占住一个端口，然后让面板去监听它
    let squatter = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let busy = squatter.local_addr().unwrap();

    let body = format!(
        "name=fresh&listen={}&upstream={}",
        form_escape(&busy.to_string()),
        form_escape(&up.to_string())
    );
    let r = post_form(gw.addr(), "/__admin/routes", &c, &body).await;
    // 错误回填到表单（200），而不是重定向
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.body.contains("无法监听"), "{}", r.body);
    // 表单里的值要还在，不然重填一遍很烦
    assert!(r.body.contains(&busy.to_string()), "{}", r.body);

    // 什么都没写进库
    let routes_file = gw.gw.cfg.routes_file.clone().unwrap();
    let raw = std::fs::read_to_string(&routes_file).unwrap_or_default();
    assert!(
        !raw.contains("name = \"fresh\""),
        "失败的改动不该落盘: {raw}"
    );
    assert!(gw.gw.rt_store.get("fresh").is_none());
    // 原有端口照旧工作
    let r = http(
        gw.addr(),
        "GET",
        "/",
        &[("cookie", &c), ("accept", "*/*")],
        None,
    )
    .await;
    assert_eq!(r.status, 200);

    drop(squatter);
    gw.token.cancel();
    _u.cancel();
}

#[tokio::test]
async fn deleting_a_port_stops_the_listener_and_refuses_to_break_the_current_one() {
    let (up, _u) = start_upstream().await;
    let (gw, _dir) = start_admin_gateway(up).await;
    let c = login(&gw, "admin", "pw-123").await.unwrap();
    let port = free_port().await;
    let new_addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

    let body = format!(
        "name=fresh&listen={}&upstream={}",
        form_escape(&format!("127.0.0.1:{port}")),
        form_escape(&up.to_string())
    );
    assert_eq!(
        post_form(gw.addr(), "/__admin/routes", &c, &body)
            .await
            .status,
        302
    );

    // 先接管 panel，让它变成"可编辑"，才能走到"删自己"那道闸
    assert_eq!(
        post_form(gw.addr(), "/__admin/routes/panel/adopt", &c, "")
            .await
            .status,
        302
    );
    // 删掉「自己正在用的那个端口」要被拒绝
    let r = post_form(gw.addr(), "/__admin/routes/panel/delete", &c, "").await;
    assert_eq!(r.status, 302);
    let page = http(gw.addr(), "GET", "/__admin/routes", &[("cookie", &c)], None).await;
    assert!(
        page.body.contains("就是你现在访问管理面板用的这个"),
        "{}",
        page.body
    );
    // 它还在、还在听
    assert!(gw.gw.routes.get("panel").is_some());
    assert_eq!(
        http(
            gw.addr(),
            "GET",
            "/",
            &[("cookie", &c), ("accept", "*/*")],
            None
        )
        .await
        .status,
        200
    );

    // 删掉刚建的那个：监听要停
    let r = post_form(gw.addr(), "/__admin/routes/fresh/delete", &c, "").await;
    assert_eq!(r.status, 302);
    // 监听真的停了：连都连不上（不是 401、不是 502，是 connection refused）
    assert!(
        tokio::net::TcpStream::connect(new_addr).await.is_err(),
        "删掉的端口不该再接受连接"
    );
    // 而面板自己那个端口照旧
    assert_eq!(
        http(
            gw.addr(),
            "GET",
            "/",
            &[("cookie", &c), ("accept", "*/*")],
            None
        )
        .await
        .status,
        200
    );

    gw.token.cancel();
    _u.cancel();
}

// ─────────────────────────────────────────────────────────────────────────────
// 独立的管理端口
// ─────────────────────────────────────────────────────────────────────────────

/// 配了 admin_listen 的网关。
async fn start_gateway_with_admin(upstream: SocketAddr) -> (Gw, std::path::PathBuf) {
    let dir = tmpdir("adminport");
    let path = dir.join("gateway-users.toml");
    let admin_port = pick_port();
    let gw = start_gateway(upstream, |c| {
        c.users_file = Some(path.clone());
        c.routes_file = Some(dir.join("gateway-routes.toml"));
        c.admin_listen = Some(format!("127.0.0.1:{admin_port}"));
        for u in &mut c.user {
            if u.name == "admin" {
                u.admin = true;
            }
        }
    })
    .await;
    (gw, path)
}

#[tokio::test]
async fn the_admin_port_serves_the_panel_and_replaces_the_per_port_path() {
    let (up, _u) = start_upstream().await;
    let (gw, _dir) = start_gateway_with_admin(up).await;
    let admin = gw.admin_addr();

    // ① 管理端口上：登录页 + 管理面板都正常
    let r = http(
        admin,
        "GET",
        "/__auth/login",
        &[("accept", "text/html")],
        None,
    )
    .await;
    assert_eq!(r.status, 200);
    assert!(r.body.contains("登录"), "{}", r.body);

    let c = login_at(&gw, admin, "admin", "pw-123")
        .await
        .expect("管理端口应能登录");
    let r = http(admin, "GET", "/__admin/", &[("cookie", &c)], None).await;
    assert_eq!(r.status, 200);
    assert!(r.body.contains("账号"));

    // ② 管理端口不转发任何东西：`/` 是网关首页，不是上游内容
    let r = http(admin, "GET", "/", &[("cookie", &c)], None).await;
    assert_eq!(r.status, 200);
    assert!(
        !r.body.contains("\"uri\":\"/\""),
        "管理端口不该转发到上游：{}",
        r.body
    );
    assert!(r.body.contains("你可以访问"), "应是网关首页：{}", r.body);

    // ③ 转发端口上的 /__admin/ 变成「在别处」的说明，而不是管理面板
    let r = http(gw.addr(), "GET", "/__admin/", &[("cookie", &c)], None).await;
    assert_eq!(r.status, 200);
    assert!(r.body.contains("管理面板不在这个端口"), "{}", r.body);
    assert!(
        r.body.contains(&format!(":{}", admin.port())),
        "要给出管理端口的链接：{}",
        r.body
    );
    assert!(!r.body.contains("最近操作"), "不该泄露管理面板内容");

    // ④ 但转发端口照旧工作
    let r = http(
        gw.addr(),
        "GET",
        "/",
        &[("cookie", &c), ("accept", "*/*")],
        None,
    )
    .await;
    assert_eq!(r.status, 200);
    assert!(r.body.contains("\"uri\":\"/\""));

    gw.token.cancel();
    _u.cancel();
}

#[tokio::test]
async fn one_login_on_the_admin_port_opens_the_proxied_panels_too() {
    let (up, _u) = start_upstream().await;
    let (gw, _dir) = start_gateway_with_admin(up).await;
    let admin = gw.admin_addr();

    // 登录发生在管理端口；cookie 是 host 级共享的（浏览器不按端口隔离），
    // 所以拿同一个 cookie 去 2224/2225 那些转发端口也该直接通过。
    let c = login_at(&gw, admin, "admin", "pw-123").await.unwrap();
    let r = http(
        gw.addr(),
        "GET",
        "/",
        &[("cookie", &c), ("accept", "*/*")],
        None,
    )
    .await;
    assert_eq!(r.status, 200, "管理端口的会话应能直接访问转发端口");

    gw.token.cancel();
    _u.cancel();
}

#[tokio::test]
async fn the_admin_port_accepts_non_route_users_for_self_service() {
    let (up, _u) = start_upstream().await;
    let (gw, _dir) = start_gateway_with_admin(up).await;
    let admin = gw.admin_addr();

    // viewer 只有 panel 的权限、不是管理员。他登不进任何"没授权"的端口，
    // 但管理端口要让他进来改自己的口令——否则一个只读账号连密码都改不了。
    let c = login_at(&gw, admin, "viewer", "pw-123")
        .await
        .expect("非管理员也应能在管理端口登录");
    let r = http(admin, "GET", "/__admin/me", &[("cookie", &c)], None).await;
    assert_eq!(r.status, 200);
    assert!(r.body.contains("我的账号"));

    // 但管理功能一如既往地关着
    let r = http(admin, "GET", "/__admin/users", &[("cookie", &c)], None).await;
    assert_eq!(r.status, 403);

    gw.token.cancel();
    _u.cancel();
}

#[tokio::test]
async fn admin_listen_cannot_collide_with_a_route_port() {
    // 撞车必须在启动时报错，而不是"谁先绑上谁赢"——输的那个可能是管理面板，
    // 那就再也进不去了。
    let raw = r#"
admin_listen = "127.0.0.1:2224"

[[user]]
name = "a"
hash = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$11111111111111111111111111111111"
routes = ["*"]

[[route]]
name = "p"
listen = "0.0.0.0:2224"
upstream = "127.0.0.1:18788"
"#;
    let cfg: esc_gateway::GatewayConfig = toml::from_str(raw).unwrap();
    let err = cfg.validate().unwrap_err();
    assert!(err.contains("同一个端口"), "{err}");

    // 0.0.0.0 与 127.0.0.1 撞同一端口也算（bind 时会 EADDRINUSE）
    let raw2 = raw.replace(
        "admin_listen = \"127.0.0.1:2224\"",
        "admin_listen = \"0.0.0.0:2224\"",
    );
    let cfg2: esc_gateway::GatewayConfig = toml::from_str(&raw2).unwrap();
    assert!(cfg2.validate().is_err());
}
