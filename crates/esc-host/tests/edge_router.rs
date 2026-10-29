//! 真二进制端到端：启一个 `esc-host` 子进程，把边缘路由从 TOML 一路跑到 HTTP 响应。
//!
//! 为什么值得写：crate 级测试证明了"路由表算得对、转发函数能跑"，但证明不了
//! **组合根接对了** —— `[edge_router]` 段真的被读、插件真的被注册、服务真的
//! 注册进 Ctx、端口真的在 listen。这几步任何一处漏掉，crate 测试都是全绿的。
//!
//! 拓扑：
//!
//! ```text
//!   测试 ── Host: cad.test ──▶ esc-host 上的边缘路由 ──▶ 假后端（标记 X-Backend）
//! ```

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "esc-host-edge-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 占一个端口再放掉，拿到一个"当前空闲"的端口号。
fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// 假后端：读一个请求，回固定响应（带上自己收到的 Host，便于断言）。
///
/// 故意用裸 TCP 而不是引 axum —— 这里验的是"请求有没有到达本机这个端口"，
/// 后端越简单越好。
fn spawn_backend(tag: &'static str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let host = req
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("Host:")
                            .or_else(|| l.strip_prefix("host:"))
                            .map(|h| h.trim().to_string())
                    })
                    .unwrap_or_default();
                let body = format!("backend={tag};host={host}");
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nX-Backend: {tag}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes());
            });
        }
    });
    port
}

fn wait_for_log(path: &Path, what: &str, pred: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        if pred(&text) {
            return text;
        }
        assert!(Instant::now() < deadline, "超时等待「{what}」，当前日志：\n{text}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// 发一个最简请求，返回完整响应文本。
fn http_get(port: u16, host: &str, path: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("连不上边缘路由端口");
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).to_string()
}

#[test]
fn edge_router_from_toml_to_http_response() {
    let dir = temp_dir("e2e");

    // 假后端 + 边缘路由端口（都预先占号再放掉）。
    let backend_cad = spawn_backend("cad-backend");
    let backend_shop = spawn_backend("shop-backend");
    let edge_port = free_port();

    // 隧道引擎是必挂的插件，所以要有一套可用证书（与 isolation.rs 同一套路）。
    let ca = esc_common::pki::Ca::generate("edge-it-ca", 2).unwrap();
    let ca_path = esc_common::pki::write_ca_cert(&dir, "ca", &ca).unwrap();
    let pair = ca.issue_server("hub", &["127.0.0.1".to_string()], 1).unwrap();
    let (cert, key) = esc_common::pki::write_pair(&dir, "hub", &pair).unwrap();
    let engine = dir.join("engine.toml");
    std::fs::write(
        &engine,
        format!(
            "role = \"hub\"\nstore = \"{}\"\n\n[hub]\nlisten = \"127.0.0.1:0\"\n\n[tls]\nca_cert = \"{}\"\ncert = \"{}\"\nkey = \"{}\"\n",
            dir.join("mappings.toml").display(),
            ca_path.display(),
            cert.display(),
            key.display()
        ),
    )
    .unwrap();

    let host_cfg = dir.join("host.toml");
    std::fs::write(
        &host_cfg,
        format!(
            r#"[engine]
config = "{engine}"

[panel]
enabled = false

[edge_router]
enabled = true

[edge_router.config]
listen = ["127.0.0.1:{edge_port}"]

[edge_router.config.routes]
cad = {backend_cad}
shop = {backend_shop}
"#,
            engine = engine.display(),
            edge_port = edge_port,
            backend_cad = backend_cad,
            backend_shop = backend_shop
        ),
    )
    .unwrap();

    let log_path = dir.join("host.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_esc-host"))
        .arg("-c")
        .arg(&host_cfg)
        .env("RUST_LOG", "info")
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .spawn()
        .expect("esc-host 应当能启动");

    let text = wait_for_log(&log_path, "边缘路由就绪", |t| {
        t.contains("edge router mounted") || t.contains("edge router ready")
    });
    // 路由表要打在日志里 —— 运维靠它确认"我以为配了的真的生效了"。
    assert!(text.contains("cad"), "日志里应能看到路由：\n{text}");
    assert!(text.contains(&backend_cad.to_string()), "日志里应能看到目标端口：\n{text}");

    // 1) cad.test → cad-backend
    let resp = http_get(edge_port, "cad.test", "/api/health?x=1");
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    // 首部名大小写不敏感；hyper 会统一输出小写，所以这里也按小写断言。
    assert!(
        resp.to_ascii_lowercase().contains("x-backend: cad-backend"),
        "{resp}"
    );
    assert!(resp.contains("backend=cad-backend"), "{resp}");
    // Host 原样透传（后端可能用它拼绝对链接）。
    assert!(resp.contains("host=cad.test"), "{resp}");

    // 2) 另一个前缀 → 另一个后端（证明不是"都转发到同一个地方"）
    let resp = http_get(edge_port, "shop.test", "/");
    assert!(resp.contains("backend=shop-backend"), "{resp}");

    // 3) 未配置的前缀 → 421 + 可读 JSON
    let resp = http_get(edge_port, "nobody.test", "/");
    assert!(resp.starts_with("HTTP/1.1 421"), "{resp}");
    assert!(resp.contains("no_route"), "{resp}");

    // 4) 纯 IP 访问（Host 里没有域名）→ 421，而不是被当成前缀
    let resp = http_get(edge_port, "127.0.0.1", "/");
    assert!(resp.starts_with("HTTP/1.1 421"), "{resp}");

    // 5) 边缘路由的控制服务已注册（面板靠这个 service 名调 HostCall）
    assert!(
        text.contains("edge control service registered"),
        "宿主应注册 esc.edge 控制服务：\n{text}"
    );

    // 6) 宿主仍在运行（边缘路由不该影响隧道/宿主生命周期）
    assert!(
        child.try_wait().expect("try_wait 失败").is_none(),
        "宿主应持续运行：\n{}",
        std::fs::read_to_string(&log_path).unwrap_or_default()
    );

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}


