//! 隧道运行时插件（**进程内**）。
//!
//! # 为什么隧道放在进程内，而面板放在子进程
//!
//! 两者的失败模式不一样：
//!
//! * 隧道是**数据面**。它一旦重启，所有已建立的用户连接都会断。所以它应该在
//!   宿主里、和宿主同生共死，不引入额外的进程边界与帧编解码延迟。
//! * 面板是**控制面**。它崩了只是没人能改配置，隧道照常转发。把它放进独立
//!   子进程后，还可以单独升级 / 单独重启（`PluginHandle::upgrade`），
//!   不会碰数据面。
//!
//! 于是这个插件的职责很窄：**把 [`esc_engine::Engine`] 变成 Ctx 里的一個服务**，
//! 让宿主（以及宿主替子进程插件代理的 HostCall）能按契约调用它。
//!
//! ```text
//!   esc-host
//!     ├─ Ctx: ServiceKey<Arc<dyn TunnelControl>>  ← 本插件 mount 时 provide
//!     ├─ 进程内插件 esc.tunnel（本 crate）
//!     └─ 子进程插件 esc.panel  ── HostCall("esc.control", …) ──▶ 宿主服务 ──▶ 上面那个服务
//! ```
//!
//! `mount()` 是同步的（框架契约），而启动引擎是异步的。所以 mount 只做两件事：
//! 立刻把「句柄」注册进 Ctx（调用时会等引擎就绪），然后 spawn 一个任务真正启动
//! 引擎。这样面板不会因为「引擎还在绑定端口」而拿到 500。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use echo_context::{Ctx, ServiceKey};
use echo_plugin::{MountContext, Plugin, PluginManifest, PluginMountError, PluginMountResult};
use serde_json::Value;
use tokio::sync::watch;
use tracing::{info, warn};

use esc_control_api::{
    ControlError, EngineStatus, ExportedConfig, MappingUpsert, MappingView, Role, TunnelControl,
};
use esc_engine::{Engine, EngineConfig};

/// Ctx 里的服务键。宿主按这个键取引擎，插件按同一个键提供它。
pub static TUNNEL_CONTROL: ServiceKey<Arc<dyn TunnelControl>> = ServiceKey::new("esc.tunnel.control");

/// 引擎就绪的最长等待时间。
///
/// 绑一个端口 + 建 TLS 配置是毫秒级的事，所以 15s 已经非常宽松；超时说明引擎
/// 真的起不来（证书缺失、端口被占），这时应该让面板看到明确的错误而不是一直转圈。
const READY_TIMEOUT: Duration = Duration::from_secs(15);

/// 引擎句柄：注册进 Ctx 的那个东西。
///
/// 它在引擎就绪前就可以被调用——内部会等（最多 [`READY_TIMEOUT`]）。
#[derive(Clone)]
pub struct TunnelHandle {
    state: watch::Receiver<EngineState>,
}

#[derive(Clone, Default)]
struct EngineState {
    engine: Option<Arc<Engine>>,
    error: Option<String>,
}

impl TunnelHandle {
    async fn engine(&self) -> Result<Arc<Engine>, ControlError> {
        let mut rx = self.state.clone();
        if let Some(e) = rx.borrow().engine.clone() {
            return Ok(e);
        }
        if let Some(err) = rx.borrow().error.clone() {
            return Err(ControlError::unavailable(format!("引擎启动失败: {err}")));
        }
        let waited = tokio::time::timeout(READY_TIMEOUT, async {
            loop {
                if rx.changed().await.is_err() {
                    return Err(ControlError::unavailable("宿主已关闭引擎通道"));
                }
                let now = rx.borrow().clone();
                if let Some(e) = now.engine {
                    return Ok(e);
                }
                if let Some(err) = now.error {
                    return Err(ControlError::unavailable(format!("引擎启动失败: {err}")));
                }
            }
        })
        .await;
        match waited {
            Ok(r) => r,
            Err(_) => Err(ControlError::unavailable(format!(
                "引擎在 {}s 内没有就绪",
                READY_TIMEOUT.as_secs()
            ))),
        }
    }
}

