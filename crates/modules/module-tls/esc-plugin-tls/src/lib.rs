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

use std::sync::{Arc, Mutex};

use echo_context::Ctx;
use echo_plugin::{MountContext, Plugin, PluginManifest, PluginMountError, PluginMountResult};
use tracing::info;

pub use config::{Action, CertConfig, Defaults, Site, SiteConfig, TlsModuleConfig};
pub use control::{resolve, Metrics, TlsControlImpl, TLS_CONTROL};

/// 插件在注册表里的 id（与 crate 名对应：`esc-plugin-tls` ↔ `esc.tls`）。
pub const PLUGIN_ID: &str = "esc.tls";

/// 运行中的监听，放在插件里只为了 `unmount` 时能关掉。
struct Running {
    handle: serve::ServerHandle,
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
        // Ctx 在这里其实用不到（没有要注册的服务键），但宿主没给说明接线错了，
        // 早报比晚报好。
        if mctx.context().is_none() {
            return Err(PluginMountError::Mount("宿主没有提供 Ctx".into()));
        }
        let _: Option<&Arc<Ctx>> = mctx.context();

        // ── 配置：一次把所有问题都报出来 ────────────────────────────────────
        let cfg: TlsModuleConfig = serde_json::from_value(mctx.config().clone())
            .map_err(|e| PluginMountError::Mount(format!("TLS 配置无法解析: {e}")))?;

        let validated = cfg.validate().map_err(|errs| {
            // 逐条列出来。配置错通常是一批（复制粘贴改了一半），只报第一条会让
            // 人改一轮跑一轮。
            let mut msg = String::from("TLS 配置有误：");
            for e in errs {
                msg.push_str("\n  · ");
                msg.push_str(&e);
            }
            PluginMountError::Mount(msg)
        })?;

        // ── 证书：读盘失败即失败（fail-closed）──────────────────────────────
        //
        // 一个"大部分端口能用、某个域名握手失败"的部署，排查成本远高于启动时报错。
        let store = Arc::new(
            certs::CertStore::load(&cfg.cert, &validated.default_cert)
                .map_err(|e| PluginMountError::Mount(format!("{e:#}")))?,
        );
        let tls_config = certs::server_config(Arc::clone(&store), validated.tls12)
            .map_err(|e| PluginMountError::Mount(format!("TLS 配置构建失败: {e:#}")))?;

        // ── 绑定：同步做，失败即挂载失败 ────────────────────────────────────
        let bound = serve::bind(&validated.sites)
            .map_err(|e| PluginMountError::Mount(format!("TLS 终结无法监听: {e}")))?;

        let metrics = Metrics::new();
        let handle = serve::spawn(
            bound,
            tls_config,
            validated.handshake_timeout,
            Arc::clone(&metrics),
        )
        .map_err(|e| PluginMountError::Mount(format!("TLS 终结启动失败: {e}")))?;

        // 注册控制面。面板靠它把「:2222 ──TLS──▶ 18790 ──隧道──▶ 4090:8080」
        // 这条链路在一屏里画出来 —— 从前 nginx 那一层面板是看不见的。
        //
        // 注册失败要把刚起的监听关掉：挂载失败但端口还占着，下次重试会报
        // "Address already in use"，而真正的原因是服务键冲突。
        let ctl: Arc<dyn esc_control_api::TlsControl> =
            Arc::new(TlsControlImpl::new(&validated, &cfg.cert, Arc::clone(&metrics)));
        let ctx = mctx.context().expect("上面已确认存在");
        let disposer = match ctx.provide(&TLS_CONTROL, ctl) {
            Ok(d) => d,
            Err(e) => {
                handle.shutdown();
                return Err(PluginMountError::Mount(format!(
                    "无法注册 TLS 控制面服务: {e}"
                )));
            }
        };

        info!(
            sites = validated.sites.len(),
            certs = ?store.names(),
            sni = ?store.sni_names(),
            default_cert = ?validated.default_cert,
            tls12 = validated.tls12,
            addrs = ?handle.addrs(),
            "TLS 终结已挂载"
        );

        let mut guard = self
            .running
            .lock()
            .map_err(|_| PluginMountError::Mount("TLS 插件状态锁中毒".into()))?;
        // 重复挂载：先把旧的关掉，否则端口冲突（错误信息会指向"地址被占用"，
        // 误导性很强）。
        if let Some(old) = guard.take() {
            old.handle.shutdown();
        }
        *guard = Some(Running { handle });
        Ok(vec![disposer])
    }

    fn unmount(&self) -> Result<(), PluginMountError> {
        let mut guard = self
            .running
            .lock()
            .map_err(|_| PluginMountError::Mount("TLS 插件状态锁中毒".into()))?;
        if let Some(r) = guard.take() {
            r.handle.shutdown();
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
