//! 宿主（组合根）的组装逻辑。
//!
//! 这个 crate 只做这几件事，**不认识隧道协议，也不认识 HTTP**：
//!
//! 1. 建 [`Ctx`]（服务定位器）；
//! 2. 挂载进程内插件 `esc.tunnel` → 它把 `Arc<dyn TunnelControl>` 放进 `Ctx`；
//! 3. 按需挂载进程内插件 `esc.edge-router` → 按域名前缀把本机端口分开
//!    （`[edge_router] enabled = true` 才挂）；
//! 4. 注册宿主服务 `esc.control` → 把子进程插件的 `HostCall` 转成对上面那个
//!    服务的 `dispatch(ControlRequest)` 调用；
//! 5. 用 `PluginSupervisor` 拉起子进程插件 `esc.tunnel-panel`，退出时优雅排空。
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
use echo_plugin_host::supervisor::{PluginHandle, PluginState, PluginSupervisor};
use echo_plugin_host::transport::PluginSpec;
use serde::Deserialize;
use serde_json::Value;
use tracing::{error, info, warn};

use esc_control_api::{
    dispatch, dispatch_edge, ControlError, ControlRequest, ControlResponse, EdgeRouteControl,
    TunnelControl, CONTROL_SERVICE, EDGE_ROUTE_SERVICE,
};
use esc_plugin_edge_router::EdgeRouterPlugin;
use esc_plugin_tunnel::TunnelPlugin;

/// 隧道插件在注册表里的 id。
pub const TUNNEL_PLUGIN_ID: &str = "esc.tunnel";
/// 边缘路由插件的 id。默认**不挂载**（要绑端口，必须显式开启）。
pub const EDGE_ROUTER_PLUGIN_ID: &str = esc_plugin_edge_router::PLUGIN_ID;
/// 隧道面板插件的 id（与面板 `Hello` 里的 plugin_id 一致）。
///
/// 名字对齐 crate：`esc-plugin-tunnel-panel` ↔ `esc.tunnel-panel`。
pub const TUNNEL_PANEL_PLUGIN_ID: &str = "esc.tunnel-panel";

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
    /// 边缘路由（域名前缀 → 本机端口）。默认关闭：它要监听端口，不能"默认打开"。
    #[serde(default)]
    pub edge_router: EdgeRouterSection,
}

/// `[edge_router]` 段。
///
/// 这里只保留宿主真正需要的两项（开关 + 透传给插件的配置），路由表的具体
/// schema 由插件自己定义 —— 宿主不认识 `routes` / `default_port`，加字段
/// 不用改这里。
#[derive(Debug, Clone, Deserialize)]
pub struct EdgeRouterSection {
    /// 关掉它（默认）就完全不挂载该插件，也不占用任何端口。
    #[serde(default)]
    pub enabled: bool,
    /// 原样透传给插件（`listen` / `routes` / `default_port` / `timeout_ms`）。
    #[serde(default = "empty_table")]
    pub config: toml::Value,
}

// `toml::Value` 没有 `Default`，所以手写一个（空表 = "本段没写"）。
impl Default for EdgeRouterSection {
    fn default() -> Self {
        Self { enabled: false, config: empty_table() }
    }
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
    "esc-plugin-tunnel-panel".to_string()
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

    fn edge_router_json(&self) -> Value {
        toml_to_json(&self.edge_router.config)
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
            // 用契约里的 `as_code`（不是 `{:?}`）：面板按同一份码表解析。
            Err((err.kind.as_code().to_string(), err.message))
        }
    }
}

/// `HostCall("esc.edge", <method>, <params>)` 的宿主端实现。
///
/// 与 [`EngineHostService`] 同一套路：同步 `HostService::call` → `block_in_place`
/// → 异步 `dispatch_edge`。分成两个服务而不是合成一个，是因为它们背后是
/// **两个不同插件**：没启用边缘路由时这个服务根本不该存在（注册了也没有
/// 实现可转），面板据此就能判断"这台机器有没有路由能力"。
pub struct EdgeHostService {
    ctl: Arc<dyn EdgeRouteControl>,
}

impl EdgeHostService {
    pub fn new(ctl: Arc<dyn EdgeRouteControl>) -> Self {
        Self { ctl }
    }
}