#[async_trait::async_trait]
impl TunnelControl for TunnelHandle {
    async fn status(&self) -> Result<EngineStatus, ControlError> {
        self.engine().await?.status().await
    }
    async fn list_mappings(&self) -> Result<Vec<MappingView>, ControlError> {
        self.engine().await?.list_mappings().await
    }
    async fn upsert_mapping(&self, spec: MappingUpsert) -> Result<MappingView, ControlError> {
        self.engine().await?.upsert_mapping(spec).await
    }
    async fn delete_mapping(&self, id: &str) -> Result<(), ControlError> {
        self.engine().await?.delete_mapping(id).await
    }
    async fn set_mapping_enabled(&self, id: &str, enabled: bool) -> Result<MappingView, ControlError> {
        self.engine().await?.set_mapping_enabled(id, enabled).await
    }
    async fn reload(&self) -> Result<EngineStatus, ControlError> {
        self.engine().await?.reload().await
    }
    async fn kick_spokes(&self, id: Option<&str>) -> Result<usize, ControlError> {
        self.engine().await?.kick_spokes(id).await
    }
    async fn export_config(&self, role: Role) -> Result<ExportedConfig, ControlError> {
        self.engine().await?.export_config(role).await
    }
}

/// 进程内隧道插件。
pub struct TunnelPlugin {
    manifest: PluginManifest,
    tx: watch::Sender<EngineState>,
    engine: Arc<Mutex<Option<Arc<Engine>>>>,
}

impl Default for TunnelPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl TunnelPlugin {
    pub fn new() -> Self {
        let (tx, _rx) = watch::channel(EngineState::default());
        Self {
            manifest: PluginManifest::builtin(
                "esc.tunnel",
                "隧道运行时",
                env!("CARGO_PKG_VERSION"),
                "tunnel",
                "tunnel",
                "hub/spoke 反向隧道内核（进程内，数据面）",
            ),
            tx,
            engine: Arc::new(Mutex::new(None)),
        }
    }

    /// 供宿主在 mount 之后拿同一个句柄（宿主服务要用它做 HostCall 的目标）。
    pub fn handle(&self) -> TunnelHandle {
        TunnelHandle { state: self.tx.subscribe() }
    }
}

impl Plugin for TunnelPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn mount(&self, mctx: &MountContext) -> PluginMountResult {
        let ctx: Arc<Ctx> = mctx
            .context()
            .cloned()
            .ok_or_else(|| PluginMountError::Mount("宿主没有提供 Ctx".into()))?;

        // 配置来自宿主：`registry.set_config("esc.tunnel", json!({...}))`
        let cfg = EngineConfig::from_json(mctx.config())
            .map_err(|e| PluginMountError::Mount(format!("隧道配置无效: {}", e.message)))?;
        let role = cfg.role;

        // 先注册句柄（此时引擎可能还没就绪，调用方会等）。
        let handle: Arc<dyn TunnelControl> = Arc::new(self.handle());
        let disposer = ctx
            .provide(&TUNNEL_CONTROL, handle)
            .map_err(|e| PluginMountError::Mount(format!("无法注册隧道服务: {e}")))?;

        // 再异步启动引擎。
        let tx = self.tx.clone();
        let slot = Arc::clone(&self.engine);
        let rt = tokio::runtime::Handle::try_current()
            .map_err(|_| PluginMountError::Mount("tunnel plugin needs a tokio runtime".into()))?;
        rt.spawn(async move {
            match Engine::start(cfg).await {
                Ok(engine) => {
                    info!(role = %role, "tunnel engine ready");
                    *slot.lock().expect("engine slot poisoned") = Some(Arc::clone(&engine));
                    let _ = tx.send(EngineState { engine: Some(engine), error: None });
                }
                Err(e) => {
                    warn!(role = %role, error = %e.message, "tunnel engine failed to start");
                    let _ = tx.send(EngineState { engine: None, error: Some(e.message) });
                }
            }
        });

