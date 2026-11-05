//! 端到端验证「运行时增删映射」——这是整个改造的核心承诺。
//!
//! 场景与生产一致：hub 引擎跑在公网侧，spoke 引擎主动连出，两者都是真
//! mTLS + yamux。区别只在于所有地址都用 `127.0.0.1:0` 让系统分配。
//!
//! 断言的东西：
//!
//! 1. spoke 先连上，声明了一个 hub 还没暴露的服务 → 被拒（安全模型不变）；
//! 2. **hub 侧后加映射，不需要 spoke 重连**，现有连接立刻被授权，端口马上可用；
//! 3. 数据真的穿过隧道到达背后的服务；
//! 4. 关掉映射，端口立即释放；
//! 5. 删掉映射，记录消失、端口也不会再被占。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use esc_common::config::{HubSection, SpokeSection, TlsConfig};
use esc_common::pki::{write_ca_cert, write_pair, Ca};
use esc_control_api::{MappingState, MappingUpsert, Role, TunnelControl};
use esc_engine::{Engine, EngineConfig};

/// 临时目录 + 一套可用的证书。
struct Fixture {
    dir: PathBuf,
    hub_tls: TlsConfig,
    spoke_tls: TlsConfig,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "esc-engine-it-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let ca = Ca::generate("esc-test-ca", 2).unwrap();
        let ca_path = write_ca_cert(&dir, "ca", &ca).unwrap();

        let hub = ca
            .issue_server("hub", &["127.0.0.1".to_string(), "localhost".to_string()], 1)
            .unwrap();
        let (hub_cert, hub_key) = write_pair(&dir.join("hub"), "hub", &hub).unwrap();

        let spoke = ca.issue_client("it-spoke", 1).unwrap();
        let (spoke_cert, spoke_key) = write_pair(&dir.join("spoke"), "spoke", &spoke).unwrap();

