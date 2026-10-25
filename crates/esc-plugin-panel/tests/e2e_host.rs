//! 端到端：**三层都是真的**。
//!
//! ```text
//!   HTTP 测试客户端
//!        │  POST /api/mappings
//!        ▼
//!   esc-plugin-panel（真子进程，stdio 插件）
//!        │  HostCall("esc.control", "mapping.upsert")
//!        ▼
//!   esc-host（本测试进程内：PluginSupervisor + Ctx + 宿主服务）
//!        │  dispatch(ControlRequest) → Arc<dyn TunnelControl>
//!        ▼
//!   esc-engine（hub 角色：真的绑端口、真的走 mTLS + yamux）
//!        ▲
//!        │  mTLS + yamux（第二个引擎实例扮演 spoke）
//!   esc-engine（spoke 角色）──▶ 127.0.0.1 上的回显服务
//! ```
//!
//! 这条链路通过，就同时证明了：
//!
//! * 面板与引擎**没有**编译期耦合（面板只认 JSON 契约）；
//! * 宿主服务 / `HostCall` 的接线正确；
//! * 通过 Web API 加的映射能立刻转发真实流量；
//! * 面板是独立进程，宿主退出时它跟着干净消失。

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use echo_plugin_host::api::{InvokeContext, InvokeOutcome};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use esc_common::config::{HubSection, SpokeSection, TlsConfig};
use esc_control_api::{Mapping, MappingState, MappingUpsert, Role, TunnelControl};
use esc_engine::{Engine, EngineConfig};
use esc_host::{Host, HostConfig};

use common::CertFixture;

async fn wait_for<F, Fut>(what: &str, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if f().await {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "超时等待：{what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn spawn_echo() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                loop {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            if sock.write_all(&buf[..n]).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
    });
    addr
}