        info!(plugin = %mctx.plugin_id(), role = %role, "tunnel plugin mounted");
        Ok(vec![disposer])
    }

    fn unmount(&self) -> Result<(), PluginMountError> {
        let engine = self.engine.lock().expect("engine slot poisoned").take();
        if let Some(engine) = engine {
            // `unmount` 是同步的，而收尾要 await：交给运行时后台做完。
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                rt.spawn(async move {
                    engine.shutdown().await;
                    info!("tunnel engine shut down by unmount");
                });
            }
        }
        let _ = self.tx.send(EngineState::default());
        Ok(())
    }
}

/// 把 Ctx 里的隧道服务取出来（宿主在 mount 之后用）。
pub fn resolve(ctx: &Ctx) -> Result<Arc<dyn TunnelControl>, ControlError> {
    ctx.require(&TUNNEL_CONTROL)
        .map_err(|e| ControlError::unavailable(format!("隧道服务未注册: {e}")))
}

/// 从 JSON 配置里读一个可选字符串（宿主与插件共用的小工具）。
pub fn config_str<'a>(cfg: &'a Value, key: &str) -> Option<&'a str> {
    cfg.get(key).and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use echo_plugin::PluginRegistry;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handle_reports_startup_error_instead_of_hanging() {
        // 通道里放一个错误 → 调用应立刻返回「启动失败」而不是一直等。
        let (tx, rx) = watch::channel(EngineState {
            engine: None,
            error: Some("端口被占用".into()),
        });
        let handle = TunnelHandle { state: rx };
        let err = handle.status().await.unwrap_err();
        assert!(err.message.contains("端口被占用"), "{err:?}");
        drop(tx);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bad_config_fails_the_mount_closed() {
        // 配置本身不合法 → 挂载失败（fail-closed），而不是起一个空引擎。
        let ctx = Arc::new(Ctx::default());
        let registry = PluginRegistry::default();
        let plugin: Arc<dyn Plugin> = Arc::new(TunnelPlugin::new());
        registry.register(plugin).unwrap();
        registry.set_config("esc.tunnel", serde_json::json!({ "config": "/nonexistent/esc-engine.toml" }));
        let errors = registry.mount_all(&MountContext::new().with_ctx(ctx.clone()));
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].1.contains("无法读取配置"), "{errors:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mounted_plugin_serves_the_engine_through_the_contract() {
        // 真起一个 hub 引擎（TLS 证书现场生成），验证 Ctx 服务可用。
        let dir = std::env::temp_dir().join(format!("esc-tunnel-plugin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ca = esc_common::pki::Ca::generate("test-ca", 2).unwrap();
        let ca_path = esc_common::pki::write_ca_cert(&dir, "ca", &ca).unwrap();
        let hub = ca
            .issue_server("hub", &["127.0.0.1".to_string()], 1)
            .unwrap();
        let (cert, key) = esc_common::pki::write_pair(&dir, "hub", &hub).unwrap();

        let ctx = Arc::new(Ctx::default());
        let registry = PluginRegistry::default();
        let plugin: Arc<dyn Plugin> = Arc::new(TunnelPlugin::new());
        registry.register(plugin).unwrap();
        registry.set_config(
            "esc.tunnel",
            serde_json::json!({
                "role": "hub",
                "hub": { "listen": "127.0.0.1:0" },
                "tls": {
                    "ca_cert": ca_path.to_string_lossy(),
                    "cert": cert.to_string_lossy(),
                    "key": key.to_string_lossy()
                }
            }),
        );
        let errors = registry.mount_all(&MountContext::new().with_ctx(ctx.clone()));
        assert!(errors.is_empty(), "{errors:?}");

        let ctl = resolve(&ctx).unwrap();
        let status = ctl.status().await.unwrap();
        assert_eq!(status.role, Role::Hub);
        assert!(status.running);
        assert!(ctl.list_mappings().await.unwrap().is_empty());
        // 通过契约加一条映射（端口交给系统分配）
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
        assert!(view.bound.is_some(), "{view:?}");

        registry.unmount_all();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