/// 持久化：真二进制 + 真 TOML + 真状态文件，重启一次验证路由还在。
///
/// 这条测的是**宿主接线**（`[edge_router.config] store` 被透传、插件 mount 时
/// 读它），不是插件内部逻辑（那有单测与组件级集成测试守着）。
#[test]
fn routes_persist_across_restart_when_store_is_set() {

    let dir = temp_dir("persist");
    let store = dir.join("edge-routes.toml");
    let edge_port = free_port();
    let backend_cad = spawn_backend("cad-backend");

    // 证书（隧道插件是必挂的）
    let ca = esc_common::pki::Ca::generate("persist-ca", 2).unwrap();
    let ca_path = esc_common::pki::write_ca_cert(&dir, "ca", &ca).unwrap();
    let pair = ca.issue_server("hub", &["127.0.0.1".to_string()], 1).unwrap();
    let (cert, key) = esc_common::pki::write_pair(&dir, "hub", &pair).unwrap();
    let engine = dir.join("engine.toml");
    std::fs::write(
        &engine,
        format!(
            "role = \"hub\"\nstore = \"{}\"\n\n[hub]\nlisten = \"127.0.0.1:0\"\n\n[tls]\nca_cert = \"{}\"\ncert = \"{}\"\nkey = \"{}\"\n",
            dir.join("mappings.toml").display(),
            ca_path.display(),
            cert.display(),
            key.display()
        ),
    )
    .unwrap();

    let host_cfg = dir.join("host.toml");
    std::fs::write(
        &host_cfg,
        format!(
            "[engine]\nconfig = \"{engine}\"\n\n[panel]\nenabled = false\n\n[edge_router]\nenabled = true\n\n[edge_router.config]\nlisten = [\"127.0.0.1:{edge_port}\"]\nstore = \"{store}\"\n\n[edge_router.config.routes]\ncad = {backend_cad}\n",
            engine = engine.display(),
            edge_port = edge_port,
            store = store.display(),
            backend_cad = backend_cad,
        ),
    )
    .unwrap();

    let log_path = dir.join("host.log");
    let spawn_host = || {
        let log = std::fs::File::create(&log_path).unwrap();
        Command::new(env!("CARGO_BIN_EXE_esc-host"))
            .arg("-c")
            .arg(&host_cfg)
            .env("RUST_LOG", "info")
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .expect("esc-host 应当能启动")
    };

    // ── 第一代：基线只有 cad ──
    let mut child = spawn_host();
    wait_for_log(&log_path, "边缘路由就绪", |t| t.contains("edge router mounted"));
    assert!(http_get(edge_port, "cad.x.com", "/").contains("cad-backend"));

    // 通过面板那个 HostCall 链路改路由？宿主这里没起面板，所以直接改状态文件
    // 模拟"面板改过"—— 这不弱化测试价值：真正要验的是"启动时读状态文件"。
    std::fs::write(
        &store,
        "[routes]\ncad = 2229\nshop = 2230\n",
    )
    .unwrap();

    // 让状态文件**先于**进程退出，然后重启
    let _ = child.kill();
    let _ = child.wait();

    // ── 第二代：必须读到状态文件，而不是 TOML 基线 ──
    std::fs::remove_file(&log_path).ok();
    let mut child = spawn_host();
    let text = wait_for_log(&log_path, "第二次启动", |t| t.contains("edge router mounted"));
    assert!(
        text.contains("from_store=true") || text.contains("路由表来自状态文件"),
        "日志应说明路由来自状态文件：\n{text}"
    );
    // cad 现在指向 2229（没人监听）→ 502；说明用的是状态文件里的端口，不是 2228 基线
    let resp = http_get(edge_port, "cad.x.com", "/");
    assert!(
        resp.contains("502") && resp.contains("2229"),
        "应使用状态文件里的 cad=2229：{resp}"
    );
    // shop 也来自状态文件（基线的 TOML 里根本没有它）
    let resp = http_get(edge_port, "shop.x.com", "/");
    assert!(resp.contains("502") && resp.contains("2230"), "shop 应来自状态文件：{resp}");

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 删掉状态文件 → 回到 TOML 基线（回滚路径）。
#[test]
fn deleting_the_store_file_returns_to_the_baseline() {
    let dir = temp_dir("rollback");
    let store = dir.join("edge-routes.toml");
    let edge_port = free_port();
    let backend = spawn_backend("base-backend");

    let ca = esc_common::pki::Ca::generate("rollback-ca", 2).unwrap();
    let ca_path = esc_common::pki::write_ca_cert(&dir, "ca", &ca).unwrap();
    let pair = ca.issue_server("hub", &["127.0.0.1".to_string()], 1).unwrap();
    let (cert, key) = esc_common::pki::write_pair(&dir, "hub", &pair).unwrap();
    let engine = dir.join("engine.toml");
    std::fs::write(
        &engine,
        format!(
            "role = \"hub\"\nstore = \"{}\"\n\n[hub]\nlisten = \"127.0.0.1:0\"\n\n[tls]\nca_cert = \"{}\"\ncert = \"{}\"\nkey = \"{}\"\n",
            dir.join("mappings.toml").display(),
            ca_path.display(),
            cert.display(),
            key.display()
        ),
    )
    .unwrap();

    // 状态文件故意指向一个没人监听的端口，基线指向真后端
    std::fs::write(&store, "[routes]\ncad = 1\n").unwrap();
    let host_cfg = dir.join("host.toml");
    std::fs::write(
        &host_cfg,
        format!(
            "[engine]\nconfig = \"{engine}\"\n\n[panel]\nenabled = false\n\n[edge_router]\nenabled = true\n\n[edge_router.config]\nlisten = [\"127.0.0.1:{edge_port}\"]\nstore = \"{store}\"\n\n[edge_router.config.routes]\ncad = {backend}\n",
            engine = engine.display(),
            edge_port = edge_port,
            store = store.display(),
            backend = backend,
        ),
    )
    .unwrap();

    let log_path = dir.join("host.log");
    let spawn_host = || {
        let log = std::fs::File::create(&log_path).unwrap();
        Command::new(env!("CARGO_BIN_EXE_esc-host"))
            .arg("-c")
            .arg(&host_cfg)
            .env("RUST_LOG", "info")
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .expect("esc-host 应当能启动")
    };

    // 第一代：状态文件生效（502，端口 1）
    let mut child = spawn_host();
    wait_for_log(&log_path, "就绪", |t| t.contains("edge router mounted"));
    assert!(http_get(edge_port, "cad.x.com", "/").contains("502"));
    let _ = child.kill();
    let _ = child.wait();

    // 删掉状态文件 → 恢复到基线（200，真后端）
    std::fs::remove_file(&store).unwrap();
    std::fs::remove_file(&log_path).ok();
    let mut child = spawn_host();
    wait_for_log(&log_path, "第二次启动", |t| t.contains("edge router mounted"));
    let resp = http_get(edge_port, "cad.x.com", "/");
    assert!(
        resp.contains("200") && resp.contains("base-backend"),
        "删掉状态文件后应回到 TOML 基线：{resp}"
    );

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}
