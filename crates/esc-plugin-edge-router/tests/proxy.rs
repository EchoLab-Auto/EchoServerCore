//! 端到端：真的起监听、真的转发、真的查状态码。
//!
//! 这组测试是「它能用」的唯一证据 —— 单元测试证明路由表算得对，但证明不了
//! `Host` 首部真的被读到、body 真的被搬过去、`X-Forwarded-For` 真的是追加。
//! 全部在 127.0.0.1 的临时端口上跑（`:0`），不会碰到机器上任何真实服务。

use std::collections::BTreeMap;
use std::net::SocketAddr;

use axum::extract::{Request, State};
use axum::response::IntoResponse;
use axum::Router;
use esc_plugin_edge_router::{bind, spawn, EdgeRouter, RouterConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// 假后端：把收到的请求原样回显成 JSON，用来断言"隧道里过了什么"。
async fn echo_backend(State(tag): State<&'static str>, req: Request) -> impl IntoResponse {
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, 4 * 1024 * 1024).await.unwrap();
    let mut headers = serde_json::Map::new();
    for (k, v) in parts.headers.iter() {
        headers.insert(
            k.as_str().to_string(),
            serde_json::Value::String(v.to_str().unwrap_or("<binary>").to_string()),
        );
    }
    let body_text = String::from_utf8_lossy(&bytes).to_string();
    let out = serde_json::json!({
        "tag": tag,
        "method": parts.method.to_string(),
        "uri": parts.uri.to_string(),
        "headers": serde_json::Value::Object(headers),
        "body": body_text,
    });
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        out.to_string(),
    )
        .into_response()
}

/// 起一个假后端，返回它的端口。
async fn start_backend(tag: &'static str) -> u16 {
    let app = Router::new().fallback(echo_backend).with_state(tag);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    port
}

/// 起一个边缘路由（路由表见参数），返回它的地址 + 句柄。
fn start_router(routes: &[(&str, u16)], default: Option<u16>) -> (SocketAddr, esc_plugin_edge_router::ServerHandle) {
    start_router_with_store(routes, default, None)
}

fn start_router_with_store(
    routes: &[(&str, u16)],
    default: Option<u16>,
    store: Option<&std::path::Path>,
) -> (SocketAddr, esc_plugin_edge_router::ServerHandle) {
    let cfg = RouterConfig {
        listen: vec!["127.0.0.1:0".into()],
        routes: routes.iter().map(|(k, v)| (k.to_string(), *v)).collect::<BTreeMap<_, _>>(),
        default_port: default,
        timeout_ms: 5_000,
        store: store.map(|p| p.to_path_buf()),
    };
    let listeners = bind(&cfg).unwrap();
    let addrs: Vec<SocketAddr> = listeners.iter().filter_map(|l| l.local_addr().ok()).collect();
    let router = EdgeRouter::load(&cfg, addrs.clone()).expect("加载路由表");
    let handle = spawn(router, listeners).unwrap();
    (handle.addrs()[0], handle)
}

/// 极简 HTTP/1.1 客户端：写请求、读到 EOF 为止。
///
/// 不用 hyper 客户端是故意的 —— 被测对象就是一个 HTTP 代理，用另一个 HTTP
/// 实现去测它，容易把"两个客户端都同意"的假设当成"协议正确"。这里直接看
/// 原始字节。
async fn raw_request(addr: SocketAddr, host: &str, path: &str, extra: &str) -> String {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n{extra}Connection: close\r\n\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    String::from_utf8_lossy(&buf).to_string()
}

fn status_line(resp: &str) -> &str {
    resp.lines().next().unwrap_or("")
}

