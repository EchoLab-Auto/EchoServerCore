//! TLS 终结插件（**进程内**）：把 nginx 那一层的功能收进本项目。
//!
//! # 它替代了什么
//!
//! 原来 cloud2 上有一个 `esc-tls` 容器（`nginx:alpine` + 一份 8.5 KB 的
//! `nginx.conf`），做四件事：
//!
//! ```text
//! ① TLS 终结            自签 / ZeroSSL 两张证书，按 SNI 选
//! ② 按端口分流          2222→隧道  2224→网关  2223→301  2226→网关管理  443→面板
//! ③ 头部改写            Host 透传、X-Forwarded-For 覆盖、X-Forwarded-Proto: https
//! ④ WebSocket 透传      Upgrade / Connection 头
//! ```
//!
//! 现在这四件事都在这里。产物是**一个插件**，挂在 `esc-host` 里，不再有独立进程、
//! 不再有一份平行配置、不再需要 `nginx -s reload`。
//!
//! # 为什么放在进程内
//!
//! 与 `esc.tunnel` / `esc.edge-router` 同一个理由：它是**数据面**。每个进来的
//! HTTPS 请求都要过一次，放进子进程就多一次 IPC 和一份帧编解码，而且它的生命周期
//! 本来就该和宿主一致（宿主没了，端口留着也没意义）。
//!
//! 代价也说清楚：**它崩了会带走宿主**。换来的是不再有两套配置要对齐 —— 原来的
//! nginx 配置和 `gateway.toml` 就是两套，改一边忘了另一边不会有任何测试报错。
//!
//! # 与原 nginx 配置的三处有意差异
//!
//! 1. **证书按 SNI 全局选一张**，不按 server 块各配一份。细节见 `certs.rs`。
//!    net 效果是 2223/2226 用域名访问时也不再警告了，不是退步。
//! 2. **访问日志是结构化的**（`tracing` 字段），不是 `log_format` 文本行。
//!    见 `proxy.rs` 的 `log_access`。
//! 3. **没有 `client_max_body_size` 的"静默截断"**：超限直接 413，不缓冲到磁盘。
//!
//! # 明确不做
//!
//! * **HTTP/2 / HTTP/3**：ALPN 只宣告 `http/1.1`。与 `esc-gateway` 的取舍一致。
//! * **证书自动续期**：那还是 `certbot` 的事。这里只读盘上的 PEM，**重载靠重启**
//!   插件（或宿主）。续期后要生效，`systemctl reload` 宿主即可 —— 不做运行期
//!   热加载文件，是因为"证书什么时候换的"必须可追溯，而 watch 文件会把它变模糊。
//! * **gzip / 缓存 / 限流**：不在这一层。

pub mod certs;
pub mod config;
pub mod control;
pub mod proxy;
pub mod serve;
pub mod store;

use std::sync::{Arc, Mutex};

use echo_context::Ctx;
use echo_plugin::{MountContext, Plugin, PluginManifest, PluginMountError, PluginMountResult};
use tracing::info;

pub use config::{Action, CertConfig, Defaults, Site, SiteConfig, TlsModuleConfig};
pub use control::{resolve, Metrics, Runtime, State, TLS_CONTROL};
pub use store::ConfigStore;

/// 插件在注册表里的 id（与 crate 名对应：`esc-plugin-tls` ↔ `esc.tls`）。
pub const PLUGIN_ID: &str = "esc.tls";

/// 运行中的插件实例，放在这里只为了 `unmount` 时能关掉。
struct Running {
    rt: Arc<Runtime>,
}

pub struct TlsPlugin {
    manifest: PluginManifest,
    running: Mutex<Option<Running>>,
}

impl TlsPlugin {
    pub fn new() -> Self {
        Self {
            manifest: PluginManifest::builtin(
                PLUGIN_ID,
                "TLS 终结（SNI 选证书 → 本机端口）",
                env!("CARGO_PKG_VERSION"),
                "network",
                "tls",
                "按 SNI 选证书并终结 TLS，把明文请求转发到本机端口",
            ),
            running: Mutex::new(None),
        }
    }
}

