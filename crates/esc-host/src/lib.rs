//! 宿主（组合根）的组装逻辑。
//!
//! 这个 crate 只做四件事，**不认识隧道协议，也不认识 HTTP**：
//!
//! 1. 建 [`Ctx`]（服务定位器）；
//! 2. 挂载进程内插件 `esc.tunnel` → 它把 `Arc<dyn TunnelControl>` 放进 `Ctx`；
//! 3. 注册宿主服务 `esc.control` → 把子进程插件的 `HostCall` 转成对上面那个
//!    服务的 `dispatch(ControlRequest)` 调用；
//! 4. 用 `PluginSupervisor` 拉起子进程插件 `esc.panel`，退出时优雅排空。
//!
//! ```text
//!   浏览器 ──▶ esc-panel（子进程，axum）
//!                 │ HostCall("esc.control", "mapping.upsert", {...})
//!                 ▼
//!              esc-host ──▶ Ctx 里的 Arc<dyn TunnelControl> ──▶ esc-engine
//!                 ▲
//!                 └─ 进程内插件 esc.tunnel 在 mount 时注册这个服务
//! ```
//!
//! 想只跑隧道不要面板：`[panel] enabled = false`。
//! 想换面板实现：换 `command`。想换隧道实现：换掉 `esc.tunnel` 插件的实现，
//! 只要它仍然注册 `esc.tunnel.control` 这个服务键。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use echo_context::Ctx;
use echo_plugin::{MountContext, Plugin, PluginRegistry};
use echo_plugin_host::api::InvokeContext;
use echo_plugin_host::host_service::HostService;
use echo_plugin_host::stdio::StdioTransport;
use echo_plugin_host::supervisor::{PluginHandle, PluginSupervisor};
use echo_plugin_host::transport::PluginSpec;
use serde::Deserialize;
use serde_json::Value;
use tracing::{info, warn};

use esc_control_api::{
    dispatch, ControlError, ControlRequest, ControlResponse, TunnelControl, CONTROL_SERVICE,
};
use esc_plugin_tunnel::TunnelPlugin;

/// 隧道插件在注册表里的 id。
pub const TUNNEL_PLUGIN_ID: &str = "esc.tunnel";
/// 面板插件的 id（与面板 `Hello` 里的 plugin_id 一致）。
pub const PANEL_PLUGIN_ID: &str = "esc.panel";

// ─────────────────────────────────────────────────────────────────────────────
// 配置
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct HostConfig {
    /// 隧道插件的配置：要么 `{ config = "<toml 路径>" }`，要么直接内联引擎配置。
    #[serde(default = "empty_table")]
    pub engine: toml::Value,
    #[serde(default)]
    pub panel: PanelSection,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PanelSection {
    /// 关掉它就只剩隧道，没有任何管理面。
    #[serde(default = "yes")]
    pub enabled: bool,
    /// 面板插件可执行文件。相对路径按宿主可执行文件所在目录解析。
    #[serde(default = "default_panel_command")]
    pub command: String,
    /// 传给面板的配置，原样透传（面板自己解释 listen / auth / title）。
    #[serde(default = "empty_table")]
    pub config: toml::Value,
}

fn yes() -> bool {
    true
}

/// `toml::Value` 没有 `Default`，但空表才是「本段没写」的正确语义。
fn empty_table() -> toml::Value {
    toml::Value::Table(toml::map::Map::new())
}

fn default_panel_command() -> String {
    "esc-plugin-panel".to_string()
}

impl Default for PanelSection {
    fn default() -> Self {
        Self {
            enabled: true,
            command: default_panel_command(),
            config: empty_table(),
        }
    }
}

impl HostConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("无法读取宿主配置 {}: {e}", path.display()))?;
        Self::parse(&raw)
    }

    pub fn parse(raw: &str) -> anyhow::Result<Self> {
        toml::from_str(raw).map_err(|e| anyhow::anyhow!("无法解析宿主配置: {e}"))
    }

    /// 隧道插件配置转成 JSON（框架的插件配置一律是 JSON）。
    fn engine_json(&self) -> Value {
        toml_to_json(&self.engine)
    }

    fn panel_json(&self) -> Value {
        toml_to_json(&self.panel.config)
    }
}

fn toml_to_json(v: &toml::Value) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

// ─────────────────────────────────────────────────────────────────────────────
// 宿主服务：把 HostCall 变成对隧道服务的 dispatch
// ─────────────────────────────────────────────────────────────────────────────

/// `HostCall("esc.control", <method>, <params>)` 的宿主端实现。
///
/// 框架的 `HostService::call` 是**同步**的，而引擎 API 是异步的。这里是唯一
/// 需要桥接的地方：`block_in_place` 把当前 worker 让出来，再用 `block_on` 等
/// 引擎返回。要求 multi-thread 运行时——`#[tokio::main]` 默认就是。
pub struct EngineHostService {
    ctl: Arc<dyn TunnelControl>,
}

impl EngineHostService {
    pub fn new(ctl: Arc<dyn TunnelControl>) -> Self {
        Self { ctl }
    }
}