fn body_of(resp: &str) -> &str {
    resp.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn routes_by_first_label_and_reports_the_prefix() {
    let backend = start_backend("cad-backend").await;
    let (edge, handle) = start_router(&[("cad", backend)], None);

    let resp = raw_request(edge, "cad.example.com", "/api/health?x=1", "").await;
    assert!(status_line(&resp).contains("200"), "{resp}");
    let body = body_of(&resp);
    // 后端收到的 path 必须原样（含查询串）。
    assert!(body.contains("/api/health?x=1"), "{body}");
    // Host 必须原样保留（后端可能用它拼绝对链接）。
    assert!(body.contains("cad.example.com"), "{body}");
    // XFF 由网关写入。
    assert!(body.contains("x-forwarded-for"), "{body}");
    assert!(body.contains("x-forwarded-proto"), "{body}");

    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_prefixes_reach_two_different_backends() {
    let a = start_backend("A").await;
    let b = start_backend("B").await;
    let (edge, handle) = start_router(&[("cad", a), ("panel", b)], None);

    let ra = raw_request(edge, "cad.x.com", "/", "").await;
    let rb = raw_request(edge, "panel.x.com", "/", "").await;
    assert!(body_of(&ra).contains("\"tag\":\"A\""), "{ra}");
    assert!(body_of(&rb).contains("\"tag\":\"B\""), "{rb}");

    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unrouted_host_gets_421_with_actionable_json() {
    let (edge, handle) = start_router(&[], None);
    let resp = raw_request(edge, "nobody.example.com", "/", "").await;
    assert!(status_line(&resp).contains("421"), "{resp}");
    let body = body_of(&resp);
    assert!(body.contains("no_route"), "{body}");
    // 报错要能自己说明怎么修：带上 host 与解析出的前缀。
    assert!(body.contains("nobody.example.com"), "{body}");
    assert!(body.contains("\"prefix\":\"nobody\""), "{body}");
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bare_ip_is_not_mistaken_for_a_prefix() {
    // 回归：`114.132.241.46` 的第一段是 `114`，绝不能因此命中名为 114 的路由。
    let backend = start_backend("should-not-be-hit").await;
    let (edge, handle) = start_router(&[("114", backend)], None);
    let resp = raw_request(edge, "114.132.241.46", "/", "").await;
    assert!(status_line(&resp).contains("421"), "{resp}");
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dead_backend_yields_502_naming_the_port() {
    // 绑一个端口再立刻释放，拿到一个"几乎肯定没人监听"的端口号。
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead = probe.local_addr().unwrap().port();
    drop(probe);

    let (edge, handle) = start_router(&[("cad", dead)], None);
    let resp = raw_request(edge, "cad.x.com", "/", "").await;
    assert!(status_line(&resp).contains("502"), "{resp}");
    let body = body_of(&resp);
    assert!(body.contains("bad_gateway"), "{body}");
    assert!(body.contains(&dead.to_string()), "错误里要给出目标端口：{body}");
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn post_body_and_multiline_headers_survive() {
    let backend = start_backend("echo").await;
    let (edge, handle) = start_router(&[("cad", backend)], None);

    let mut s = TcpStream::connect(edge).await.unwrap();
    let body = "a=1&b=%E4%B8%AD%E6%96%87";
    let req = format!(
        "POST /submit HTTP/1.1\r\nHost: cad.x.com\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    let resp = String::from_utf8_lossy(&buf).to_string();
    assert!(status_line(&resp).contains("200"), "{resp}");
    assert!(body_of(&resp).contains(body), "请求体必须原样到达后端：{resp}");
    assert!(body_of(&resp).contains("\"method\":\"POST\""), "{resp}");
    handle.shutdown().await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Upgrade（WebSocket）：101 之后必须变成真正的双向字节流
// ─────────────────────────────────────────────────────────────────────────────

/// 假 WebSocket 后端：握手回 101，之后进入**回显**循环（收到什么原样吐回）。
///
/// 用裸 std TcpListener + 线程，而不是 axum —— 这里要验的正是"101 之后
/// 连接不再走 HTTP 状态机"，任何 HTTP 框架都会把这一点藏起来。
fn spawn_ws_echo_backend(require_upgrade: bool) -> u16 {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            std::thread::spawn(move || {
                // 读到请求头结束（\r\n\r\n）
                let mut head = Vec::new();
                let mut tmp = [0u8; 2048];
                loop {
                    let n = s.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    head.extend_from_slice(&tmp[..n]);
                    if head.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let req = String::from_utf8_lossy(&head).to_ascii_lowercase();
                if require_upgrade && !req.contains("upgrade: websocket") {
                    // 回归点：边缘路由**必须**把 Upgrade 首部传给后端，
                    // 否则后端根本不知道要升级。
                    let _ = s.write_all(
                        b"HTTP/1.1 400 Bad Request\r\nContent-Length: 20\r\nConnection: close\r\n\r\nmissing upgrade hdr\r\n",
                    );
                    return;
                }
                if !require_upgrade {
                    // 故意拒绝升级：验证"对端拒绝"被原样返回而不是变成 502
                    let _ = s.write_all(
                        b"HTTP/1.1 426 Upgrade Required\r\nContent-Length: 6\r\nConnection: close\r\n\r\nnope!\n",
                    );
                    return;
                }
                let _ = s.write_all(
                    b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nX-Backend: ws\r\n\r\n",
                );
                let _ = s.flush();
                // 回显
                loop {
                    let n = s.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    if s.write_all(&tmp[..n]).is_err() {
                        return;
                    }
                }
            });
        }
    });
    port
}

/// 连上边缘路由、发 Upgrade 请求、返回 (连接, 响应头文本)。
async fn start_upgrade(edge: SocketAddr, host: &str, path: &str) -> (TcpStream, String) {
    use tokio::io::AsyncWriteExt;
    let mut s = TcpStream::connect(edge).await.unwrap();
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
    );
    s.write_all(req.as_bytes()).await.unwrap();
    // 读到响应头结束为止（不能多读：之后是业务字节）
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = s.read(&mut byte).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        head.push(byte[0]);
        if head.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    (s, String::from_utf8_lossy(&head).to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn websocket_upgrade_becomes_a_real_bidirectional_pipe() {
    let backend = spawn_ws_echo_backend(true);
    let (edge, handle) = start_router(&[("cad", backend)], None);

    let (mut client, head) = start_upgrade(edge, "cad.x.com", "/ws").await;
    assert!(
        head.starts_with("HTTP/1.1 101"),
        "应回 101，实际：{head}"
    );
    let lower = head.to_ascii_lowercase();
    // 交换协议的两个首部必须原样回给客户端，否则浏览器会认为握手失败。
    assert!(lower.contains("upgrade: websocket"), "{head}");
    assert!(lower.contains("connection: upgrade"), "{head}");
    // 后端自定义的首部也要透传。
    assert!(lower.contains("x-backend: ws"), "{head}");

    // 现在是裸字节：发什么都该被后端原样吐回。
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for payload in ["ping-1", "中文也要原样通过", "x"] {
        client.write_all(payload.as_bytes()).await.unwrap();
        let mut buf = vec![0u8; payload.len()];
        tokio::time::timeout(std::time::Duration::from_secs(5), client.read_exact(&mut buf))
            .await
            .expect("等回显超时（说明 101 之后没有真的对接字节流）")
            .expect("读回显失败");
        assert_eq!(
            String::from_utf8_lossy(&buf),
            payload,
            "回显内容不一致（双向搬运有问题）"
        );
    }

    // 服务端关闭 → 客户端读到 EOF（copy_bidirectional 正确收尾）
    drop(client);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // 计数器：至少有一次成功升级
    let st = handle.router().snapshot();
    assert!(st.counters.upgraded >= 1, "{st:?}");
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backend_refusing_upgrade_is_passed_through_not_masked_as_502() {
    // 对端回 426 是**正常**结果（它不想升级），不能变成 502。
    let backend = spawn_ws_echo_backend(false);
    let (edge, handle) = start_router(&[("cad", backend)], None);

    let (mut client, head) = start_upgrade(edge, "cad.x.com", "/ws").await;
    assert!(head.starts_with("HTTP/1.1 426"), "应原样返回 426：{head}");

    // body 也要透传
    use tokio::io::AsyncReadExt;
    let mut body = String::new();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), client.read_to_string(&mut body)).await;
    assert!(body.contains("nope!"), "拒绝原因应透传：{body:?}");

    let st = handle.router().snapshot();
    assert_eq!(st.counters.upgraded, 0, "被拒绝的升级不该计数：{st:?}");
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upgrade_to_dead_backend_is_502_with_port() {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead = probe.local_addr().unwrap().port();
    drop(probe);
    let (edge, handle) = start_router(&[("cad", dead)], None);
    let (_, head) = start_upgrade(edge, "cad.x.com", "/ws").await;
    assert!(head.contains("502"), "{head}");
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runtime_upsert_and_delete_take_effect_without_restart() {
    let a = start_backend("A").await;
    let b = start_backend("B").await;
    let (edge, handle) = start_router(&[("cad", a)], None);

    assert!(body_of(&raw_request(edge, "cad.x.com", "/", "").await).contains("\"tag\":\"A\""));

    // 指向 B（覆盖同前缀）
    esc_plugin_edge_router::EdgeRouter::upsert_route(&handle.router(), "cad", b).unwrap();
    assert!(
        body_of(&raw_request(edge, "cad.x.com", "/", "").await).contains("\"tag\":\"B\""),
        "覆盖同前缀应立刻生效"
    );

    // 新前缀
    esc_plugin_edge_router::EdgeRouter::upsert_route(&handle.router(), "shop", b).unwrap();
    assert!(body_of(&raw_request(edge, "shop.x.com", "/", "").await).contains("\"tag\":\"B\""));

    // 删除后回到 421
    esc_plugin_edge_router::EdgeRouter::delete_route(&handle.router(), "shop").unwrap();
    assert!(status_line(&raw_request(edge, "shop.x.com", "/", "").await).contains("421"));

    // 状态里的计数要对得上（至少 requests 涨了、unrouted 有值）
    let st = handle.router().snapshot();
    assert!(st.counters.requests >= 4, "{st:?}");
    assert!(st.counters.unrouted >= 1, "{st:?}");
    assert_eq!(st.routes.len(), 1, "{st:?}");
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_releases_the_port() {
    let backend = start_backend("A").await;
    let (edge, handle) = start_router(&[("cad", backend)], None);
    let port = edge.port();
    assert!(body_of(&raw_request(edge, "cad.x.com", "/", "").await).contains("\"tag\":\"A\""));

    handle.shutdown().await;

    // 关闭后必须能立刻重新绑同一个端口：否则"改配置 → 重启"会失败。
    let mut ok = false;
    for _ in 0..50 {
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            ok = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(ok, "端口 {port} 在 shutdown 后没有被释放");
}

// ─────────────────────────────────────────────────────────────────────────────
// 插件层：配置错误必须在 mount 阶段失败，而不是"挂上了但全 421"
// ─────────────────────────────────────────────────────────────────────────────

mod plugin_level {
    use super::*;
    use echo_context::Ctx;
    use echo_plugin::{MountContext, Plugin, PluginRegistry};
    use esc_plugin_edge_router::EdgeRouterPlugin;
    use std::sync::Arc;

    /// 挂载一次，把**注册表也一起返回**。
    ///
    /// 必须返回注册表：它持有 `mount()` 返回的 `Disposer`，而 `Disposer` 的
    /// `Drop` 会真的把服务从 `Ctx` 里摘掉（框架语义：注册即副作用、必须可逆）。
    /// 早期版本的测试只返回 `ctx`，于是注册表一出作用域、服务就消失了 ——
    /// 表现是 `resolve()` 报 "service key not available"，看起来像插件没挂上，
    /// 其实是测试自己把它撤了。真实宿主当然会一直持有注册表。
    fn mount_with(config: serde_json::Value) -> (Vec<(String, String)>, Arc<Ctx>, PluginRegistry) {
        let ctx = Arc::new(Ctx::default());
        let registry = PluginRegistry::default();
        let plugin: Arc<dyn Plugin> = Arc::new(EdgeRouterPlugin::new());
        registry.register(plugin).unwrap();
        registry.set_config(esc_plugin_edge_router::PLUGIN_ID, config);
        let errors = registry.mount_all(&MountContext::new().with_ctx(ctx.clone()));
        (errors, ctx, registry)
    }

    #[test]
    fn bad_config_fails_closed_at_mount_time() {
        // 没有 listen：不能默认去绑 :80。
        let (errors, _, _reg) = mount_with(serde_json::json!({ "routes": { "cad": 2228 } }));
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].1.contains("listen"), "{errors:?}");
    }

    #[test]
    fn occupied_port_fails_closed_with_a_bind_error() {
        // 先占住一个端口，再让插件去绑它 —— mount 必须失败，且说清是 bind。
        let squatter = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = squatter.local_addr().unwrap();
        let (errors, _, _reg) = mount_with(serde_json::json!({ "listen": [addr.to_string()] }));
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].1.contains("无法监听"),
            "错误应指出来是绑定失败：{errors:?}"
        );
        drop(squatter);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mounted_plugin_serves_and_exposes_control_in_ctx() {
        let backend = start_backend("A").await;
        // 用 :0 让系统分配，mount 之后从 Ctx 里的控制面读出真实端口。
        let (errors, ctx, _registry) = mount_with(serde_json::json!({
            "listen": ["127.0.0.1:0"],
            "routes": { "cad": backend }
        }));
        assert!(errors.is_empty(), "{errors:?}");

        // 服务键必须已注册（宿主/面板据此发现能力）。
        let ctl = esc_plugin_edge_router::resolve(&ctx).unwrap();
        let st = ctl.status().await.unwrap();
        assert_eq!(st.routes.len(), 1);
        assert_eq!(st.routes[0].prefix, "cad");
        assert_eq!(st.routes[0].port, backend);

        let listen: SocketAddr = st.listens[0].parse().expect("listens 应是 host:port");
        let resp = raw_request(listen, "cad.anywhere.io", "/", "").await;
        assert!(status_line(&resp).contains("200"), "{resp}");
        assert!(body_of(&resp).contains("\"tag\":\"A\""), "{resp}");

        // 控制面能改路由（面板将来的入口就是它）。
        ctl.upsert_route(esc_plugin_edge_router::EdgeRouteUpsert {
            prefix: "shop".into(),
            port: backend,
            old_prefix: None,
        })
        .await
        .unwrap();
        let resp = raw_request(listen, "shop.anywhere.io", "/", "").await;
        assert!(status_line(&resp).contains("200"), "{resp}");
        ctl.delete_route("shop").await.unwrap();
        let resp = raw_request(listen, "shop.anywhere.io", "/", "").await;
        assert!(status_line(&resp).contains("421"), "{resp}");
    }
}


// ─────────────────────────────────────────────────────────────────────────────
// 持久化：面板上的改动能活过重启
// ─────────────────────────────────────────────────────────────────────────────

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "esc-edge-persist-{tag}-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runtime_edits_survive_a_restart_when_store_is_configured() {
    let dir = tmpdir("restart");
    let store = dir.join("edge-routes.toml");
    let backend = start_backend("A").await;

    // 第一代：基线有一条 cad，运行期加 shop 并删掉 cad 的兄弟条目
    let (edge1, h1) = start_router_with_store(&[("cad", backend)], None, Some(&store));
    assert!(body_of(&raw_request(edge1, "cad.x.com", "/", "").await).contains("\"tag\":\"A\""));
    esc_plugin_edge_router::EdgeRouter::upsert_route(&h1.router(), "shop", backend).unwrap();
    h1.shutdown().await;

    // 状态文件里应该已经有两条
    let text = std::fs::read_to_string(&store).unwrap();
    assert!(text.contains("cad = "), "{text}");
    assert!(text.contains("shop = "), "{text}");

    // 第二代：**故意给一条不同的基线**（模拟"配置文件没跟着改"）
    // 生效的必须是状态文件，而不是基线 —— 否则用户的改动等于白做。
    let (edge2, h2) = start_router_with_store(&[("other", 1)], None, Some(&store));
    let st = h2.router().snapshot();
    assert_eq!(st.store.as_deref(), Some(store.to_str().unwrap()));
    let prefixes: Vec<_> = st.routes.iter().map(|r| r.prefix.as_str()).collect();
    assert!(prefixes.contains(&"cad"), "状态文件里的 cad 应生效：{st:?}");
    assert!(prefixes.contains(&"shop"), "状态文件里的 shop 应生效：{st:?}");
    assert!(!prefixes.contains(&"other"), "基线不该混进来：{st:?}");
    assert!(body_of(&raw_request(edge2, "shop.x.com", "/", "").await).contains("\"tag\":\"A\""));
    h2.shutdown().await;

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_store_edits_are_memory_only() {
    let backend = start_backend("A").await;
    let (edge, handle) = start_router(&[("cad", backend)], None);
    esc_plugin_edge_router::EdgeRouter::upsert_route(&handle.router(), "shop", backend).unwrap();
    assert!(handle.router().snapshot().store.is_none(), "未配 store 应报告 None");
    assert!(body_of(&raw_request(edge, "shop.x.com", "/", "").await).contains("\"tag\":\"A\""));
    handle.shutdown().await;
    // 没有 store 就没有任何文件被创建 —— 不能偷偷写到当前工作目录。
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn save_failure_leaves_memory_untouched() {
    // 让状态文件指向一个**不可能写入**的路径（父路径是文件而非目录）。
    let dir = tmpdir("failwrite");
    let blocker = dir.join("blocker");
    std::fs::write(&blocker, b"i am a file, not a dir").unwrap();
    let store = blocker.join("routes.toml"); // create_dir_all 必然失败

    let backend = start_backend("A").await;
    // 注意用 `new` 而不是 `load`：`load` 会先读盘，而这个坏路径连读都会失败
    // （ENOTDIR），那样测到的就是"加载失败"而不是"写盘失败"。本测试要验的是
    // upsert 的原子性，所以直接构造出"内存正常、store 不可写"的状态。
    let cfg = RouterConfig {
        listen: vec!["127.0.0.1:0".into()],
        routes: [("cad".to_string(), backend)].into_iter().collect(),
        default_port: None,
        timeout_ms: 5_000,
        store: Some(store.clone()),
    };
    let listeners = esc_plugin_edge_router::bind(&cfg).unwrap();
    let addrs: Vec<SocketAddr> = listeners.iter().filter_map(|l| l.local_addr().ok()).collect();
    let router = EdgeRouter::new(&cfg, addrs);
    let handle = esc_plugin_edge_router::spawn(router, listeners).unwrap();
    let edge = handle.addrs()[0];

    let err = esc_plugin_edge_router::EdgeRouter::upsert_route(&handle.router(), "shop", backend)
        .unwrap_err();
    assert_eq!(err.kind, esc_control_api::ErrorKind::Other, "{err:?}");

    // 关键：内存里也**没有** shop —— "返回失败"必须意味着"确实没生效"
    let prefixes: Vec<String> = handle.router().snapshot().routes.iter().map(|r| r.prefix.clone()).collect();
    assert_eq!(prefixes, vec!["cad".to_string()], "写盘失败不该留下内存改动");
    assert!(status_line(&raw_request(edge, "shop.x.com", "/", "").await).contains("421"));
    handle.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_is_persisted_too() {
    let dir = tmpdir("delete");
    let store = dir.join("routes.toml");
    let backend = start_backend("A").await;
    let (_edge, handle) =
        start_router_with_store(&[("cad", backend), ("shop", backend)], None, Some(&store));
    esc_plugin_edge_router::EdgeRouter::delete_route(&handle.router(), "shop").unwrap();
    handle.shutdown().await;

    // 重启后 shop 不该回来（这正是"整体覆盖基线"能成立的前提）
    let (edge2, h2) = start_router_with_store(&[("cad", backend), ("shop", backend)], None, Some(&store));
    let prefixes: Vec<String> = h2.router().snapshot().routes.iter().map(|r| r.prefix.clone()).collect();
    assert_eq!(prefixes, vec!["cad".to_string()], "删掉的条目不该被基线复活");
    assert!(status_line(&raw_request(edge2, "shop.x.com", "/", "").await).contains("421"));
    h2.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}