impl HostService for EdgeHostService {
    fn call(&self, method: &str, payload: Value) -> Result<Value, (String, String)> {
        let ctl = Arc::clone(&self.ctl);
        let req = ControlRequest { method: method.to_string(), params: payload };
        let resp: ControlResponse = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(dispatch_edge(ctl.as_ref(), req))
        });
        if resp.ok {
            Ok(resp.data)
        } else {
            let err = resp.error.unwrap_or_else(|| ControlError::other("边缘路由控制失败"));
            Err((err.kind.as_code().to_string(), err.message))
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
    edge_router: Option<Arc<dyn EdgeRouteControl>>,
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

        // 边缘路由按需注册：没 enabled 就完全不存在（不注册 → 不挂载 → 不占端口）。
        if cfg.edge_router.enabled {
            let router: Arc<dyn Plugin> = Arc::new(EdgeRouterPlugin::new());
            registry
                .register(router)
                .map_err(|e| anyhow::anyhow!("无法注册边缘路由插件: {e}"))?;
            registry.set_config(EDGE_ROUTER_PLUGIN_ID, cfg.edge_router_json());
        }

        let errors = registry.mount_all(&MountContext::new().with_ctx(Arc::clone(&ctx)));
        if let Some((id, why)) = errors.first() {
            anyhow::bail!("插件 {id} 挂载失败: {why}");
        }
        info!("tunnel plugin mounted");

        let control = esc_plugin_tunnel::resolve(&ctx)
            .map_err(|e| anyhow::anyhow!("隧道服务不可用: {}", e.message))?;

        let edge_router = if cfg.edge_router.enabled {
            let ctl = esc_plugin_edge_router::resolve(&ctx)
                .map_err(|e| anyhow::anyhow!("边缘路由服务不可用: {}", e.message))?;
            match ctl.status().await {
                Ok(st) => info!(
                    listen = %st.listens.join(","),
                    routes = st.routes.len(),
                    default_port = ?st.default_port,
                    "edge router ready"
                ),
                Err(e) => warn!("边缘路由已挂载但状态不可读: {}", e.message),
            }
            Some(ctl)
        } else {
            None
        };

        let supervisor = PluginSupervisor::new();
        supervisor
            .host_services()
            .register(CONTROL_SERVICE, Arc::new(EngineHostService::new(Arc::clone(&control))));

        // 边缘路由控制面：只有启用了插件才注册 —— 面板据此判断能力是否存在。
        if let Some(edge) = &edge_router {
            supervisor.host_services().register(
                EDGE_ROUTE_SERVICE,
                Arc::new(EdgeHostService::new(Arc::clone(edge))),
            );
            info!(service = EDGE_ROUTE_SERVICE, "edge control service registered");
        }

        let mut panel = None;
        if cfg.panel.enabled {
            let program = panel_program.unwrap_or_else(|| resolve_program(&cfg.panel.command));
            let spec = PluginSpec {
                plugin_id: TUNNEL_PANEL_PLUGIN_ID.to_string(),
                config: cfg.panel_json(),
            };
            match supervisor.start(spec, Box::new(StdioTransport::new(program.clone()))).await {
                Ok(handle) => {
                    info!(program = %program.display(), "panel plugin ready");
                    if let Some(url) = probe_panel_url(&handle).await {
                        info!(url = %url, "web panel is up");
                    }
                    watch_panel(handle.clone());
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

        Ok(Self { ctx, registry, panel, control, edge_router })
    }

    /// 控制面句柄（测试与嵌入式用法）。
    pub fn control(&self) -> Arc<dyn TunnelControl> {
        Arc::clone(&self.control)
    }

    pub fn panel(&self) -> Option<&PluginHandle> {
        self.panel.as_ref()
    }

    /// 边缘路由控制面（未启用 `[edge_router]` 时为 `None`）。
    ///
    /// 暴露出来的动机：面板将来要显示"哪个域名去了哪个端口"，它需要这个句柄。
    /// 现在没有调用方，但它是**契约的一部分**（服务已注册在 Ctx 里），所以
    /// 保留并 `#[allow(dead_code)]`，而不是等到要用时再改公共 API。
    pub fn edge_router(&self) -> Option<Arc<dyn EdgeRouteControl>> {
        self.edge_router.as_ref().map(Arc::clone)
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

/// 盯着面板的状态。
///
/// supervisor 对崩溃的插件最多重启 5 次，之后把它钉在 `Failed` 并且**不再
/// 尝试**。这对隧道毫无影响（数据面照跑），但对运维是真问题：面板打不开，
/// 而日志里只有 supervisor 的普通 warn，很容易几个月没人发现。
///
/// 所以这里在状态变成 `Failed` 时打一条 ERROR，说清楚「面板已永久停摆、
/// 隧道仍在运行、怎么恢复」。
fn watch_panel(handle: PluginHandle) {
    tokio::spawn(async move {
        let mut reported = false;
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            match handle.state() {
                PluginState::Failed(why) if !reported => {
                    reported = true;
                    error!(
                        restarts = handle.restarts(),
                        reason = %why,
                        "面板插件已停摆（重启预算耗尽）——隧道仍在正常转发，\
                         但 Web 面板不再可用。排查原因后重启本服务即可恢复：\
                         systemctl restart esc-host。原始原因见上方 supervisor 日志。"
                    );
                }
                // 面板被正常停掉（宿主收尾流程）→ 哨兵使命结束。
                PluginState::Stopped | PluginState::Draining => return,
                _ => {}
            }
        }
    });
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
            .invoke(TUNNEL_PANEL_PLUGIN_ID, InvokeContext::default(), Value::Null)
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

// ─────────────────────────────────────────────────────────────────────────────
// 命令行
// ─────────────────────────────────────────────────────────────────────────────

/// 默认配置路径。
pub const DEFAULT_CONFIG: &str = "/etc/esc/host.toml";

/// 二进制入口的三种结局。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    Run(std::path::PathBuf),
    Help,
    Version,
}

/// 帮助文本。
pub fn usage() -> String {
    format!(
        "esc-host {version} —— 隧道宿主（进程内隧道插件 + 子进程 Web 面板插件）

用法:
    esc-host [-c|--config <路径>]

选项:
    -c, --config <路径>   宿主配置，默认 {DEFAULT_CONFIG}
                          也可用环境变量 ESC_HOST_CONFIG
    -h, --help            显示本帮助后退出
    -V, --version         显示版本后退出

配置文件示例见仓库 examples/host.toml。",
        version = env!("CARGO_PKG_VERSION"),
    )
}

/// 解析命令行参数。
///
/// 单独抽出来的理由：`esc-host --help` 曾经会被当成「没给参数」而直接用默认
/// 配置启动服务——在生产上是很难受的误操作（会去抢 4433/2222 端口）。所以
/// **任何不认识的参数都必须报错退出**，而不是静默忽略。
pub fn parse_args<I, S>(args: I) -> Result<Invocation, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut config: Option<std::path::PathBuf> = None;
    let mut it = args.into_iter().map(Into::into).peekable();

    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Invocation::Help),
            "-V" | "--version" => return Ok(Invocation::Version),
            "-c" | "--config" => {
                let value = it.next().ok_or_else(|| format!("{arg} 需要一个路径参数"))?;
                config = Some(std::path::PathBuf::from(value));
            }
            other if other.starts_with("--config=") => {
                let value = other.trim_start_matches("--config=");
                if value.is_empty() {
                    return Err("--config= 后面需要一个路径".to_string());
                }
                config = Some(std::path::PathBuf::from(value));
            }
            other if other.starts_with('-') => {
                return Err(format!("不认识的选项 {other:?}\n\n{}", usage()));
            }
            // 位置参数也拒绝：这个程序没有位置参数，多给了说明是误用。
            other => {
                return Err(format!("多余的参数 {other:?}\n\n{}", usage()));
            }
        }
    }

    Ok(Invocation::Run(match config {
        Some(path) => path,
        None => std::path::PathBuf::from(
            std::env::var("ESC_HOST_CONFIG").unwrap_or_else(|_| DEFAULT_CONFIG.to_string()),
        ),
    }))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Invocation {
        parse_args(args.iter().copied()).expect("应当解析成功")
    }

    #[test]
    fn no_args_uses_default_path() {
        std::env::remove_var("ESC_HOST_CONFIG");
        assert_eq!(run(&[]), Invocation::Run(DEFAULT_CONFIG.into()));
    }

    #[test]
    fn help_and_version_short_circuit() {
        assert_eq!(run(&["--help"]), Invocation::Help);
        assert_eq!(run(&["-h"]), Invocation::Help);
        assert_eq!(run(&["--version"]), Invocation::Version);
        assert_eq!(run(&["-V"]), Invocation::Version);
        // 帮助优先：即使后面还有别的参数也不去解析它们
        assert_eq!(run(&["-h", "-c", "/nope"]), Invocation::Help);
    }

    #[test]
    fn config_forms_are_equivalent() {
        assert_eq!(run(&["-c", "/tmp/a.toml"]), Invocation::Run("/tmp/a.toml".into()));
        assert_eq!(run(&["--config", "/tmp/a.toml"]), Invocation::Run("/tmp/a.toml".into()));
        assert_eq!(run(&["--config=/tmp/a.toml"]), Invocation::Run("/tmp/a.toml".into()));
    }

    #[test]
    fn unknown_args_are_rejected_instead_of_being_ignored() {
        // 这条是关键：不然 `esc-host --hlep` 会用默认配置把服务拉起来。
        assert!(parse_args(["--hlep"]).is_err());
        assert!(parse_args(["-x"]).is_err());
        assert!(parse_args(["/tmp/whatever"]).is_err());
        assert!(parse_args(["-c"]).is_err(), "-c 后面缺路径应报错");
        assert!(parse_args(["--config="]).is_err());
    }

    #[test]
    fn error_message_points_at_the_usage() {
        let err = parse_args(["--nope"]).unwrap_err();
        assert!(err.contains("用法"), "错误信息应带用法：{err}");
    }

    // ── [edge_router] 段 ────────────────────────────────────────────────

    #[test]
    fn edge_router_is_off_unless_explicitly_enabled() {
        // 默认关闭是刻意的：这个插件要 bind 端口（可能是 :80），不能"默认打开"。
        let cfg = HostConfig::parse("[panel]\nenabled = false\n").unwrap();
        assert!(!cfg.edge_router.enabled);
        // 关闭时配置原样是空表 —— 宿主不认识路由表 schema，不做任何猜测。
        assert_eq!(cfg.edge_router_json(), Value::Object(Default::default()));
    }

    // ── 边缘路由的跨进程控制面 ──────────────────────────────────────────
    //
    // 验证链路：面板 HostCall("esc.edge", method, params)
    //   → EdgeHostService::call → dispatch_edge → EdgeRouteControl → 插件
    //
    // 这里直接调 EdgeHostService::call（supervisor 那层是框架的，已有自己的
    // 测试）；整个调用是同步签名，与真实 HostCall 一致。

    /// 挂一个边缘路由插件（监听 :0），返回控制面句柄。
    ///
    /// 注册表要跟着返回：它持有 mount() 的 `Disposer`，Drop 时会撤销服务
    /// （框架语义：注册即副作用、必须可逆）。只返回句柄的话，注册表一
    /// 出作用域服务就消失了。
    async fn mount_edge_router() -> (
        Arc<dyn EdgeRouteControl>,
        Arc<Ctx>,
        PluginRegistry,
    ) {
        let ctx = Arc::new(Ctx::default());
        let registry = PluginRegistry::default();
        let plugin: Arc<dyn Plugin> = Arc::new(EdgeRouterPlugin::new());
        registry.register(plugin).unwrap();
        registry.set_config(
            EDGE_ROUTER_PLUGIN_ID,
            serde_json::json!({
                "listen": ["127.0.0.1:0"],
                "routes": { "cad": 2228 },
            }),
        );
        let errors = registry.mount_all(&MountContext::new().with_ctx(Arc::clone(&ctx)));
        assert!(errors.is_empty(), "{errors:?}");
        let ctl = esc_plugin_edge_router::resolve(&ctx).expect("控制面应已注册");
        (ctl, ctx, registry)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn edge_control_service_exposes_status_and_mutations() {
        use esc_control_api::method;
        let (ctl, _ctx, _registry) = mount_edge_router().await;
        let svc = EdgeHostService::new(ctl);

        // 1) status：监听地址 + 路由表 + 计数器都在
        let st = svc.call(method::EDGE_ROUTE_STATUS, Value::Null).unwrap();
        assert_eq!(st["routes"][0]["prefix"], "cad");
        assert_eq!(st["routes"][0]["port"], 2228);
        assert!(
            st["listens"][0].as_str().unwrap().starts_with("127.0.0.1:"),
            "{st}"
        );
        assert_eq!(st["counters"]["requests"], 0);

        // 2) list：只给路由数组（面板画表格用）
        let list = svc.call(method::EDGE_ROUTE_LIST, Value::Null).unwrap();
        assert_eq!(list.as_array().unwrap().len(), 1, "{list}");

        // 3) upsert：新增立即出现在 status 里
        let added = svc
            .call(
                method::EDGE_ROUTE_UPSERT,
                serde_json::json!({ "prefix": "panel", "port": 8080 }),
            )
            .unwrap();
        assert_eq!(added["prefix"], "panel");
        assert_eq!(added["port"], 8080);
        let st = svc.call(method::EDGE_ROUTE_STATUS, Value::Null).unwrap();
        assert_eq!(st["routes"].as_array().unwrap().len(), 2, "{st}");

        // 4) delete
        svc.call(method::EDGE_ROUTE_DELETE, serde_json::json!({ "prefix": "panel" }))
            .unwrap();
        let st = svc.call(method::EDGE_ROUTE_STATUS, Value::Null).unwrap();
        assert_eq!(st["routes"].as_array().unwrap().len(), 1, "{st}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn edge_control_service_maps_errors_to_contract_codes() {
        use esc_control_api::method;
        let (ctl, _ctx, _registry) = mount_edge_router().await;
        let svc = EdgeHostService::new(ctl);

        // 非法前缀 → invalid（面板按 ErrorKind::http_status 变 400）
        let (code, msg) = svc
            .call(
                method::EDGE_ROUTE_UPSERT,
                serde_json::json!({ "prefix": "a b", "port": 1 }),
            )
            .unwrap_err();
        assert_eq!(code, "invalid", "{msg}");

        // 删不存在的 → not_found（→ 404）
        let (code, msg) = svc
            .call(method::EDGE_ROUTE_DELETE, serde_json::json!({ "prefix": "nope" }))
            .unwrap_err();
        assert_eq!(code, "not_found", "{msg}");

        // 参数形状不对 → invalid，且错误里带方法名（便于定位是谁调错）
        let (code, msg) = svc
            .call(method::EDGE_ROUTE_DELETE, serde_json::json!({ "wrong": 1 }))
            .unwrap_err();
        assert_eq!(code, "invalid", "{msg}");
        assert!(msg.contains("edge.route.delete"), "错误应点名方法：{msg}");

        // 未知方法 → invalid（不是静默成功）
        let (code, _) = svc.call("edge.route.explode", Value::Null).unwrap_err();
        assert_eq!(code, "invalid");
    }

    #[test]
    fn edge_router_section_is_passed_through_verbatim() {
        let cfg = HostConfig::parse(
            r#"
[panel]
enabled = false

[edge_router]
enabled = true

[edge_router.config]
listen = ["0.0.0.0:2229"]
store = "/var/lib/esc/edge-routes.toml"

[edge_router.config.routes]
cad = 2228
panel = 8080
"#,
        )
        .unwrap();
        assert!(cfg.edge_router.enabled);
        let json = cfg.edge_router_json();
        assert_eq!(json["listen"][0], "0.0.0.0:2229");
        assert_eq!(json["routes"]["cad"], 2228);
        assert_eq!(json["routes"]["panel"], 8080);
        assert_eq!(json["store"], "/var/lib/esc/edge-routes.toml");
        // 透传的东西能被插件自己的 schema 解析（这里做一次端到端校验）。
        let parsed = esc_plugin_edge_router::RouterConfig::from_json(&json)
            .expect("宿主透传的形状必须能被插件解析");
        assert_eq!(parsed.listen, vec!["0.0.0.0:2229"]);
        assert_eq!(parsed.routes.get("cad"), Some(&2228));
        // 宿主不认识 store 的语义，但必须原样透传 —— 否则面板改了路由不会落盘。
        assert_eq!(
            parsed.store.as_deref(),
            Some(std::path::Path::new("/var/lib/esc/edge-routes.toml"))
        );
    }
}
