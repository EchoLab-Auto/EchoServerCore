//! 边缘路由插件（**进程内**）：把「域名前缀 → 本机端口」的分流做成一个服务。
//!
//! ```text
//!   浏览器 ── Host: cad.example.com ──▶ esc-host:2229（本插件）
//!                                        │  取 Host 第一段 → 查路由表
//!                                        ▼
//!                              127.0.0.1:2228（echo-cad-toolbox）
//! ```
//!
//! # 为什么放在进程内
//!
//! 它是**数据面**：每次重启都会中断在途请求。和隧道插件同理，交给宿主的
//! 生命周期管理，不引入进程边界和帧编解码延迟。它也不需要子进程那种
//! "单独升级"能力 —— 路由表可以在运行期改（[`EdgeRouterControl`]），
//! 改配置才需要重启。
//!
//! # 它与 esc-gateway 的分工（别搞混）
//!
//! | | `esc-gateway` | 本插件 |
//! | --- | --- | --- |
//! | 做什么 | 把**远端**节点的端口映射到本机（经隧道） | 把本机**不同端口**按域名分开 |
//! | 目标地址 | 来自隧道注册表（另一台机器） | 恒为 `127.0.0.1`（本机） |
//! | 面向 | 穿透/内网暴露 | 单机上多服务的统一入口 |
//!
//! 两者可以同时跑：`cad.example.com → 2228` 由本插件分流，而 `2228` 上的
//! 服务本身可以是 `esc-gateway` 暴露出来的远端端口。
//!
//! # mount 时的取舍：同步 bind
//!
//! `bind` 在 `mount()` 里同步完成。这样端口占用、没有特权端口权限这类错误
//! 会在**挂载阶段**就让宿主看到（fail-closed），而不是"挂载成功但永远 421"。
//! bind 是微秒级操作，阻塞代价可忽略；真正长跑的部分（accept + 转发）在
//! 后台任务里。

use std::sync::{Arc, Mutex};

use echo_context::{Ctx, ServiceKey};
use echo_plugin::{MountContext, Plugin, PluginManifest, PluginMountError, PluginMountResult};
use tracing::{info, warn};

use esc_control_api::ControlError;

pub mod config;
pub mod control;
pub mod router;
pub mod store;

pub use config::RouterConfig;
pub use control::Counters;
pub use store::RouteStore;
pub use router::{bind, first_label, spawn, EdgeRouter, Resolution, ServerHandle};

// 契约类型从 `esc-control-api` 重新导出：调用方只 `use esc_plugin_edge_router::…`
// 就能拿到完整的一套，不必再记住"哪些定义在哪个 crate"。
pub use esc_control_api::{
    EdgeRoute, EdgeRouteControl, EdgeRouteUpsert, EdgeRouterCounters, EdgeRouterStatus,
};

/// Ctx 里的服务键。宿主按这个键取控制面。
///
/// 值是 `esc-control-api` 的 [`EdgeRouteControl`]（不是插件私有 trait）——
/// 这样宿主、面板代理、测试都用同一份契约。
pub static EDGE_ROUTER_CONTROL: ServiceKey<Arc<dyn EdgeRouteControl>> =
    ServiceKey::new("esc.edge-router.control");

/// 插件在注册表里的 id（与 crate 名对应：`esc-plugin-edge-router` ↔ `esc.edge-router`）。
pub const PLUGIN_ID: &str = "esc.edge-router";

/// 正在运行的监听，放在插件里只为了 `unmount` 时能关掉。
struct Running {
    handle: ServerHandle,
}

pub struct EdgeRouterPlugin {
    manifest: PluginManifest,
    running: Mutex<Option<Running>>,
}

impl EdgeRouterPlugin {
    pub fn new() -> Self {
        Self {
            manifest: PluginManifest::builtin(
                PLUGIN_ID,
                "边缘路由（域名前缀 → 本机端口）",
                env!("CARGO_PKG_VERSION"),
                "network",
                "edge_router",
                "按 Host 的域名前缀把请求分流到本机不同端口",
            ),
            running: Mutex::new(None),
        }
    }
}

impl Default for EdgeRouterPlugin {
    fn default() -> Self {
        Self::new()
    }
}
impl Plugin for EdgeRouterPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn mount(&self, mctx: &MountContext) -> PluginMountResult {
        let ctx: Arc<Ctx> = mctx
            .context()
            .cloned()
            .ok_or_else(|| PluginMountError::Mount("宿主没有提供 Ctx".into()))?;