impl HostService for EngineHostService {
    fn call(&self, method: &str, payload: Value) -> Result<Value, (String, String)> {
        let ctl = Arc::clone(&self.ctl);
        let req = ControlRequest { method: method.to_string(), params: payload };
        let resp: ControlResponse = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(dispatch(ctl.as_ref(), req))
        });
        if resp.ok {
            Ok(resp.data)
        } else {
            let err = resp.error.unwrap_or_else(|| ControlError::other("控制调用失败"));
            Err((format!("{:?}", err.kind).to_lowercase(), err.message))
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Host
// ─────────────────────────────────────────────────────────────────────────────

/// 已启动的宿主。
pub struct Host {
    ctx: Arc<Ctx>,
    registry: PluginRegistry,
    panel: Option<PluginHandle>,
    control: Arc<dyn TunnelControl>,
}

impl Host {
    /// 组装并启动：挂进程内隧道插件 → 注册宿主服务 → 起面板子进程插件。
    ///
    /// `panel_program` 为空时用配置里的 `command`（相对路径按宿主可执行文件目录解析）。
    pub async fn start(cfg: HostConfig, panel_program: Option<PathBuf>) -> anyhow::Result<Self> {
        let ctx = Arc::new(Ctx::default());
        let registry = PluginRegistry::default();
        let tunnel: Arc<dyn Plugin> = Arc::new(TunnelPlugin::new());
        registry
            .register(tunnel)
            .map_err(|e| anyhow::anyhow!("无法注册隧道插件: {e}"))?;
        registry.set_config(TUNNEL_PLUGIN_ID, cfg.engine_json());
        let errors = registry.mount_all(&MountContext::new().with_ctx(Arc::clone(&ctx)));
        if let Some((id, why)) = errors.first() {
            anyhow::bail!("插件 {id} 挂载失败: {why}");
        }
        info!("tunnel plugin mounted");

        let control = esc_plugin_tunnel::resolve(&ctx)
            .map_err(|e| anyhow::anyhow!("隧道服务不可用: {}", e.message))?;

        let supervisor = PluginSupervisor::new();
        supervisor
            .host_services()
            .register(CONTROL_SERVICE, Arc::new(EngineHostService::new(Arc::clone(&control))));

        let mut panel = None;
        if cfg.panel.enabled {
            let program = panel_program.unwrap_or_else(|| resolve_program(&cfg.panel.command));
            let spec = PluginSpec {
                plugin_id: PANEL_PLUGIN_ID.to_string(),
                config: cfg.panel_json(),
            };
            match supervisor.start(spec, Box::new(StdioTransport::new(program.clone()))).await {
                Ok(handle) => {
                    info!(program = %program.display(), "panel plugin ready");
                    if let Some(url) = probe_panel_url(&handle).await {
                        info!(url = %url, "web panel is up");
                    }
                    panel = Some(handle);
                }
                Err(e) => {
                    // 面板起不来不应该让隧道跟着停——这正是拆成两个插件的意义。
                    warn!(error = %e, "panel plugin failed to start; tunnel keeps running");
                }
            }
        } else {
            info!("panel disabled; tunnel-only mode");
        }

        Ok(Self { ctx, registry, panel, control })
    }

    /// 控制面句柄（测试与嵌入式用法）。
    pub fn control(&self) -> Arc<dyn TunnelControl> {
        Arc::clone(&self.control)
    }

    pub fn panel(&self) -> Option<&PluginHandle> {
        self.panel.as_ref()
    }

    /// 优雅收尾：先控制面（面板），后数据面（隧道）。
    pub async fn shutdown(mut self) {
        if let Some(handle) = self.panel.take() {
            if let Err(e) = handle.drain(Duration::from_secs(5)).await {
                warn!(error = %e, "panel drain reported an error");
            }
            info!(state = handle.state().as_str(), "panel plugin stopped");
        }
        self.registry.unmount_all();
        // 隧道插件的 unmount 已经把引擎 shutdown 了；这里只保留 Ctx 供日志检查。
        let keys = self.ctx.keys();
        info!(keys = ?keys, "host stopped");
    }
}

/// 相对路径按宿主可执行文件所在目录解析，这样 `cargo run` 时不用配绝对路径。
fn resolve_program(command: &str) -> PathBuf {
    let p = PathBuf::from(command);
    if p.is_absolute() {
        return p;
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join(command)))
        .filter(|candidate| candidate.is_file())
        .unwrap_or(p)
}

/// 问面板「你在哪个地址上服务」，最多等 5 秒。
async fn probe_panel_url(handle: &PluginHandle) -> Option<String> {
    for _ in 0..25 {
        let outcome = handle
            .invoke(PANEL_PLUGIN_ID, InvokeContext::default(), Value::Null)
            .await;
        if let echo_plugin_host::api::InvokeOutcome::Ok { result, .. } = outcome {
            if result.get("serving").and_then(Value::as_bool) == Some(true) {
                let listen = result.get("listen").and_then(Value::as_str)?.to_string();
                let auth = result.get("auth").and_then(Value::as_bool).unwrap_or(false);
                return Some(if auth {
                    format!("http://{listen}/（需要口令）")
                } else {
                    format!("http://{listen}/")
                });
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    None
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => return std::future::pending().await,
        };
        let mut int = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(_) => return std::future::pending().await,
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// 读配置 → 起宿主 → 等信号 → 优雅退出（二进制入口用它）。
pub async fn run_until_signal(cfg_path: &Path, panel_program: Option<PathBuf>) -> anyhow::Result<()> {
    let cfg = HostConfig::load(cfg_path)?;
    info!(config = %cfg_path.display(), panel = cfg.panel.enabled, "esc host starting");
    let host = Host::start(cfg, panel_program).await?;
    wait_for_signal().await;
    info!("shutdown signal received");
    host.shutdown().await;
    Ok(())
}