impl Default for TlsPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for TlsPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn mount(&self, mctx: &MountContext) -> PluginMountResult {
        // Ctx 用来注册控制面服务。宿主没给说明接线错了，早报比晚报好。
        let ctx: Arc<Ctx> = Arc::clone(
            mctx.context()
                .ok_or_else(|| PluginMountError::Mount("宿主没有提供 Ctx".into()))?,
        );

        // ── 配置来源：状态文件优先，其次 host.toml 的 [tls.config] ──────────
        //
        // 「整体覆盖」而不是「合并」：面板删掉一个站点后，重启不该让它从
        // host.toml 里复活。想回到基线就删掉状态文件（见 store.rs）。
        let baseline: TlsModuleConfig = serde_json::from_value(mctx.config().clone())
            .map_err(|e| PluginMountError::Mount(format!("TLS 配置无法解析: {e}")))?;

        let store = baseline.store.clone().map(ConfigStore::new);
        let cfg = match &store {
            Some(s) => match s.load() {
                Ok(Some(saved)) => {
                    info!(path = %s.path().display(), "TLS 配置取自运行期状态文件");
                    saved
                }
                Ok(None) => baseline,
                // 状态文件坏了就**不启动**：静默回落基线会让面板显示的东西和
                // 实际跑的东西分家，那比一次明确的启动失败难查得多。
                Err(e) => return Err(PluginMountError::Mount(e)),
            },
            None => baseline,
        };

        // ── 起运行期：校验 → 读证书 → 绑端口 ────────────────────────────────
        //
        // 绑端口在这里同步做，失败即挂载失败（fail-closed）。等「挂载成功但每个
        // 连接都失败」再排查，那已经是一次线上事故了。
        let metrics = Metrics::new();
        let rt = Runtime::start(cfg, Arc::clone(&metrics), store)
            .map_err(PluginMountError::Mount)?;

        // 注册控制面。面板靠它读状态、也靠它保存改动。
        let rt_arc: Arc<Runtime> = Arc::clone(&rt);
        let ctl: Arc<dyn esc_control_api::TlsControl> = rt_arc;
        let disposer = match ctx.provide(&TLS_CONTROL, ctl) {
            Ok(d) => d,
            Err(e) => {
                // 注册失败要把刚起的监听关掉：挂载失败但端口还占着，下次重试会
                // 报 "Address already in use"，而真正的原因是服务键冲突。
                rt.shutdown();
                return Err(PluginMountError::Mount(format!(
                    "无法注册 TLS 控制面服务: {e}"
                )));
            }
        };

        let mut guard = self
            .running
            .lock()
            .map_err(|_| PluginMountError::Mount("TLS 插件状态锁中毒".into()))?;
        // 重复挂载：先把旧的关掉，否则端口冲突（错误信息会指向"地址被占用"，
        // 误导性很强）。
        if let Some(old) = guard.take() {
            old.rt.shutdown();
        }
        *guard = Some(Running { rt });
        Ok(vec![disposer])
    }

    fn unmount(&self) -> Result<(), PluginMountError> {
        let mut guard = self
            .running
            .lock()
            .map_err(|_| PluginMountError::Mount("TLS 插件状态锁中毒".into()))?;
        if let Some(r) = guard.take() {
            r.rt.shutdown();
            info!("TLS 终结已停止");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_is_stable() {
        let p = TlsPlugin::new();
        let m = p.manifest();
        assert_eq!(m.id, "esc.tls");
        // `entry` 就是 `host.toml` 里的配置段名（`[tls]`）。改它等于改配置文件的键，
        // 所以钉住。
        assert_eq!(m.entry, "tls");
        assert_eq!(m.kind, "network");
    }
}