        // 配置错就挂载失败（fail-closed）。一个"起来了但全 421"的边缘路由
        // 比没起来更难查。
        let cfg = RouterConfig::from_json(mctx.config())
            .map_err(|e| PluginMountError::Mount(format!("edge router 配置无效: {}", e.message)))?;

        // 同步 bind（见文件头「mount 时的取舍」）。
        let listeners = router::bind(&cfg)
            .map_err(|e| PluginMountError::Mount(format!("edge router 无法监听: {}", e.message)))?;
        let addrs: Vec<_> = listeners
            .iter()
            .filter_map(|l| l.local_addr().ok())
            .map(|a| a.to_string())
            .collect();

        // `load` 会把状态文件叠加到基线路由之上；读失败 fail-closed ——
        // 内容坏了不能装作没看见，否则用户会以为改动还在，直到某次重启后消失。
        let binds: Vec<std::net::SocketAddr> =
            listeners.iter().filter_map(|l| l.local_addr().ok()).collect();
        let from_store = cfg
            .store
            .as_ref()
            .map(|p| RouteStore::new(p).load().ok().flatten().is_some())
            .unwrap_or(false);
        let edge = EdgeRouter::load(&cfg, binds)
            .map_err(|e| PluginMountError::Mount(format!("边缘路由无法加载路由表: {}", e.message)))?;
        let handle = router::spawn(Arc::clone(&edge), listeners)
            .map_err(|e| PluginMountError::Mount(format!("edge router 启动失败: {}", e.message)))?;

        // 注册控制面。失败要把刚起的监听关掉，否则端口泄漏（挂载失败但
        // 端口还占着，下次重试会报 "Address already in use"）。
        // `EdgeRouter` 自己就实现契约，不需要适配层。
        let ctl: Arc<dyn EdgeRouteControl> = Arc::clone(&edge) as Arc<dyn EdgeRouteControl>;
        let disposer = match ctx.provide(&EDGE_ROUTER_CONTROL, ctl) {
            Ok(d) => d,
            Err(e) => {
                let rt = tokio::runtime::Handle::try_current();
                if let Ok(rt) = rt {
                    let h = handle;
                    rt.spawn(async move { h.shutdown().await });
                }
                return Err(PluginMountError::Mount(format!(
                    "无法注册边缘路由服务: {e}"
                )));
            }
        };

        let routes = edge.routes();
        info!(
            plugin = %mctx.plugin_id(),
            listen = %addrs.join(","),
            routes = routes.len(),
            default_port = ?cfg.default_port,
            store = cfg.store.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "（无，改动不落盘）".into()),
            from_store,
            "edge router mounted"
        );
        if from_store {
            info!("路由表来自状态文件（覆盖了 host.toml 里的基线）");
        }
        for r in &routes {
            info!(prefix = %r.prefix, port = r.port, "  edge route");
        }
        if routes.is_empty() {
            warn!("edge router 路由表为空：所有请求都会 421（除非配了 default_port）");
        }

        if let Ok(mut slot) = self.running.lock() {
            *slot = Some(Running { handle });
        }
        Ok(vec![disposer])
    }

    fn unmount(&self) -> Result<(), PluginMountError> {
        let running = self.running.lock().ok().and_then(|mut g| g.take());
        if let Some(Running { handle }) = running {
            // `unmount` 是同步的，而优雅关闭要 await：交给运行时后台做完。
            // 即使宿主进程马上就要退出，tcp 监听也会随进程回收。
            match tokio::runtime::Handle::try_current() {
                Ok(rt) => {
                    rt.spawn(async move {
                        handle.shutdown().await;
                        info!("edge router 已由 unmount 关闭");
                    });
                }
                Err(_) => warn!("unmount 时没有 tokio 运行时，edge router 监听未优雅关闭"),
            }
        }
        Ok(())
    }
}

/// 把 Ctx 里的边缘路由服务取出来（宿主在 mount 之后用）。
pub fn resolve(ctx: &Ctx) -> Result<Arc<dyn EdgeRouteControl>, ControlError> {
    ctx.require(&EDGE_ROUTER_CONTROL)
        .map_err(|e| ControlError::unavailable(format!("边缘路由服务未注册: {e}")))
}