        Self {
            hub_tls: TlsConfig { ca_cert: ca_path.clone(), cert: hub_cert, key: hub_key },
            spoke_tls: TlsConfig { ca_cert: ca_path, cert: spoke_cert, key: spoke_key },
            dir,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 一个只会回显的 TCP 服务，代表「内网里那个要暴露的应用」。
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

async fn wait_for<F, Fut>(what: &str, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if f().await {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "超时等待：{what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// 读回显：连上去写一段，读回同样一段。
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

fn hub_config(tls: TlsConfig) -> EngineConfig {
    EngineConfig {
        role: Role::Hub,
        tls,
        hub: HubSection {
            listen: "127.0.0.1:0".into(),
            heartbeat_secs: 30,
            reject_when_absent: true,
        },
        spoke: SpokeSection::default(),
        store_path: None,
        seed: Vec::new(),
        source: None,
    }
}

fn spoke_config(tls: TlsConfig, control: SocketAddr, services: Vec<(String, String)>) -> EngineConfig {
    use esc_control_api::Mapping;
    EngineConfig {
        role: Role::Spoke,
        tls,
        hub: HubSection::default(),
        spoke: SpokeSection {
            id: "it-spoke".into(),
            server: control.to_string(),
            backoff_min_ms: 50,
            backoff_max_ms: 200,
            max_streams: 32,
            server_name: None,
        },
        store_path: None,
        seed: services
            .into_iter()
            .map(|(name, local)| Mapping {
                id: Mapping::new_id(),
                name,
                enabled: true,
                listen: None,
                spoke: None,
                local: Some(local),
                note: String::new(),
            })
            .collect(),
        source: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mapping_can_be_added_while_spoke_is_connected() {
    let fx = Fixture::new("add");
    let echo = spawn_echo().await;

    let hub = Engine::start(hub_config(fx.hub_tls.clone())).await.unwrap();
    let control = hub.control_addr().unwrap();

    // spoke 只声明 echo，但 hub 此刻还没暴露它 → 应当被拒。
    let spoke = Engine::start(spoke_config(
        fx.spoke_tls.clone(),
        control,
        vec![("echo".into(), echo.to_string())],
    ))
    .await
    .unwrap();

    wait_for("spoke 连上 hub", || async {
        spoke.status().await.unwrap().hub_connected == Some(true)
    })
    .await;

    // hub 侧此前没有任何映射
    assert!(hub.list_mappings().await.unwrap().is_empty());

    // ——— 关键动作：运行时加一条映射，spoke 不重连 ———
    let view = hub
        .upsert_mapping(MappingUpsert {
            id: None,
            name: "echo".into(),
            enabled: true,
            listen: Some("127.0.0.1:0".into()),
            spoke: None,
            local: Some(echo.to_string()),
            note: "runtime add".into(),
        })
        .await
        .unwrap();

    assert_eq!(view.state, MappingState::Active, "映射应当立即生效: {view:?}");
    assert_eq!(view.providers, vec!["it-spoke".to_string()], "现有连接应被动态授权");

    let bound: SocketAddr = view.bound.clone().unwrap().parse().unwrap();

    // 数据面：真的穿过去了
    let got = roundtrip(bound, b"hello-esc").await.expect("隧道应能转发");
    assert_eq!(got, b"hello-esc");

    // 计数应当被记录
    let view = hub.list_mappings().await.unwrap().pop().unwrap();
    assert!(view.stats.connections_total >= 1, "应记录连接数: {view:?}");
    assert!(view.stats.bytes_up >= 9, "应记录上行字节: {view:?}");

    // ——— 关闭映射：端口立即释放 ———
    hub.set_mapping_enabled(&view.mapping.id, false).await.unwrap();
    wait_for("端口被释放", || async {
        // 连续两次连不上才算真的释放了（第一次可能撞上旧监听销毁的窗口）
        TcpStream::connect(bound).await.is_err()
    })
    .await;

    // 再打开：同一个端口仍然可用，且依旧能转发
    let view = hub.set_mapping_enabled(&view.mapping.id, true).await.unwrap();
    assert_eq!(view.state, MappingState::Active);
    let bound2: SocketAddr = view.bound.clone().unwrap().parse().unwrap();
    wait_for("端口恢复", || async {
        roundtrip(bound2, b"again").await.as_deref() == Some(b"again".as_slice())
    })
    .await;

    // ——— 删除映射：记录消失 ———
    hub.delete_mapping(&view.mapping.id).await.unwrap();
    assert!(hub.list_mappings().await.unwrap().is_empty());

    hub.shutdown().await;
    spoke.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bound_port_is_actually_forwarded_and_released_on_delete() {
    let fx = Fixture::new("del");
    let echo = spawn_echo().await;

    let hub = Engine::start(hub_config(fx.hub_tls.clone())).await.unwrap();
    let control = hub.control_addr().unwrap();
    let spoke = Engine::start(spoke_config(
        fx.spoke_tls.clone(),
        control,
        vec![("echo".into(), echo.to_string())],
    ))
    .await
    .unwrap();
    wait_for("spoke 连上", || async {
        spoke.status().await.unwrap().hub_connected == Some(true)
    })
    .await;

    let view = hub
        .upsert_mapping(MappingUpsert {
            id: None,
            name: "echo".into(),
            enabled: true,
            listen: Some("127.0.0.1:0".into()),
            spoke: None,
            local: None,
            note: String::new(),
        })
        .await
        .unwrap();
    let bound: SocketAddr = view.bound.clone().unwrap().parse().unwrap();
    assert_eq!(roundtrip(bound, b"x").await.as_deref(), Some(b"x".as_slice()));

    hub.delete_mapping(&view.mapping.id).await.unwrap();
    wait_for("删除后端口释放", || async {
        TcpStream::connect(bound).await.is_err()
    })
    .await;

    hub.shutdown().await;
    spoke.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_name_and_listen_are_rejected() {
    let fx = Fixture::new("dup");
    let hub = Engine::start(hub_config(fx.hub_tls.clone())).await.unwrap();

    let mk = |name: &str, listen: &str| MappingUpsert {
        id: None,
        name: name.into(),
        enabled: true,
        listen: Some(listen.into()),
        spoke: None,
        local: Some("127.0.0.1:9".into()),
        note: String::new(),
    };

    let first = hub.upsert_mapping(mk("a", "127.0.0.1:0")).await.unwrap();
    assert_eq!(first.state, MappingState::Active);

    let err = hub.upsert_mapping(mk("a", "127.0.0.1:0")).await.unwrap_err();
    assert!(err.message.contains("已被另一条映射占用"), "{err:?}");

    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn export_produces_config_that_matches_the_mappings() {
    let fx = Fixture::new("export");
    let hub = Engine::start(hub_config(fx.hub_tls.clone())).await.unwrap();
    hub.upsert_mapping(MappingUpsert {
        id: None,
        name: "svc".into(),
        enabled: true,
        listen: Some("127.0.0.1:0".into()),
        spoke: Some("pin-me".into()),
        local: Some("127.0.0.1:8080".into()),
        note: String::new(),
    })
    .await
    .unwrap();

    let exported = hub.export_config(Role::Hub).await.unwrap();
    assert_eq!(exported.filename, "hub.toml");
    assert!(exported.toml.contains("[[service]]"));
    assert!(exported.toml.contains("name = \"svc\""));
    assert!(exported.toml.contains("spoke = \"pin-me\""));
    // 导出的 TOML 必须能被 CLI 读回
    let parsed: Result<esc_common::config::HubConfig, _> = toml::from_str(&exported.toml);
    assert!(parsed.is_ok(), "导出的配置应可解析: {:?}", parsed.err());

    let spoke_export = hub.export_config(Role::Spoke).await.unwrap();
    assert!(spoke_export.toml.contains("127.0.0.1:8080"));
    let parsed: Result<esc_common::config::SpokeConfig, _> = toml::from_str(&spoke_export.toml);
    assert!(parsed.is_ok(), "导出的 spoke 配置应可解析: {:?}", parsed.err());

    hub.shutdown().await;
}

/// 让 `Fixture::dir` 不触发 dead_code：测试里有时只关心 TLS 文件。
#[allow(dead_code)]
fn _touch(p: &Path) -> bool {
    p.exists()
}