async fn roundtrip(addr: SocketAddr, payload: &[u8]) -> Option<Vec<u8>> {
    let mut sock = TcpStream::connect(addr).await.ok()?;
    sock.write_all(payload).await.ok()?;
    let mut got = vec![0u8; payload.len()];
    tokio::time::timeout(Duration::from_secs(5), sock.read_exact(&mut got))
        .await
        .ok()?
        .ok()?;
    Some(got)
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "esc-e2e-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 写引擎配置并返回路径。
fn write_engine_toml(
    dir: &std::path::Path,
    certs: &CertFixture,
    server_cert: &std::path::Path,
    server_key: &std::path::Path,
) -> std::path::PathBuf {
    let text = format!(
        r#"
role  = "hub"
store = "{store}"

[hub]
listen = "127.0.0.1:0"

[tls]
ca_cert = "{ca}"
cert    = "{cert}"
key     = "{key}"
"#,
        store = dir.join("mappings.toml").display(),
        ca = certs.ca.display(),
        cert = server_cert.display(),
        key = server_key.display(),
    );
    let path = dir.join("engine.toml");
    std::fs::write(&path, text).unwrap();
    path
}

/// 问面板它绑在哪个地址上（配置里写的是 `:0`）。
async fn panel_addr(host: &Host) -> String {
    let panel = host.panel().expect("面板插件应当已启动").clone();
    for _ in 0..60 {
        if let InvokeOutcome::Ok { result, .. } = panel
            .invoke("esc.panel", InvokeContext::default(), Value::Null)
            .await
        {
            if result.get("serving").and_then(Value::as_bool) == Some(true) {
                if let Some(listen) = result.get("listen").and_then(Value::as_str) {
                    return listen.to_string();
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("面板没有报告监听地址");
}

async fn wait_engine_ready(host: &Host) {
    let ctl = host.control();
    wait_for("引擎就绪", || {
        let ctl = ctl.clone();
        async move { ctl.status().await.is_ok() }
    })
    .await;
}

/// 起一个真实 spoke 引擎，暴露 `name → echo`。
async fn start_spoke(
    certs: &CertFixture,
    control_listen: String,
    name: &str,
    local: SocketAddr,
) -> std::sync::Arc<Engine> {
    Engine::start(EngineConfig {
        role: Role::Spoke,
        tls: TlsConfig {
            ca_cert: certs.ca.clone(),
            cert: certs.spoke_cert.clone(),
            key: certs.spoke_key.clone(),
        },
        hub: HubSection::default(),
        spoke: SpokeSection {
            id: "e2e-spoke".into(),
            server: control_listen,
            backoff_min_ms: 50,
            backoff_max_ms: 300,
            max_streams: 32,
            server_name: Some("127.0.0.1".into()),
        },
        store_path: None,
        seed: vec![Mapping {
            id: Mapping::new_id(),
            name: name.into(),
            enabled: true,
            listen: None,
            spoke: None,
            local: Some(local.to_string()),
            note: String::new(),
        }],
        source: None,
    })
    .await
    .expect("spoke 引擎应能启动")
}

// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn web_api_drives_the_engine_and_the_panel_dies_with_the_host() {
    let certs = CertFixture::hub();
    let dir = temp_dir("api");
    let engine_path = write_engine_toml(&dir, &certs, &certs.hub_cert, &certs.hub_key);

    let host_toml = format!(
        r#"
[engine]
config = "{engine}"

[panel]
enabled = true
command = "{panel}"

[panel.config]
listen = "127.0.0.1:0"
auth   = "admin:secret"
title  = "e2e 面板"
"#,
        engine = engine_path.display(),
        panel = env!("CARGO_BIN_EXE_esc-plugin-panel"),
    );
    let host = Host::start(HostConfig::parse(&host_toml).unwrap(), None)
        .await
        .expect("宿主应能启动");
    wait_engine_ready(&host).await;

    let addr = panel_addr(&host).await;
    let auth = common::basic("admin:secret");

    // 1) 认证
    let (code, _) = common::http(&addr, "GET", "/api/status", None, None).await;
    assert_eq!(code, 401, "未认证应被拒");
    let (code, body) = common::http(&addr, "GET", "/api/status", None, Some(&auth)).await;
    assert_eq!(code, 200, "{body}");
    assert!(body.contains("\"role\":\"hub\""), "{body}");

    // 2) 页面本身可访问
    let (code, page) = common::http(&addr, "GET", "/", None, Some(&auth)).await;
    assert_eq!(code, 200);
    assert!(page.contains("e2e 面板"), "标题应生效");

    // 3) 新建映射 → 端口真的绑上
    let (code, body) = common::http(
        &addr,
        "POST",
        "/api/mappings",
        Some(r#"{"name":"demo","listen":"127.0.0.1:0","enabled":true,"note":"e2e"}"#),
        Some(&auth),
    )
    .await;
    assert_eq!(code, 200, "{body}");
    let created: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(created["state"], "active", "{created}");
    let id = created["id"].as_str().unwrap().to_string();
    let bound: SocketAddr = created["bound"].as_str().unwrap().parse().unwrap();
    assert!(
        TcpStream::connect(bound).await.is_ok(),
        "公网端口应当已经可以建立 TCP 连接"
    );

    // 4) 列表 / 导出
    let (_, body) = common::http(&addr, "GET", "/api/mappings", None, Some(&auth)).await;
    assert!(body.contains("\"demo\""), "{body}");
    let (code, body) = common::http(&addr, "GET", "/api/export?role=hub", None, Some(&auth)).await;
    assert_eq!(code, 200, "{body}");
    let exported: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(exported["filename"], "hub.toml");
    let toml_text = exported["toml"].as_str().unwrap();
    assert!(toml_text.contains("[[service]]"), "{toml_text}");
    assert!(toml_text.contains("name = \"demo\""), "{toml_text}");

    // 5) 停用 → 端口释放
    let (code, body) = common::http(
        &addr,
        "POST",
        &format!("/api/mappings/{id}/enabled"),
        Some(r#"{"enabled":false}"#),
        Some(&auth),
    )
    .await;
    assert_eq!(code, 200, "{body}");
    assert!(body.contains("\"disabled\""), "{body}");
    wait_for("停用后端口释放", || async {
        TcpStream::connect(bound).await.is_err()
    })
    .await;

    // 6) 删除
    let (code, _) = common::http(&addr, "DELETE", &format!("/api/mappings/{id}"), None, Some(&auth)).await;
    assert_eq!(code, 200);
    let (_, body) = common::http(&addr, "GET", "/api/mappings", None, Some(&auth)).await;
    assert_eq!(body.trim(), "[]", "删除后列表应为空: {body}");

    // 7) 重新加载 / 踢 spoke 都不应报错
    let (code, resp) = common::http(&addr, "POST", "/api/reload", None, Some(&auth)).await;
    assert_eq!(code, 200, "{resp}");
    let (code, resp) = common::http(
        &addr,
        "POST",
        "/api/spokes/kick",
        Some(r#"{"id":"nobody"}"#),
        Some(&auth),
    )
    .await;
    assert_eq!(code, 200, "{resp}");

    // 8) 重复名字应 4xx（映射表层面的冲突被透传上来）
    let (code, _) = common::http(
        &addr,
        "POST",
        "/api/mappings",
        Some(r#"{"name":"dup","listen":"127.0.0.1:0"}"#),
        Some(&auth),
    )
    .await;
    assert_eq!(code, 200);
    let (code, body) = common::http(
        &addr,
        "POST",
        "/api/mappings",
        Some(r#"{"name":"dup","listen":"127.0.0.1:0"}"#),
        Some(&auth),
    )
    .await;
    assert!(
        (400..500).contains(&code),
        "重复服务名应返回 4xx，实际 {code}: {body}"
    );

    // 9) 收尾：宿主退出 → 面板进程消失
    host.shutdown().await;
    wait_for("面板停止服务", || {
        let addr = addr.clone();
        async move { TcpStream::connect(&addr).await.is_err() }
    })
    .await;

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mapping_created_through_control_plane_forwards_real_bytes() {
    let certs = CertFixture::both();
    let dir = temp_dir("flow");
    let engine_path = write_engine_toml(&dir, &certs, &certs.hub_cert, &certs.hub_key);

    let host_toml = format!(
        r#"
[engine]
config = "{engine}"

[panel]
enabled = false
"#,
        engine = engine_path.display(),
    );
    let host = Host::start(HostConfig::parse(&host_toml).unwrap(), None)
        .await
        .expect("宿主应能启动");
    wait_engine_ready(&host).await;
    let ctl = host.control();

    let echo = spawn_echo().await;
    let control_listen = ctl.status().await.unwrap().control_listen.unwrap();
    let spoke = start_spoke(&certs, control_listen, "demo", echo).await;
    wait_for("spoke 连上宿主的引擎", || {
        let spoke = spoke.clone();
        async move { spoke.status().await.unwrap().hub_connected == Some(true) }
    })
    .await;

    let view = ctl
        .upsert_mapping(MappingUpsert {
            id: None,
            name: "demo".into(),
            enabled: true,
            listen: Some("127.0.0.1:0".into()),
            spoke: None,
            local: None,
            note: String::new(),
        })
        .await
        .unwrap();
    assert_eq!(view.state, MappingState::Active, "{view:?}");
    assert_eq!(view.providers, vec!["e2e-spoke".to_string()]);
    let bound: SocketAddr = view.bound.clone().unwrap().parse().unwrap();

    let got = roundtrip(bound, b"through-the-host").await.expect("隧道应能转发");
    assert_eq!(got, b"through-the-host");

    host.shutdown().await;
    spoke.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}
